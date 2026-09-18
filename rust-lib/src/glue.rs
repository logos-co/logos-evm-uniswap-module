//! Logos module glue for `uniswap_module`: the wallet's price oracle and swap quoter/encoder,
//! reached through `eth_rpc_module` alone. It holds no key and sends nothing; a consumer
//! hands the calls it builds to `tx_sender_module`. Compiled only with the `logos_module`
//! feature; the pure cores are tested with `cargo test --no-default-features`.
//!
//! `concurrency: "multi"`: every method blocks on a Multicall3 `eth_call`, so dispatch is
//! concurrent. The config map lives behind a `RwLock` — read it, clone the chain, drop the
//! lock, then call. `configure` is the only writer.

use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, U256};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{ChainUniswap, ConfigStore, STABLE_DECIMALS};
use crate::pricing::{self, parse_addr as parse_addr_opt, Version};
use crate::reply::err;
use crate::swap::{self, Approval, CallKind, Quoted};

pub trait UniswapModule: Send + Sync + 'static {
    /// Add or override a chain's Uniswap config (JSON of `ChainUniswap`).
    fn configure(&self, chain_json: String) -> bool;
    /// All configured chains (defaults + overrides).
    fn get_chains(&self) -> String;
    /// Token→ETH and token→USD prices for `{ "tokens": [{address, decimals}] }`,
    /// best-rate across V2/V3/V4, batched into one Multicall3 `eth_call`.
    fn get_prices(&self, chain_id: i64, tokens_json: String) -> String;
    /// Best swap quote for `{ tokenIn, tokenOut, amountIn, owner? }` (native = "ETH"):
    /// the route, its output, the price impact, a gas hint, and — with `owner` — the
    /// account's balance and whether an approval must go first.
    fn quote_swap(&self, chain_id: i64, params_json: String) -> String;
    /// The quote plus the calls that make the swap, in order, in the shape
    /// `tx_sender_module` takes: `{ calls: [{ kind, to, value, data, gasLimitHint, label }] }`.
    fn build_swap(&self, chain_id: i64, params_json: String) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct UniswapModuleImpl {
    cfg: RwLock<Option<ConfigStore>>,
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// One Multicall3 read through eth_rpc. A quote batch is tens of quoter calls, which a
/// public node answers in a second or two; the verified proxy adds a proof round trip.
const RPC_BUDGET: Duration = Duration::from_millis(15_000);

/// The deadline handed to eth_rpc: the budget less the margin the transport itself needs.
fn callee_deadline(t: Duration) -> Option<i64> {
    t.checked_sub(Duration::from_millis(300)).map(|d| d.as_millis() as i64)
}

/// Native ETH is "ETH"/""/`0x0…0`; everything else is a 20-byte address.
fn parse_token(s: &str) -> Result<Address, String> {
    let t = s.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("eth") || t.eq_ignore_ascii_case("native") {
        return Ok(Address::ZERO);
    }
    parse_addr_opt(t).ok_or_else(|| format!("invalid address: {s}"))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Deserialize)]
struct TokenIn {
    address: String,
    #[serde(default = "default_decimals")]
    decimals: u8,
}
fn default_decimals() -> u8 {
    18
}

#[derive(Deserialize)]
struct PricesReq {
    #[serde(default)]
    tokens: Vec<TokenIn>,
}

/// A swap request. `owner` is the account that pays and, unless `recipient` says otherwise,
/// receives; `symbolIn`/`symbolOut` only name the calls the sender records.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapReq {
    token_in: String,
    token_out: String,
    amount_in: String,
    #[serde(default)]
    owner: String,
    #[serde(default)]
    recipient: String,
    #[serde(default)]
    amount_out_min: String,
    #[serde(default)]
    deadline: u64,
    #[serde(default = "default_slippage_bps")]
    slippage_bps: u64,
    #[serde(default)]
    symbol_in: String,
    #[serde(default)]
    symbol_out: String,
}
fn default_slippage_bps() -> u64 {
    50 // 0.5%
}

const MAX_SLIPPAGE_BPS: u64 = 5_000;

/// The parsed half of a request, shared by the two swap methods.
struct Parsed {
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    owner: Option<Address>,
}

