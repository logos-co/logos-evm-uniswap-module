//! Uniswap quoting and swap encoding: pure and offline, like `pricing`.
//!
//! A quote is one Multicall3 batch: every candidate route (V2 direct and via WETH, V3 direct
//! per fee tier and via WETH per tier pair) is asked for the amount and for a PROBE of a
//! thousandth of it, whose rate stands in for the marginal price; the same batch reads the
//! owner's balance and allowances. V2 swaps go to the legacy router; V3 swaps go to
//! SwapRouter02 wrapped in `multicall(deadline, …)`, ether out unwrapped in the same call.
//! Nothing here holds a key or picks a chain; `tx_sender_module` does the sending.

use alloy::primitives::aliases::{U160, U24};
use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;

use crate::config::ChainUniswap;
use crate::pricing::{parse_addr, Version};

sol! {
    #[allow(missing_docs)]
    interface IUniswapV2Router {
        function getAmountsOut(uint256 amountIn, address[] path) external view returns (uint256[] amounts);
        function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external returns (uint256[] amounts);
        function swapExactETHForTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline) external payable returns (uint256[] amounts);
        function swapExactTokensForETH(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external returns (uint256[] amounts);
    }

    // SwapRouter02: the structs carry NO deadline; `multicall(deadline, …)` does.
    #[allow(missing_docs)]
    struct ExactInputSingleParams {
        address tokenIn; address tokenOut; uint24 fee; address recipient;
        uint256 amountIn; uint256 amountOutMinimum; uint160 sqrtPriceLimitX96;
    }
    #[allow(missing_docs)]
    struct ExactInputParams {
        bytes path; address recipient; uint256 amountIn; uint256 amountOutMinimum;
    }
    #[allow(missing_docs)]
    interface ISwapRouter02 {
        function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256 amountOut);
        function exactInput(ExactInputParams params) external payable returns (uint256 amountOut);
        function unwrapWETH9(uint256 amountMinimum, address recipient) external payable;
        function multicall(uint256 deadline, bytes[] data) external payable returns (bytes[] results);
    }

    #[allow(missing_docs)]
    struct QuoteExactInputSingleParams {
        address tokenIn; address tokenOut; uint256 amountIn; uint24 fee; uint160 sqrtPriceLimitX96;
    }
    #[allow(missing_docs)]
    interface IQuoterV2 {
        function quoteExactInputSingle(QuoteExactInputSingleParams params) external returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
        function quoteExactInput(bytes path, uint256 amountIn) external returns (uint256 amountOut, uint160[] sqrtPriceX96AfterList, uint32[] initializedTicksCrossedList, uint256 gasEstimate);
    }

    #[allow(missing_docs)]
    interface IERC20Swap {
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function balanceOf(address owner) external view returns (uint256);
    }
    #[allow(missing_docs)]
    interface IMulticall3Balance {
        function getEthBalance(address addr) external view returns (uint256 balance);
    }
}

/// SwapRouter02's `ADDRESS_THIS`: a recipient meaning "keep it in the router" so a later
/// call in the same multicall can unwrap it.
pub fn router_itself() -> Address {
    Address::with_last_byte(2)
}

/// A V2 pool takes 0.30% on every hop.
pub const V2_FEE_BPS: u32 = 30;

/// `true` for native ETH (`address(0)`), which every route carries as WETH.
pub fn is_native(a: Address) -> bool {
    a == Address::ZERO
}

fn as_erc20(a: Address, weth: Address) -> Address {
    if is_native(a) {
        weth
    } else {
        a
    }
}

/// An amount as the module accepts it: decimal digits or `0x` hex, and never nothing. A
/// string that is not a number used to read as zero, which then quoted a swap of nothing.
pub fn parse_amount(s: &str) -> Result<U256, String> {
    let t = s.trim();
    let parsed = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        U256::from_str_radix(h, 16).ok()
    } else if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        t.parse().ok()
    } else {
        None
    };
    match parsed {
        Some(v) if !v.is_zero() => Ok(v),
        Some(_) => Err("amount must be greater than zero".to_string()),
        None => Err(format!("not an amount: {s:?}")),
    }
}

// ── Routes ───────────────────────────────────────────────────────────────────

/// One way from the input token to the output token: the ERC-20 hops (WETH stands in for
/// ether, and for a two-hop route it is the middle) and, on V3, the fee tier of each hop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub version: Version,
    pub tokens: Vec<Address>,
    pub fees: Vec<u32>,
}

impl Route {
    pub fn hops(&self) -> usize {
        self.tokens.len().saturating_sub(1)
    }

    pub fn via_weth(&self) -> bool {
        self.tokens.len() == 3
    }

    /// The pool fees the swap pays, summed over the hops, in basis points.
    pub fn fee_bps(&self) -> u32 {
        match self.version {
            Version::V2 => V2_FEE_BPS * self.hops() as u32,
            Version::V3 => self.fees.iter().map(|f| f / 100).sum(),
            Version::V4 => 0,
        }
    }

    /// The encoded V3 path: `token (20) | fee (3) | token (20) | …`.
    pub fn v3_path(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(20 + 23 * self.hops());
        out.extend_from_slice(self.tokens[0].as_slice());
        for (fee, token) in self.fees.iter().zip(self.tokens.iter().skip(1)) {
            out.extend_from_slice(&fee.to_be_bytes()[1..]);
            out.extend_from_slice(token.as_slice());
        }
        out
    }
}

// ── Quote calldata + decode ──────────────────────────────────────────────────

