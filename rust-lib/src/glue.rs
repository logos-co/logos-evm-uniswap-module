//! Logos module glue for `uniswap_module` (rust-first authoring).
//!
//! Depends on `eth_rpc_module` (declared in metadata.json `dependencies`),
//! reached as `modules().eth_rpc_module.call(chainId, callJson)`. This module is
//! the wallet's **price oracle and swap router**: it derives pool addresses
//! offline, bundles every read into one Multicall3 `eth_call`, and returns
//! token→ETH / token→USD prices and best-rate swap quotes/transactions.
//!
//! Compiled only with the default `logos_module` feature; the pure cores
//! (`config`, `pricing`, `swap`) are tested with `cargo test --no-default-features`.
//!
//! `concurrency: "multi"` (metadata.json): every price/quote/swap method blocks on
//! a Multicall3 `eth_call` through eth_rpc, so the module opts into concurrent
//! dispatch — pricing several chains at once no longer serializes. The multi
//! contract makes the generated trait take `&self` + `Send + Sync`; the config map
//! lives behind a `RwLock` (read it, clone the chain, drop the lock, then call —
//! `configure` is the only writer).

use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, U256};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{ChainUniswap, ConfigStore, STABLE_DECIMALS};
use crate::pricing::{self, parse_addr as parse_addr_opt};
use crate::swap;