fn parse_swap(p: &SwapReq) -> Result<Parsed, String> {
    let token_in = parse_token(&p.token_in)?;
    let token_out = parse_token(&p.token_out)?;
    let amount_in = swap::parse_amount(&p.amount_in)?;
    let owner = if p.owner.trim().is_empty() {
        None
    } else {
        Some(parse_addr_opt(p.owner.trim()).ok_or_else(|| format!("invalid owner: {}", p.owner))?)
    };
    if p.slippage_bps > MAX_SLIPPAGE_BPS {
        return Err(format!("slippage above {MAX_SLIPPAGE_BPS} bps is refused"));
    }
    Ok(Parsed { token_in, token_out, amount_in, owner })
}

/// Everything one quote round trip learned.
struct QuoteOutcome {
    chain: ChainUniswap,
    best: Quoted,
    impact_bps: Option<u32>,
    owner: swap::OwnerState,
    /// The router the winning route swaps on.
    spender: Address,
    /// eth_rpc's own label for how the read was served (`direct`, `verified`, …).
    rpc_route: Option<String>,
}

fn short(a: Address) -> String {
    let s = format!("{a}");
    format!("{}…{}", &s[..6], &s[s.len() - 4..])
}

fn name_of(sym: &str, token: Address) -> String {
    if !sym.trim().is_empty() {
        sym.trim().to_string()
    } else if swap::is_native(token) {
        "ETH".to_string()
    } else {
        short(token)
    }
}

fn route_json(q: &Quoted) -> Value {
    let hops: Vec<Value> = q
        .route
        .tokens
        .windows(2)
        .enumerate()
        .map(|(i, w)| {
            let mut h = json!({ "tokenIn": format!("{}", w[0]), "tokenOut": format!("{}", w[1]) });
            if let Some(fee) = q.route.fees.get(i) {
                h["fee"] = json!(fee);
            }
            h
        })
        .collect();
    json!({
        "version": format!("{:?}", q.route.version),
        "hops": hops,
        "viaWeth": q.route.via_weth(),
    })
}

impl UniswapModuleImpl {
    /// Read the config under a shared lock (concurrent readers overlap). Clone out
    /// what you need and let the guard drop before any blocking eth_rpc call.
    fn with_cfg<R>(&self, f: impl FnOnce(&ConfigStore) -> R) -> Result<R, String> {
        match self.cfg.read().unwrap().as_ref() {
            Some(c) => Ok(f(c)),
            None => Err("uniswap not initialized (context not ready)".to_string()),
        }
    }

    /// Write the config (the only mutator is `configure`).
    fn with_cfg_mut(&self, f: impl FnOnce(&mut ConfigStore) -> bool) -> bool {
        match self.cfg.write().unwrap().as_mut() {
            Some(c) => f(c),
            None => false,
        }
    }

    /// Look up + clone a chain's config under the read lock.
    fn chain_cfg(&self, chain_id: i64) -> Result<ChainUniswap, String> {
        match self.with_cfg(|c| c.chain(chain_id as u64).cloned()) {
            Ok(Some(ch)) => Ok(ch),
            Ok(None) => Err(format!("no uniswap config for chain {chain_id}")),
            Err(e) => Err(e),
        }
    }