pub fn v2_get_amounts_out_calldata(amount_in: U256, path: &[Address]) -> Vec<u8> {
    IUniswapV2Router::getAmountsOutCall { amountIn: amount_in, path: path.to_vec() }.abi_encode()
}

/// `getAmountsOut` → the final output amount (last hop).
pub fn decode_amounts_out(data: &[u8]) -> Option<U256> {
    let amounts = IUniswapV2Router::getAmountsOutCall::abi_decode_returns(data).ok()?;
    amounts.last().copied()
}

pub fn v3_quote_calldata(token_in: Address, token_out: Address, fee: u32, amount_in: U256) -> Vec<u8> {
    let params = QuoteExactInputSingleParams {
        tokenIn: token_in,
        tokenOut: token_out,
        amountIn: amount_in,
        fee: U24::from(fee),
        sqrtPriceLimitX96: U160::ZERO,
    };
    IQuoterV2::quoteExactInputSingleCall { params }.abi_encode()
}

pub fn v3_quote_path_calldata(path: Vec<u8>, amount_in: U256) -> Vec<u8> {
    IQuoterV2::quoteExactInputCall { path: Bytes::from(path), amountIn: amount_in }.abi_encode()
}

fn word(data: &[u8], i: usize) -> Option<U256> {
    data.get(i * 32..i * 32 + 32).map(U256::from_be_slice)
}

/// QuoterV2 answers `(amountOut, …, …, gasEstimate)` for both shapes: the two middle returns
/// are a word each for the single-hop call and an offset each for the path one, so the
/// amount is always word 0 and the gas estimate always word 3.
pub fn decode_v3_quote(data: &[u8]) -> Option<(U256, Option<u64>)> {
    let amount = word(data, 0)?;
    let gas = word(data, 3).and_then(|g| u64::try_from(g).ok());
    Some((amount, gas))
}

/// The amount a probe quote asks for: a thousandth of the real one, never nothing.
pub fn probe_amount(amount_in: U256) -> U256 {
    let p = amount_in / U256::from(1000u64);
    if p.is_zero() {
        U256::from(1u64)
    } else {
        p
    }
}

// ── The batch ────────────────────────────────────────────────────────────────

/// Every read a quote needs, in one Multicall3: per route the real quote then its probe,
/// then the owner's balance and allowance if an owner was named.
pub struct QuoteBatch {
    pub calls: Vec<(Address, Vec<u8>)>,
    routes: Vec<Route>,
    probe_in: U256,
    balance_at: Option<usize>,
    /// One allowance read per router the quote may land on: `(spender, call index)`.
    allowance_at: Vec<(Address, usize)>,
}

/// One route the chain answered for.
#[derive(Clone, Debug)]
pub struct Quoted {
    pub route: Route,
    pub amount_out: U256,
    /// QuoterV2's own gas estimate of the pool legs (V3 only).
    pub gas_estimate: Option<u64>,
    /// What the probe amount would have fetched on the same route.
    pub probe_out: Option<U256>,
}

/// What the batch read about the owner. `None` where nothing was asked or the read failed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnerState {
    pub balance_in: Option<U256>,
    /// The input token's allowance per router asked about.
    pub allowances: Vec<(Address, U256)>,
}

impl OwnerState {
    pub fn allowance_for(&self, spender: Address) -> Option<U256> {
        self.allowances.iter().find(|(s, _)| *s == spender).map(|(_, a)| *a)
    }
}

/// The routers a quote may end up on: V2's for a V2 route, SwapRouter02 for V3.
pub fn router_for(chain: &ChainUniswap, version: Version) -> Option<Address> {
    match version {
        Version::V2 => chain.v2_router.as_deref().and_then(parse_addr),
        Version::V3 => chain.v3_router.as_deref().and_then(parse_addr),
        Version::V4 => None,
    }
}

/// Enumerate the candidate routes on `chain`. A pair that IS the WETH pair has no way
/// "via WETH"; ether against WETH has no route at all, because wrapping is not a swap.
pub fn candidate_routes(chain: &ChainUniswap, token_in: Address, token_out: Address) -> Vec<Route> {
    let Some(weth) = parse_addr(&chain.weth) else { return Vec::new() };
    let a = as_erc20(token_in, weth);
    let b = as_erc20(token_out, weth);
    if a == b {
        return Vec::new();
    }
    let mut routes = Vec::new();
    let can_bridge = a != weth && b != weth;
    if chain.v2_router.as_deref().and_then(parse_addr).is_some() {
        routes.push(Route { version: Version::V2, tokens: vec![a, b], fees: vec![] });
        if can_bridge {
            routes.push(Route { version: Version::V2, tokens: vec![a, weth, b], fees: vec![] });
        }
    }
    let v3 = chain.v3_quoter.as_deref().and_then(parse_addr).is_some()
        && chain.v3_router.as_deref().and_then(parse_addr).is_some();
    if v3 {
        for &fee in &chain.v3_fee_tiers {
            routes.push(Route { version: Version::V3, tokens: vec![a, b], fees: vec![fee] });
        }
        if can_bridge {
            for &f1 in &chain.v3_fee_tiers {
                for &f2 in &chain.v3_fee_tiers {
                    routes.push(Route { version: Version::V3, tokens: vec![a, weth, b], fees: vec![f1, f2] });
                }
            }
        }
    }
    routes
}