pub trait UniswapModule: Send + Sync + 'static {
    /// Add or override a chain's Uniswap config (JSON of `ChainUniswap`).
    fn configure(&self, chain_json: String) -> bool;
    /// All configured chains (defaults + overrides).
    fn get_chains(&self) -> String;
    /// Token→ETH and token→USD prices for `{ "tokens": [{address, decimals}] }`,
    /// best-rate across V2/V3/V4, batched into one Multicall3 `eth_call`.
    fn get_prices(&self, chain_id: i64, tokens_json: String) -> String;
    /// Best swap quote for `{ tokenIn, tokenOut, amountIn }` (native = "ETH").
    fn quote_swap(&self, chain_id: i64, params_json: String) -> String;
    /// Unsigned swap tx (router, value, data, +approval) for the best route.
    fn build_swap(&self, chain_id: i64, params_json: String) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct UniswapModuleImpl {
    cfg: RwLock<Option<ConfigStore>>,
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

/// Native ETH is "ETH"/""/`0x0…0`; everything else is a 20-byte address.
fn parse_token(s: &str) -> Result<Address, String> {
    let t = s.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("eth") || t.eq_ignore_ascii_case("native") {
        return Ok(Address::ZERO);
    }
    parse_addr_opt(t).ok_or_else(|| format!("invalid address: {s}"))
}

fn parse_u256(s: &str) -> U256 {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        U256::from_str_radix(h, 16).unwrap_or(U256::ZERO)
    } else {
        t.parse().unwrap_or(U256::ZERO)
    }
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapReq {
    token_in: String,
    token_out: String,
    amount_in: String,
    #[serde(default)]
    amount_out_min: String,
    #[serde(default)]
    recipient: String,
    #[serde(default)]
    deadline: u64,
    #[serde(default = "default_slippage_bps")]
    slippage_bps: u64,
}
fn default_slippage_bps() -> u64 {
    50 // 0.5%
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

    /// Issue `aggregate3(calls)` through eth_rpc and return per-call results.
    /// Touches no module state, so no lock is held across the blocking call.
    fn run_multicall(&self, chain_id: i64, multicall3: &str, calls: &[(Address, Vec<u8>)]) -> Result<Vec<Option<Vec<u8>>>, String> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let data = pricing::multicall3_aggregate3_calldata(calls);
        let call_json = json!({ "to": multicall3, "data": format!("0x{}", hex::encode(data)) }).to_string();
        let resp = modules().eth_rpc_module.call(chain_id, &call_json).map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&resp).map_err(|e| e.to_string())?;
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            return Err(v.get("error").and_then(Value::as_str).unwrap_or("eth_call failed").to_string());
        }
        let result_hex = v.get("result").and_then(Value::as_str).ok_or("multicall: no result")?;
        let bytes = hex::decode(result_hex.trim_start_matches("0x")).map_err(|e| e.to_string())?;
        pricing::decode_aggregate3_returns(&bytes).ok_or_else(|| "multicall: decode failed".to_string())
    }

    /// Quote `amount_in` of `token_in → token_out` across V2 + V3 fee tiers.
    fn quote(&self, chain_id: i64, token_in: Address, token_out: Address, amount_in: U256) -> Result<swap::BestQuote, String> {
        let chain = self.chain_cfg(chain_id)?;
        let batch = swap::build_quote_batch(&chain, token_in, token_out, amount_in);
        let results = self.run_multicall(chain_id, &chain.multicall3, &batch.calls)?;
        swap::decode_best_quote(&batch, &results).ok_or_else(|| "no route found".to_string())
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

        let results = match self.run_multicall(chain_id, &mc, &batch.calls) {
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
        let p: SwapReq = match serde_json::from_str(&params_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let (token_in, token_out) = match (parse_token(&p.token_in), parse_token(&p.token_out)) {
            (Ok(i), Ok(o)) => (i, o),
            (Err(e), _) | (_, Err(e)) => return err(e),
        };
        match self.quote(chain_id, token_in, token_out, parse_u256(&p.amount_in)) {
            Ok(q) => json!({
                "ok": true,
                "version": format!("{:?}", q.version),
                "fee": q.fee,
                "amountOut": q.amount_out.to_string(),
            })
            .to_string(),
            Err(e) => err(e),
        }
    }

    fn build_swap(&self, chain_id: i64, params_json: String) -> String {
        let p: SwapReq = match serde_json::from_str(&params_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let (token_in, token_out) = match (parse_token(&p.token_in), parse_token(&p.token_out)) {
            (Ok(i), Ok(o)) => (i, o),
            (Err(e), _) | (_, Err(e)) => return err(e),
        };
        let amount_in = parse_u256(&p.amount_in);
        let recipient = match parse_addr_opt(&p.recipient) {
            Some(r) => r,
            None => return err("recipient required"),
        };

        let quote = match self.quote(chain_id, token_in, token_out, amount_in) {
            Ok(q) => q,
            Err(e) => return err(e),
        };

        // amountOutMin: explicit if given, else quote minus slippage.
        let amount_out_min = if p.amount_out_min.is_empty() {
            let bps = U256::from(10_000u64.saturating_sub(p.slippage_bps));
            quote.amount_out.saturating_mul(bps) / U256::from(10_000u64)
        } else {
            parse_u256(&p.amount_out_min)
        };
        let deadline = if p.deadline > 0 { p.deadline } else { now_secs() + 1200 };

        let built = {
            let chain = match self.chain_cfg(chain_id) {
                Ok(c) => c,
                Err(e) => return err(e),
            };
            swap::build_swap(&chain, &quote, token_in, token_out, amount_in, amount_out_min, recipient, U256::from(deadline))
        };
        match built {
            Some(b) => {
                let approve = b.approve.map(|(token, spender, data)| {
                    json!({ "token": format!("{token}"), "spender": format!("{spender}"), "data": format!("0x{}", hex::encode(data)) })
                });
                json!({
                    "ok": true,
                    "version": format!("{:?}", quote.version),
                    "fee": quote.fee,
                    "router": format!("{}", b.router),
                    "value": format!("0x{:x}", b.value),
                    "data": format!("0x{}", hex::encode(b.data)),
                    "amountOut": quote.amount_out.to_string(),
                    "amountOutMin": amount_out_min.to_string(),
                    "approve": approve,
                })
                .to_string()
            }
            None => err("could not build swap for the best route (V4 swaps are a fast-follow)"),
        }
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<UniswapModuleImpl>();
}