    /// Issue `aggregate3(calls)` through eth_rpc and return per-call results plus the
    /// route eth_rpc served it by. Touches no module state, so no lock is held across
    /// the blocking call. A refusal is eth_rpc's own reply, returned whole for `err` to relay.
    fn run_multicall(
        &self,
        chain_id: i64,
        multicall3: &str,
        calls: &[(Address, Vec<u8>)],
    ) -> Result<(Vec<Option<Vec<u8>>>, Option<String>), String> {
        if calls.is_empty() {
            return Ok((Vec::new(), None));
        }
        let data = pricing::multicall3_aggregate3_calldata(calls);
        let call_json = json!({ "to": multicall3, "data": format!("0x{}", hex::encode(data)) }).to_string();
        let resp = modules()
            .eth_rpc_module
            .call_with_timeout(chain_id, &call_json, callee_deadline(RPC_BUDGET), RPC_BUDGET)
            .map_err(|e| format!("{e:?}"))?;
        let v: Value = serde_json::from_str(&resp).map_err(|e| e.to_string())?;
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            return Err(resp);
        }
        let result_hex = v.get("result").and_then(Value::as_str).ok_or("multicall: no result")?;
        let bytes = hex::decode(result_hex.trim_start_matches("0x")).map_err(|e| e.to_string())?;
        let route = v.get("route").and_then(Value::as_str).map(str::to_string);
        let results = pricing::decode_aggregate3_returns(&bytes).ok_or_else(|| "multicall: decode failed".to_string())?;
        Ok((results, route))
    }

    /// One round trip: every candidate route quoted for the amount and for its probe, plus
    /// the owner's balance and allowances when an owner was named.
    fn quote(&self, chain_id: i64, p: &Parsed) -> Result<QuoteOutcome, String> {
        let chain = self.chain_cfg(chain_id)?;
        let mut batch = swap::build_quote_batch(&chain, p.token_in, p.token_out, p.amount_in, p.owner);
        if batch.calls.is_empty() {
            return Err("no route: the pair has no pool this module can quote".to_string());
        }
        if let Some(owner) = p.owner {
            for v in [Version::V3, Version::V2] {
                if let Some(router) = swap::router_for(&chain, v) {
                    batch = swap::with_allowance_read(batch, p.token_in, owner, router);
                }
            }
        }
        let (results, rpc_route) = self.run_multicall(chain_id, &chain.multicall3, &batch.calls)?;
        let (quotes, owner) = swap::decode_quotes(&batch, &results);
        let best = swap::pick_best(&quotes).cloned().ok_or_else(|| "no route found".to_string())?;
        let spender = swap::router_for(&chain, best.route.version).ok_or("no router for the winning route")?;
        let impact_bps = best
            .probe_out
            .and_then(|po| swap::price_impact_bps(p.amount_in, best.amount_out, batch.probe_in(), po));
        Ok(QuoteOutcome { chain, best, impact_bps, owner, spender, rpc_route })
    }

    fn quote_json(&self, chain_id: i64, req: &SwapReq, p: &Parsed, o: &QuoteOutcome) -> Value {
        let native_in = swap::is_native(p.token_in);
        let approval = p.owner.map(|_| swap::approval_needed(native_in, o.owner.allowance_for(o.spender), p.amount_in));
        let mut v = json!({
            "ok": true,
            "chainId": chain_id,
            "tokenIn": req.token_in,
            "tokenOut": req.token_out,
            "amountIn": p.amount_in.to_string(),
            "amountOut": o.best.amount_out.to_string(),
            "route": route_json(&o.best),
            "feeBps": o.best.route.fee_bps(),
            "priceImpactBps": o.impact_bps,
            "gasLimitHint": swap::gas_hint(&o.best, native_in, swap::is_native(p.token_out)),
            "spender": format!("{}", o.spender),
            "balanceIn": o.owner.balance_in.map(|b| b.to_string()),
            "allowance": o.owner.allowance_for(o.spender).map(|a| a.to_string()),
            "needsApproval": approval.map(|a| a != Approval::None),
            "approval": approval.map(|a| match a {
                Approval::None => "none",
                Approval::Set(_) => "set",
                Approval::ResetThenSet(_) => "resetThenSet",
            }),
        });
        if let Some(r) = &o.rpc_route {
            v["rpcRoute"] = json!(r);
        }
        v
    }
}

impl UniswapModule for UniswapModuleImpl {
    fn on_context_ready(&self, ctx: &RustModuleContext) {
        let dir = std::path::PathBuf::from(&ctx.instance_persistence_path);
        *self.cfg.write().unwrap() = Some(ConfigStore::with_path(dir.join("config.json")));
    }

    fn configure(&self, chain_json: String) -> bool {
        let chain: ChainUniswap = match serde_json::from_str(&chain_json) {
            Ok(c) => c,
            Err(_) => return false,
        };
        self.with_cfg_mut(|cfg| {
            cfg.set_chain(chain);
            true
        })
    }

    fn get_chains(&self) -> String {
        match self.with_cfg(|c| json!({ "ok": true, "chains": c.all() }).to_string()) {
            Ok(s) => s,
            Err(e) => err(e),
        }
    }