fn quote_call(chain: &ChainUniswap, route: &Route, amount_in: U256) -> Option<(Address, Vec<u8>)> {
    match route.version {
        Version::V2 => {
            let router = chain.v2_router.as_deref().and_then(parse_addr)?;
            Some((router, v2_get_amounts_out_calldata(amount_in, &route.tokens)))
        }
        Version::V3 => {
            let quoter = chain.v3_quoter.as_deref().and_then(parse_addr)?;
            let data = if route.hops() == 1 {
                v3_quote_calldata(route.tokens[0], route.tokens[1], route.fees[0], amount_in)
            } else {
                v3_quote_path_calldata(route.v3_path(), amount_in)
            };
            Some((quoter, data))
        }
        Version::V4 => None,
    }
}

/// Build the batch for `amount_in` of `token_in → token_out`. With an `owner` the batch also
/// reads what that account holds of the input token and has allowed the routers to spend.
/// Issue `calls` (Multicall3 `aggregate3`, every call allowed to fail), then [`decode_quotes`].
pub fn build_quote_batch(
    chain: &ChainUniswap,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    owner: Option<Address>,
) -> QuoteBatch {
    let probe_in = probe_amount(amount_in);
    let mut calls = Vec::new();
    let mut routes = Vec::new();
    for route in candidate_routes(chain, token_in, token_out) {
        let (Some(main), Some(probe)) = (quote_call(chain, &route, amount_in), quote_call(chain, &route, probe_in))
        else {
            continue;
        };
        calls.push(main);
        calls.push(probe);
        routes.push(route);
    }
    let mut balance_at = None;
    if let Some(owner) = owner {
        if is_native(token_in) {
            if let Some(mc) = parse_addr(&chain.multicall3) {
                balance_at = Some(calls.len());
                calls.push((mc, IMulticall3Balance::getEthBalanceCall { addr: owner }.abi_encode()));
            }
        } else {
            balance_at = Some(calls.len());
            calls.push((token_in, IERC20Swap::balanceOfCall { owner }.abi_encode()));
        }
    }
    QuoteBatch { calls, routes, probe_in, balance_at, allowance_at: Vec::new() }
}

/// Add an allowance read for one router. The spender that matters is the winning route's,
/// unknown until the quotes are back, so the glue adds one read per router the chain has
/// and picks the right answer afterwards. Ether needs none and gets none.
pub fn with_allowance_read(mut batch: QuoteBatch, token_in: Address, owner: Address, spender: Address) -> QuoteBatch {
    if !is_native(token_in) && !batch.allowance_at.iter().any(|(s, _)| *s == spender) {
        batch.allowance_at.push((spender, batch.calls.len()));
        batch.calls.push((token_in, IERC20Swap::allowanceCall { owner, spender }.abi_encode()));
    }
    batch
}

impl QuoteBatch {
    pub fn probe_in(&self) -> U256 {
        self.probe_in
    }
}

/// Decode the same-order results. `None` in `results` is a call that reverted — a pool that
/// does not exist, mostly — and simply yields no quote for that route.
pub fn decode_quotes(batch: &QuoteBatch, results: &[Option<Vec<u8>>]) -> (Vec<Quoted>, OwnerState) {
    let decode = |i: usize, route: &Route| -> Option<(U256, Option<u64>)> {
        let data = results.get(i)?.as_ref()?;
        match route.version {
            Version::V2 => decode_amounts_out(data).map(|a| (a, None)),
            Version::V3 => decode_v3_quote(data),
            Version::V4 => None,
        }
    };
    let mut quotes = Vec::new();
    for (n, route) in batch.routes.iter().enumerate() {
        let Some((amount_out, gas_estimate)) = decode(2 * n, route) else { continue };
        if amount_out.is_zero() {
            continue;
        }
        let probe_out = decode(2 * n + 1, route).map(|(a, _)| a).filter(|a| !a.is_zero());
        quotes.push(Quoted { route: route.clone(), amount_out, gas_estimate, probe_out });
    }
    let read = |i: usize| results.get(i).and_then(|r| r.as_ref()).and_then(|d| word(d, 0));
    let owner = OwnerState {
        balance_in: batch.balance_at.and_then(read),
        allowances: batch.allowance_at.iter().filter_map(|(s, i)| read(*i).map(|a| (*s, a))).collect(),
    };
    (quotes, owner)
}

/// The route with the largest output; between equals, the one with fewer hops.
pub fn pick_best(quotes: &[Quoted]) -> Option<&Quoted> {
    quotes.iter().max_by(|a, b| {
        a.amount_out
            .cmp(&b.amount_out)
            .then_with(|| b.route.hops().cmp(&a.route.hops()))
    })
}

/// How far the real rate falls short of the probe's, in basis points: `1 − rate/probeRate`.
/// The probe is a thousandth of the amount on the same pools, so what it pays in fees the
/// real amount pays too, and the difference is the amount's own weight. `None` when the
/// probe did not answer; `0` when the real rate is somehow the better one.
pub fn price_impact_bps(amount_in: U256, amount_out: U256, probe_in: U256, probe_out: U256) -> Option<u32> {
    if probe_out.is_zero() || amount_in.is_zero() {
        return None;
    }
    let num = amount_out.checked_mul(probe_in)?;
    let den = probe_out.checked_mul(amount_in)?;
    if num >= den {
        return Some(0);
    }
    let kept = num.checked_mul(U256::from(10_000u64))? / den;
    Some(10_000 - u32::try_from(kept).unwrap_or(10_000))
}

// ── Gas hints ────────────────────────────────────────────────────────────────