    fn get_prices(&self, chain_id: i64, tokens_json: String) -> String {
        let req: PricesReq = match serde_json::from_str(&tokens_json) {
            Ok(r) => r,
            Err(e) => return err(e),
        };

        // Resolve chain config, WETH, stablecoins (priced too, to anchor USD).
        let (mc, weth, stable_addrs, batch) = {
            let chain = match self.chain_cfg(chain_id) {
                Ok(c) => c,
                Err(e) => return err(e),
            };
            let weth = match parse_addr_opt(&chain.weth) {
                Some(w) => w,
                None => return err("invalid WETH address in config"),
            };
            let stable_addrs: Vec<Address> = chain.stablecoins.iter().filter_map(|s| parse_addr_opt(s)).collect();

            // Price the user's tokens plus the stablecoins (USD anchor).
            let mut priced: Vec<(Address, u8)> = Vec::new();
            for t in &req.tokens {
                if let Some(a) = parse_addr_opt(&t.address) {
                    priced.push((a, t.decimals));
                }
            }
            for s in &stable_addrs {
                if !priced.iter().any(|(a, _)| a == s) {
                    priced.push((*s, STABLE_DECIMALS));
                }
            }
            (chain.multicall3.clone(), weth, stable_addrs, pricing::build_pricing_batch(&chain, weth, &priced))
        };

        let (results, _) = match self.run_multicall(chain_id, &mc, &batch.calls) {
            Ok(r) => r,
            Err(e) => return err(e),
        };
        let eth_prices = pricing::decode_prices(&batch, &results);
        let usd_prices = pricing::token_usd_prices(&eth_prices, weth, &stable_addrs);

        // Report the user's tokens (+ native ETH) with both prices.
        let mut out = Vec::new();
        out.push(json!({
            "address": "ETH",
            "eth": 1.0,
            "usd": usd_prices.get(&weth).copied(),
        }));
        for t in &req.tokens {
            if let Some(a) = parse_addr_opt(&t.address) {
                out.push(json!({
                    "address": t.address,
                    "eth": eth_prices.get(&a).copied(),
                    "usd": usd_prices.get(&a).copied(),
                }));
            }
        }
        json!({ "ok": true, "chainId": chain_id, "prices": out }).to_string()
    }

    fn quote_swap(&self, chain_id: i64, params_json: String) -> String {
        let req: SwapReq = match serde_json::from_str(&params_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let p = match parse_swap(&req) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        match self.quote(chain_id, &p) {
            Ok(o) => self.quote_json(chain_id, &req, &p, &o).to_string(),
            Err(e) => err(e),
        }
    }

    fn build_swap(&self, chain_id: i64, params_json: String) -> String {
        let req: SwapReq = match serde_json::from_str(&params_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let p = match parse_swap(&req) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        // The payer is the owner; the recipient defaults to the payer. Without an owner
        // there is no allowance to read and no account to build the swap for.
        let Some(owner) = p.owner else {
            return err("owner required");
        };
        let recipient = if req.recipient.trim().is_empty() {
            owner
        } else {
            match parse_addr_opt(req.recipient.trim()) {
                Some(r) => r,
                None => return err(format!("invalid recipient: {}", req.recipient)),
            }
        };

        let o = match self.quote(chain_id, &p) {
            Ok(o) => o,
            Err(e) => return err(e),
        };

        // amountOutMin: explicit if given, else quote minus slippage.
        let amount_out_min = if req.amount_out_min.trim().is_empty() {
            let bps = U256::from(10_000u64 - req.slippage_bps);
            o.best.amount_out.saturating_mul(bps) / U256::from(10_000u64)
        } else {
            match swap::parse_amount(&req.amount_out_min) {
                Ok(m) => m,
                Err(e) => return err(format!("amountOutMin: {e}")),
            }
        };
        let deadline = if req.deadline > 0 { req.deadline } else { now_secs() + 1800 };

        let native_in = swap::is_native(p.token_in);
        let approval = swap::approval_needed(native_in, o.owner.allowance_for(o.spender), p.amount_in);
        let names = swap::Names {
            token_in: &name_of(&req.symbol_in, p.token_in),
            token_out: &name_of(&req.symbol_out, p.token_out),
        };
        let built = swap::build_swap(
            &o.chain,
            &o.best,
            p.token_in,
            p.token_out,
            p.amount_in,
            amount_out_min,
            recipient,
            U256::from(deadline),
            approval,
            names,
        );
        let Some(built) = built else {
            return err("could not build the swap for the best route");
        };
        let calls: Vec<Value> = built
            .calls
            .iter()
            .map(|c| {
                json!({
                    "kind": match c.kind { CallKind::Approve => "approve", CallKind::Swap => "swap" },
                    "to": format!("{}", c.to),
                    "value": format!("0x{:x}", c.value),
                    "data": format!("0x{}", hex::encode(&c.data)),
                    "gasLimitHint": c.gas_limit_hint,
                    "label": c.label,
                })
            })
            .collect();
        let mut v = self.quote_json(chain_id, &req, &p, &o);
        v["owner"] = json!(format!("{owner}"));
        v["recipient"] = json!(format!("{recipient}"));
        v["amountOutMin"] = json!(amount_out_min.to_string());
        v["slippageBps"] = json!(req.slippage_bps);
        v["deadline"] = json!(deadline);
        v["calls"] = json!(calls);
        v.to_string()
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<UniswapModuleImpl>();
}