/// Gas for an ERC-20 `approve`, with room.
pub const APPROVE_GAS: u64 = 60_000;
const V2_SINGLE_GAS: u64 = 180_000;
const V2_TWO_HOP_GAS: u64 = 260_000;
/// What SwapRouter02 spends around the pool legs the quoter measured: the transfers in and
/// out, and the multicall itself.
const V3_ROUTER_OVERHEAD: u64 = 70_000;
const V3_HOP_FALLBACK_GAS: u64 = 150_000;
const WRAP_GAS: u64 = 30_000;
const UNWRAP_GAS: u64 = 40_000;

/// A gas limit for the swap leg. It is a hint: the sender estimates a leg it can, and a
/// swap that must wait for its approval cannot be estimated until that lands, so the
/// caller passes this instead. Generous on purpose — a limit that is too low burns the
/// fee and swaps nothing.
pub fn gas_hint(q: &Quoted, native_in: bool, native_out: bool) -> u64 {
    match q.route.version {
        Version::V2 => {
            if q.route.hops() == 1 {
                V2_SINGLE_GAS
            } else {
                V2_TWO_HOP_GAS
            }
        }
        Version::V3 => {
            let pools = q.gas_estimate.unwrap_or(V3_HOP_FALLBACK_GAS * q.route.hops() as u64);
            let mut g = pools + pools / 4 + V3_ROUTER_OVERHEAD;
            if native_in {
                g += WRAP_GAS;
            }
            if native_out {
                g += UNWRAP_GAS;
            }
            g
        }
        Version::V4 => V3_HOP_FALLBACK_GAS,
    }
}

// ── Approval policy ──────────────────────────────────────────────────────────

/// What must be approved before the swap can pull the input token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    /// Ether needs no approval, and neither does an allowance that already covers the amount.
    None,
    /// Approve exactly the amount. No infinite approvals: the router is trusted with one swap.
    Set(U256),
    /// Some tokens (USDT is the famous one) refuse to move a non-zero allowance to another
    /// non-zero one, so an allowance that is there but too small is zeroed first.
    ResetThenSet(U256),
}

/// An allowance nobody read (`None`) is treated as absent: approving what is already
/// approved is harmless, while skipping a needed approval fails the swap.
pub fn approval_needed(native_in: bool, allowance: Option<U256>, amount_in: U256) -> Approval {
    if native_in {
        return Approval::None;
    }
    match allowance {
        Some(a) if a >= amount_in => Approval::None,
        Some(a) if !a.is_zero() => Approval::ResetThenSet(amount_in),
        _ => Approval::Set(amount_in),
    }
}

// ── Swap transaction building ────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallKind {
    Approve,
    Swap,
}

/// One transaction of a swap, in the shape `tx_sender_module` takes.
#[derive(Clone, Debug)]
pub struct Call {
    pub kind: CallKind,
    pub to: Address,
    pub value: U256,
    pub data: Vec<u8>,
    pub gas_limit_hint: u64,
    pub label: String,
}

/// The calls that make the swap, in the order they must land.
#[derive(Clone, Debug)]
pub struct BuiltSwap {
    pub spender: Address,
    pub calls: Vec<Call>,
}

pub fn erc20_approve_calldata(spender: Address, amount: U256) -> Vec<u8> {
    IERC20Swap::approveCall { spender, amount }.abi_encode()
}

/// Names for the labels on the sender's rows; the caller knows the symbols, this file does not.
pub struct Names<'a> {
    pub token_in: &'a str,
    pub token_out: &'a str,
}

/// Encode the swap for a quoted route. `amount_out_min` is the slippage floor the caller
/// derived from `q.amount_out`; `deadline` is a unix time the caller (which has a clock)
/// picked. `None` only when the chain has no router for the route's version.
pub fn build_swap(
    chain: &ChainUniswap,
    q: &Quoted,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    amount_out_min: U256,
    recipient: Address,
    deadline: U256,
    approval: Approval,
    names: Names<'_>,
) -> Option<BuiltSwap> {
    let native_in = is_native(token_in);
    let native_out = is_native(token_out);
    let router = router_for(chain, q.route.version)?;
    let value = if native_in { amount_in } else { U256::ZERO };
    let path = q.route.tokens.clone();

    let data = match q.route.version {
        Version::V2 => {
            if native_in {
                IUniswapV2Router::swapExactETHForTokensCall { amountOutMin: amount_out_min, path, to: recipient, deadline }
                    .abi_encode()
            } else if native_out {
                IUniswapV2Router::swapExactTokensForETHCall {
                    amountIn: amount_in,
                    amountOutMin: amount_out_min,
                    path,
                    to: recipient,
                    deadline,
                }
                .abi_encode()
            } else {
                IUniswapV2Router::swapExactTokensForTokensCall {
                    amountIn: amount_in,
                    amountOutMin: amount_out_min,
                    path,
                    to: recipient,
                    deadline,
                }
                .abi_encode()
            }
        }
        Version::V3 => {
            // Ether out: the swap pays the router and the unwrap pays the recipient.
            let swap_to = if native_out { router_itself() } else { recipient };
            let swap = if q.route.hops() == 1 {
                let params = ExactInputSingleParams {
                    tokenIn: q.route.tokens[0],
                    tokenOut: q.route.tokens[1],
                    fee: U24::from(q.route.fees[0]),
                    recipient: swap_to,
                    amountIn: amount_in,
                    amountOutMinimum: amount_out_min,
                    sqrtPriceLimitX96: U160::ZERO,
                };
                ISwapRouter02::exactInputSingleCall { params }.abi_encode()
            } else {
                let params = ExactInputParams {
                    path: Bytes::from(q.route.v3_path()),
                    recipient: swap_to,
                    amountIn: amount_in,
                    amountOutMinimum: amount_out_min,
                };
                ISwapRouter02::exactInputCall { params }.abi_encode()
            };
            let mut inner = vec![Bytes::from(swap)];
            if native_out {
                inner.push(Bytes::from(
                    ISwapRouter02::unwrapWETH9Call { amountMinimum: amount_out_min, recipient }.abi_encode(),
                ));
            }
            ISwapRouter02::multicallCall { deadline, data: inner }.abi_encode()
        }
        Version::V4 => return None,
    };

    let mut calls = Vec::new();
    let approve = |amount: U256| Call {
        kind: CallKind::Approve,
        to: token_in,
        value: U256::ZERO,
        data: erc20_approve_calldata(router, amount),
        gas_limit_hint: APPROVE_GAS,
        label: if amount.is_zero() {
            format!("Reset the {} allowance for Uniswap", names.token_in)
        } else {
            format!("Approve {} for Uniswap", names.token_in)
        },
    };
    match approval {
        Approval::None => {}
        Approval::Set(amount) => calls.push(approve(amount)),
        Approval::ResetThenSet(amount) => {
            calls.push(approve(U256::ZERO));
            calls.push(approve(amount));
        }
    }
    calls.push(Call {
        kind: CallKind::Swap,
        to: router,
        value,
        data,
        gas_limit_hint: gas_hint(q, native_in, native_out),
        label: format!(
            "Swap {} for {} on Uniswap {}",
            names.token_in,
            names.token_out,
            match q.route.version {
                Version::V2 => "V2",
                Version::V3 => "V3",
                Version::V4 => "V4",
            }
        ),
    });
    Some(BuiltSwap { spender: router, calls })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use alloy::sol_types::SolValue;

    const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
    const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const ALICE: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
    const ROUTER02: Address = address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45");

    fn mainnet() -> ChainUniswap {
        crate::config::default_chains().remove(&1).unwrap()
    }

    fn names() -> Names<'static> {
        Names { token_in: "USDC", token_out: "DAI" }
    }

    // QuoterV2 single-hop return: (amountOut, sqrtPriceAfter, ticksCrossed, gasEstimate).
    fn quote_ret(amount_out: u64, gas: u64) -> Vec<u8> {
        (U256::from(amount_out), U256::ZERO, U256::ZERO, U256::from(gas)).abi_encode_params()
    }

    // QuoterV2 path return: the two middle returns are dynamic arrays.
    fn path_quote_ret(amount_out: u64, gas: u64) -> Vec<u8> {
        IQuoterV2::quoteExactInputCall::abi_encode_returns(&IQuoterV2::quoteExactInputReturn {
            amountOut: U256::from(amount_out),
            sqrtPriceX96AfterList: vec![U160::ZERO, U160::ZERO],
            initializedTicksCrossedList: vec![0, 0],
            gasEstimate: U256::from(gas),
        })
    }

    fn v2_ret(amount_out: u64) -> Vec<u8> {
        IUniswapV2Router::getAmountsOutCall::abi_encode_returns(&vec![U256::from(1u64), U256::from(amount_out)])
    }

    fn word_ret(v: u64) -> Vec<u8> {
        U256::from(v).abi_encode()
    }

    #[test]
    fn the_selectors_are_swaprouter02s() {
        let params = ExactInputSingleParams {
            tokenIn: USDC, tokenOut: WETH, fee: U24::from(500u32), recipient: ALICE,
            amountIn: U256::from(1u64), amountOutMinimum: U256::ZERO, sqrtPriceLimitX96: U160::ZERO,
        };
        assert_eq!(&ISwapRouter02::exactInputSingleCall { params }.abi_encode()[..4], &[0x04, 0xe4, 0x5a, 0xaf]);
        let params = ExactInputParams { path: Bytes::new(), recipient: ALICE, amountIn: U256::ZERO, amountOutMinimum: U256::ZERO };
        assert_eq!(&ISwapRouter02::exactInputCall { params }.abi_encode()[..4], &[0xb8, 0x58, 0x18, 0x3f]);
        assert_eq!(&ISwapRouter02::unwrapWETH9Call { amountMinimum: U256::ZERO, recipient: ALICE }.abi_encode()[..4], &[0x49, 0x40, 0x4b, 0x7c]);
        assert_eq!(&ISwapRouter02::multicallCall { deadline: U256::ZERO, data: vec![] }.abi_encode()[..4], &[0x5a, 0xe4, 0x01, 0xdc]);
        assert_eq!(&IQuoterV2::quoteExactInputCall { path: Bytes::new(), amountIn: U256::ZERO }.abi_encode()[..4], &[0xcd, 0xca, 0x17, 0x53]);
        assert_eq!(&v2_get_amounts_out_calldata(U256::from(1u64), &[USDC, WETH])[..4], &[0xd0, 0x6c, 0xa6, 0x1f]);
        assert_eq!(&erc20_approve_calldata(WETH, U256::from(5u64))[..4], &[0x09, 0x5e, 0xa7, 0xb3]);
        assert_eq!(&IERC20Swap::allowanceCall { owner: ALICE, spender: WETH }.abi_encode()[..4], &[0xdd, 0x62, 0xed, 0x3e]);
        assert_eq!(&IMulticall3Balance::getEthBalanceCall { addr: ALICE }.abi_encode()[..4], &[0x4d, 0x23, 0x01, 0xcc]);
    }

    #[test]
    fn a_v3_path_packs_twenty_three_bytes_per_hop() {
        let one = Route { version: Version::V3, tokens: vec![USDC, WETH], fees: vec![500] };
        let p = one.v3_path();
        assert_eq!(p.len(), 43);
        assert_eq!(&p[..20], USDC.as_slice());
        assert_eq!(&p[20..23], &[0x00, 0x01, 0xf4]);
        assert_eq!(&p[23..], WETH.as_slice());
        let two = Route { version: Version::V3, tokens: vec![USDC, WETH, DAI], fees: vec![500, 3000] };
        let p = two.v3_path();
        assert_eq!(p.len(), 66);
        assert_eq!(&p[43..46], &[0x00, 0x0b, 0xb8]);
        assert!(two.via_weth() && !one.via_weth());
        assert_eq!(two.fee_bps(), 35);
        assert_eq!(Route { version: Version::V2, tokens: vec![USDC, WETH, DAI], fees: vec![] }.fee_bps(), 60);
    }

    #[test]
    fn the_candidates_cover_direct_and_via_weth_on_both_versions() {
        let chain = mainnet();
        // USDC → DAI: V2 direct + V2 via WETH + 4 V3 direct + 16 V3 via WETH = 22 routes.
        let routes = candidate_routes(&chain, USDC, DAI);
        assert_eq!(routes.len(), 22);
        assert_eq!(routes.iter().filter(|r| r.via_weth()).count(), 17);
        // Against WETH itself (ether in), nothing goes "via WETH": V2 direct + 4 V3 direct.
        let routes = candidate_routes(&chain, Address::ZERO, USDC);
        assert_eq!(routes.len(), 5);
        assert!(routes.iter().all(|r| r.tokens[0] == WETH));
        // Ether against WETH is a wrap, not a swap.
        assert!(candidate_routes(&chain, Address::ZERO, WETH).is_empty());
        // Each route costs two calls: the quote and its probe.
        let batch = build_quote_batch(&chain, USDC, DAI, U256::from(1_000_000u64), None);
        assert_eq!(batch.calls.len(), 44);
        assert_eq!(batch.probe_in(), U256::from(1_000u64));
    }

    #[test]
    fn a_probe_is_a_thousandth_and_never_nothing() {
        assert_eq!(probe_amount(U256::from(5_000u64)), U256::from(5u64));
        assert_eq!(probe_amount(U256::from(999u64)), U256::from(1u64));
    }

    #[test]
    fn the_owner_reads_ride_at_the_end_of_the_batch() {
        let chain = mainnet();
        let n = candidate_routes(&chain, USDC, DAI).len();
        let batch = build_quote_batch(&chain, USDC, DAI, U256::from(1_000_000u64), Some(ALICE));
        let v2 = parse_addr(chain.v2_router.as_deref().unwrap()).unwrap();
        let batch = with_allowance_read(batch, USDC, ALICE, ROUTER02);
        let batch = with_allowance_read(batch, USDC, ALICE, v2);
        let batch = with_allowance_read(batch, USDC, ALICE, v2); // asked twice, read once
        assert_eq!(batch.calls.len(), 2 * n + 3);
        assert_eq!(batch.calls[2 * n].0, USDC); // balanceOf on the token
        assert_eq!(&batch.calls[2 * n].1[..4], &[0x70, 0xa0, 0x82, 0x31]);
        assert_eq!(batch.calls[2 * n + 1].0, USDC); // allowance on the token, per router
        assert_eq!(batch.calls[2 * n + 2].0, USDC);
        // Ether: the balance comes from Multicall3 and there is no allowance to read.
        let batch = build_quote_batch(&chain, Address::ZERO, USDC, U256::from(1u64), Some(ALICE));
        let m = candidate_routes(&chain, Address::ZERO, USDC).len();
        let batch = with_allowance_read(batch, Address::ZERO, ALICE, ROUTER02);
        assert_eq!(batch.calls.len(), 2 * m + 1);
        assert_eq!(batch.calls[2 * m].0, parse_addr(&chain.multicall3).unwrap());
    }

    #[test]
    fn the_best_route_is_the_largest_output_and_the_probe_travels_with_it() {
        let chain = mainnet();
        let amount = U256::from(1_000_000u64);
        let batch = build_quote_batch(&chain, USDC, DAI, amount, Some(ALICE));
        let batch = with_allowance_read(batch, USDC, ALICE, ROUTER02);
        let routes = candidate_routes(&chain, USDC, DAI);
        let mut results: Vec<Option<Vec<u8>>> = Vec::new();
        for (i, r) in routes.iter().enumerate() {
            // Route 7 (V3 via WETH, tiers 100/500) wins with 990; its probe answers 1 for the
            // 1000-unit probe amount, i.e. a rate of 0.001 against the real 0.00099.
            let (main, probe) = if i == 7 { (990u64, 1u64) } else { (900, 1) };
            let ret = |amt: u64| match r.version {
                Version::V2 => v2_ret(amt),
                Version::V3 if r.hops() == 1 => quote_ret(amt, 120_000),
                _ => path_quote_ret(amt, 240_000),
            };
            results.push(Some(ret(main)));
            results.push(Some(ret(probe)));
        }
        results.push(Some(word_ret(5_000_000))); // balance
        results.push(Some(word_ret(0))); // allowance
        let (quotes, owner) = decode_quotes(&batch, &results);
        assert_eq!(quotes.len(), 22);
        let best = pick_best(&quotes).unwrap();
        assert_eq!(best.route, routes[7]);
        assert_eq!(best.amount_out, U256::from(990u64));
        assert_eq!(best.gas_estimate, Some(240_000));
        assert_eq!(best.probe_out, Some(U256::from(1u64)));
        assert_eq!(owner, OwnerState { balance_in: Some(U256::from(5_000_000u64)), allowances: vec![(ROUTER02, U256::ZERO)] });
        assert_eq!(owner.allowance_for(ROUTER02), Some(U256::ZERO));
        assert_eq!(owner.allowance_for(ALICE), None);
        // 990/1_000_000 against 1/1_000: 1 − 0.99 = 100 bps.
        assert_eq!(price_impact_bps(amount, best.amount_out, batch.probe_in(), U256::from(1u64)), Some(100));
    }

    #[test]
    fn a_reverted_quote_is_no_route_and_a_missing_probe_is_no_impact() {
        let chain = mainnet();
        let batch = build_quote_batch(&chain, Address::ZERO, USDC, U256::from(10u64), None);
        let routes = candidate_routes(&chain, Address::ZERO, USDC);
        let mut results: Vec<Option<Vec<u8>>> = vec![None; 2 * routes.len()];
        results[2] = Some(quote_ret(30, 90_000)); // first V3 tier answers, its probe does not
        let (quotes, owner) = decode_quotes(&batch, &results);
        assert_eq!(quotes.len(), 1);
        assert_eq!(quotes[0].probe_out, None);
        assert_eq!(owner, OwnerState::default());
        assert_eq!(price_impact_bps(U256::from(10u64), U256::from(30u64), U256::from(1u64), U256::ZERO), None);
        // A zero output is not a quote either.
        results[2] = Some(quote_ret(0, 0));
        assert!(decode_quotes(&batch, &results).0.is_empty());
    }

    #[test]
    fn price_impact_is_the_shortfall_against_the_marginal_rate() {
        // Real: 1000 → 950. Probe: 1 → 1. Rate 0.95 vs 1 → 500 bps.
        assert_eq!(price_impact_bps(U256::from(1000u64), U256::from(950u64), U256::from(1u64), U256::from(1u64)), Some(500));
        // The real rate being better than the probe's is impact zero, not negative.
        assert_eq!(price_impact_bps(U256::from(1000u64), U256::from(1100u64), U256::from(1u64), U256::from(1u64)), Some(0));
        // Equal rates: none.
        assert_eq!(price_impact_bps(U256::from(1000u64), U256::from(1000u64), U256::from(1u64), U256::from(1u64)), Some(0));
    }

    #[test]
    fn between_equal_outputs_the_shorter_route_wins() {
        let short = Quoted { route: Route { version: Version::V3, tokens: vec![USDC, DAI], fees: vec![500] }, amount_out: U256::from(9u64), gas_estimate: None, probe_out: None };
        let long = Quoted { route: Route { version: Version::V3, tokens: vec![USDC, WETH, DAI], fees: vec![500, 500] }, amount_out: U256::from(9u64), gas_estimate: None, probe_out: None };
        assert_eq!(pick_best(&[long.clone(), short.clone()]).unwrap().route, short.route);
        assert!(pick_best(&[]).is_none());
    }

    #[test]
    fn the_approval_policy_resets_a_stale_allowance_and_skips_a_sufficient_one() {
        let amt = U256::from(100u64);
        assert_eq!(approval_needed(true, None, amt), Approval::None);
        assert_eq!(approval_needed(false, Some(U256::from(100u64)), amt), Approval::None);
        assert_eq!(approval_needed(false, Some(U256::from(1_000u64)), amt), Approval::None);
        assert_eq!(approval_needed(false, Some(U256::ZERO), amt), Approval::Set(amt));
        assert_eq!(approval_needed(false, None, amt), Approval::Set(amt));
        assert_eq!(approval_needed(false, Some(U256::from(99u64)), amt), Approval::ResetThenSet(amt));
    }

    fn quoted(route: Route, gas: Option<u64>) -> Quoted {
        Quoted { route, amount_out: U256::from(3000u64), gas_estimate: gas, probe_out: None }
    }

    #[test]
    fn a_v3_swap_is_a_multicall_with_the_deadline_and_an_unwrap_for_ether_out() {
        let chain = mainnet();
        let q = quoted(Route { version: Version::V3, tokens: vec![USDC, WETH], fees: vec![500] }, Some(100_000));
        let built = build_swap(&chain, &q, USDC, Address::ZERO, U256::from(1_000u64), U256::from(2_900u64), ALICE, U256::from(99u64), Approval::Set(U256::from(1_000u64)), names()).unwrap();
        assert_eq!(built.spender, ROUTER02);
        assert_eq!(built.calls.len(), 2);
        let approve = &built.calls[0];
        assert_eq!((approve.kind, approve.to, approve.value), (CallKind::Approve, USDC, U256::ZERO));
        assert_eq!(approve.data, erc20_approve_calldata(ROUTER02, U256::from(1_000u64)));
        assert_eq!(approve.gas_limit_hint, APPROVE_GAS);
        assert_eq!(approve.label, "Approve USDC for Uniswap");
        let swap = &built.calls[1];
        assert_eq!((swap.kind, swap.to, swap.value), (CallKind::Swap, ROUTER02, U256::ZERO));
        assert_eq!(swap.label, "Swap USDC for DAI on Uniswap V3");
        let mc = ISwapRouter02::multicallCall::abi_decode(&swap.data).unwrap();
        assert_eq!(mc.deadline, U256::from(99u64));
        assert_eq!(mc.data.len(), 2);
        let single = ISwapRouter02::exactInputSingleCall::abi_decode(&mc.data[0]).unwrap();
        assert_eq!(single.params.recipient, router_itself(), "ether out lands in the router first");
        assert_eq!(single.params.tokenOut, WETH);
        assert_eq!(single.params.amountOutMinimum, U256::from(2_900u64));
        let unwrap = ISwapRouter02::unwrapWETH9Call::abi_decode(&mc.data[1]).unwrap();
        assert_eq!((unwrap.amountMinimum, unwrap.recipient), (U256::from(2_900u64), ALICE));
        // 100_000 + 25_000 + 70_000 + unwrap 40_000.
        assert_eq!(swap.gas_limit_hint, 235_000);
    }

    #[test]
    fn a_two_hop_v3_swap_uses_exact_input_with_the_packed_path_and_pays_the_recipient() {
        let chain = mainnet();
        let route = Route { version: Version::V3, tokens: vec![WETH, USDC, DAI], fees: vec![500, 100] };
        let q = quoted(route.clone(), None);
        let built = build_swap(&chain, &q, Address::ZERO, DAI, U256::from(7u64), U256::from(6u64), ALICE, U256::from(1u64), Approval::None, names()).unwrap();
        assert_eq!(built.calls.len(), 1, "ether in needs no approval");
        let swap = &built.calls[0];
        assert_eq!(swap.value, U256::from(7u64), "ether in rides as value");
        let mc = ISwapRouter02::multicallCall::abi_decode(&swap.data).unwrap();
        assert_eq!(mc.data.len(), 1, "no unwrap: the output is a token");
        let multi = ISwapRouter02::exactInputCall::abi_decode(&mc.data[0]).unwrap();
        assert_eq!(multi.params.path.as_ref(), route.v3_path().as_slice());
        assert_eq!(multi.params.recipient, ALICE);
        // Two fallback hops: 300_000 + 75_000 + 70_000 + wrap 30_000.
        assert_eq!(swap.gas_limit_hint, 475_000);
    }

    #[test]
    fn a_v2_swap_goes_through_the_legacy_router_with_its_own_deadline() {
        let chain = mainnet();
        let v2 = parse_addr(chain.v2_router.as_deref().unwrap()).unwrap();
        let route = Route { version: Version::V2, tokens: vec![USDC, WETH, DAI], fees: vec![] };
        let q = quoted(route, None);
        let built = build_swap(&chain, &q, USDC, DAI, U256::from(5u64), U256::from(4u64), ALICE, U256::from(77u64), Approval::ResetThenSet(U256::from(5u64)), names()).unwrap();
        assert_eq!(built.spender, v2);
        assert_eq!(built.calls.len(), 3);
        assert_eq!(built.calls[0].label, "Reset the USDC allowance for Uniswap");
        assert_eq!(built.calls[0].data, erc20_approve_calldata(v2, U256::ZERO));
        assert_eq!(built.calls[1].data, erc20_approve_calldata(v2, U256::from(5u64)));
        let swap = &built.calls[2];
        assert_eq!(swap.to, v2);
        let call = IUniswapV2Router::swapExactTokensForTokensCall::abi_decode(&swap.data).unwrap();
        assert_eq!((call.amountIn, call.amountOutMin, call.deadline), (U256::from(5u64), U256::from(4u64), U256::from(77u64)));
        assert_eq!(call.path, vec![USDC, WETH, DAI]);
        assert_eq!(swap.gas_limit_hint, 260_000);
        // Ether out on V2 is one call, not a multicall.
        let q = quoted(Route { version: Version::V2, tokens: vec![USDC, WETH], fees: vec![] }, None);
        let built = build_swap(&chain, &q, USDC, Address::ZERO, U256::from(5u64), U256::from(4u64), ALICE, U256::from(77u64), Approval::None, names()).unwrap();
        assert_eq!(&built.calls[0].data[..4], &[0x18, 0xcb, 0xaf, 0xe5]); // swapExactTokensForETH
    }

    #[test]
    fn a_chain_without_a_router_for_the_route_builds_nothing() {
        let mut chain = mainnet();
        chain.v3_router = None;
        let q = quoted(Route { version: Version::V3, tokens: vec![USDC, WETH], fees: vec![500] }, None);
        assert!(build_swap(&chain, &q, USDC, WETH, U256::from(1u64), U256::from(1u64), ALICE, U256::from(1u64), Approval::None, names()).is_none());
        // And a chain without a V3 router offers no V3 candidates to begin with.
        assert!(candidate_routes(&chain, USDC, DAI).iter().all(|r| r.version == Version::V2));
    }

    #[test]
    fn an_amount_is_digits_or_hex_and_never_nothing() {
        assert_eq!(parse_amount("1000"), Ok(U256::from(1000u64)));
        assert_eq!(parse_amount(" 0x10 "), Ok(U256::from(16u64)));
        assert!(parse_amount("0").unwrap_err().contains("greater than zero"));
        assert!(parse_amount("").unwrap_err().contains("not an amount"));
        assert!(parse_amount("1.5").unwrap_err().contains("not an amount"));
        assert!(parse_amount("ten").unwrap_err().contains("not an amount"));
        assert!(parse_amount("-1").is_err());
    }

    #[test]
    fn the_quoter_gas_estimate_is_word_three_in_both_shapes() {
        assert_eq!(decode_v3_quote(&quote_ret(5, 123)), Some((U256::from(5u64), Some(123))));
        assert_eq!(decode_v3_quote(&path_quote_ret(6, 456)), Some((U256::from(6u64), Some(456))));
        assert_eq!(decode_v3_quote(&[0u8; 32]), Some((U256::ZERO, None)));
    }
}
