//! Uniswap quote + swap-transaction building (V2 and V3; V4 is a fast-follow).
//!
//! Pure/offline, like `pricing`: we build the `eth_call` calldata for quotes
//! (V2 `getAmountsOut`, V3 `QuoterV2.quoteExactInputSingle`), decode them, pick
//! the best route, and ABI-encode the router calldata for the winning swap. The
//! glue issues the calls through `eth_rpc_module` and the backend assembles the
//! unsigned tx (nonce/fee/gas) around the `(router, value, data)` we return.
//!
//! Native ETH is represented as `address(0)`; we substitute WETH into paths /
//! params and set the tx `value` when ETH is the input. (Native **output** on V3
//! yields WETH to the recipient — unwrap is a fast-follow alongside V4 swaps.)

use alloy::primitives::aliases::{U160, U24};
use alloy::primitives::{Address, U256};
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

    #[allow(missing_docs)]
    struct ExactInputSingleParams {
        address tokenIn; address tokenOut; uint24 fee; address recipient;
        uint256 deadline; uint256 amountIn; uint256 amountOutMinimum; uint160 sqrtPriceLimitX96;
    }
    #[allow(missing_docs)]
    interface ISwapRouter {
        function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256 amountOut);
    }

    #[allow(missing_docs)]
    struct QuoteExactInputSingleParams {
        address tokenIn; address tokenOut; uint256 amountIn; uint24 fee; uint160 sqrtPriceLimitX96;
    }
    #[allow(missing_docs)]
    interface IQuoterV2 {
        function quoteExactInputSingle(QuoteExactInputSingleParams params) external returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
    }

    #[allow(missing_docs)]
    interface IERC20Approve {
        function approve(address spender, uint256 amount) external returns (bool);
    }
}

/// `true` for native ETH (`address(0)`), which we route through WETH.
fn is_native(a: Address) -> bool {
    a == Address::ZERO
}

/// Replace native ETH with WETH for path/param construction.
fn as_erc20(a: Address, weth: Address) -> Address {
    if is_native(a) {
        weth
    } else {
        a
    }
}

// ── Quote calldata + decode ──────────────────────────────────────────────────

pub fn v2_get_amounts_out_calldata(amount_in: U256, path: &[Address]) -> Vec<u8> {
    IUniswapV2Router::getAmountsOutCall { amountIn: amount_in, path: path.to_vec() }.abi_encode()
}

/// Decode `getAmountsOut` → the final output amount (last hop).
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

/// Decode QuoterV2 → `amountOut` (first return word).
pub fn decode_v3_quote(data: &[u8]) -> Option<U256> {
    data.get(0..32).map(U256::from_be_slice)
}

// ── Best-quote selection across V2 + V3 fee tiers ────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuoteKind {
    V2,
    V3 { fee: u32 },
}

/// A built set of quote calls plus the parallel kinds describing each result.
pub struct QuoteBatch {
    /// `(target, calldata)` pairs to issue (router for V2, quoter for V3).
    pub calls: Vec<(Address, Vec<u8>)>,
    kinds: Vec<QuoteKind>,
}

/// The winning route for a swap.
#[derive(Clone, Copy, Debug)]
pub struct BestQuote {
    pub version: Version,
    /// Fee tier (V3 only; 0 for V2).
    pub fee: u32,
    pub amount_out: U256,
}

/// Build the quote calls for `amount_in` of `token_in` → `token_out` across V2
/// (if a router is configured) and every configured V3 fee tier. Issue `calls`
/// (e.g. batched via Multicall3 with `allowFailure`), then decode with
/// [`decode_best_quote`].
pub fn build_quote_batch(chain: &ChainUniswap, token_in: Address, token_out: Address, amount_in: U256) -> QuoteBatch {
    let weth = parse_addr(&chain.weth).unwrap_or(Address::ZERO);
    let in_erc20 = as_erc20(token_in, weth);
    let out_erc20 = as_erc20(token_out, weth);

    let mut calls = Vec::new();
    let mut kinds = Vec::new();

    if let Some(router) = chain.v2_router.as_deref().and_then(parse_addr) {
        let path = vec![in_erc20, out_erc20];
        calls.push((router, v2_get_amounts_out_calldata(amount_in, &path)));
        kinds.push(QuoteKind::V2);
    }

    if let Some(quoter) = chain.v3_quoter.as_deref().and_then(parse_addr) {
        for &fee in &chain.v3_fee_tiers {
            calls.push((quoter, v3_quote_calldata(in_erc20, out_erc20, fee, amount_in)));
            kinds.push(QuoteKind::V3 { fee });
        }
    }

    QuoteBatch { calls, kinds }
}

/// Decode the (same-order) quote results and return the route with the largest
/// output. `results` is the per-call return data (`None` = the call reverted).
pub fn decode_best_quote(batch: &QuoteBatch, results: &[Option<Vec<u8>>]) -> Option<BestQuote> {
    let mut best: Option<BestQuote> = None;
    for (kind, ret) in batch.kinds.iter().zip(results.iter()) {
        let Some(data) = ret else { continue };
        let (version, fee, amount) = match kind {
            QuoteKind::V2 => (Version::V2, 0u32, decode_amounts_out(data)),
            QuoteKind::V3 { fee } => (Version::V3, *fee, decode_v3_quote(data)),
        };
        let Some(amount) = amount.filter(|a| !a.is_zero()) else { continue };
        if best.map(|b| amount > b.amount_out).unwrap_or(true) {
            best = Some(BestQuote { version, fee, amount_out: amount });
        }
    }
    best
}

// ── Swap transaction building ────────────────────────────────────────────────

/// Everything the backend needs to broadcast a swap: the router call plus, for
/// ERC20 inputs, the ERC20 `approve` that must land first.
#[derive(Clone, Debug)]
pub struct BuiltSwap {
    pub router: Address,
    /// `amountIn` when the input is native ETH, else `0`.
    pub value: U256,
    pub data: Vec<u8>,
    /// `(token, spender, calldata)` for a prerequisite ERC20 approval, if needed.
    pub approve: Option<(Address, Address, Vec<u8>)>,
}

pub fn erc20_approve_calldata(spender: Address, amount: U256) -> Vec<u8> {
    IERC20Approve::approveCall { spender, amount }.abi_encode()
}

/// Build the unsigned swap call for a chosen `BestQuote`. `amount_out_min` is the
/// slippage floor (caller derives it from `quote.amount_out`), `deadline` is a
/// unix timestamp supplied by the (clock-bearing) caller.
pub fn build_swap(
    chain: &ChainUniswap,
    quote: &BestQuote,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    amount_out_min: U256,
    recipient: Address,
    deadline: U256,
) -> Option<BuiltSwap> {
    let weth = parse_addr(&chain.weth)?;
    let in_native = is_native(token_in);
    let out_native = is_native(token_out);
    let in_erc20 = as_erc20(token_in, weth);
    let out_erc20 = as_erc20(token_out, weth);

    match quote.version {
        Version::V2 => {
            let router = chain.v2_router.as_deref().and_then(parse_addr)?;
            let (data, value) = if in_native {
                (
                    IUniswapV2Router::swapExactETHForTokensCall {
                        amountOutMin: amount_out_min,
                        path: vec![weth, out_erc20],
                        to: recipient,
                        deadline,
                    }
                    .abi_encode(),
                    amount_in,
                )
            } else if out_native {
                (
                    IUniswapV2Router::swapExactTokensForETHCall {
                        amountIn: amount_in,
                        amountOutMin: amount_out_min,
                        path: vec![in_erc20, weth],
                        to: recipient,
                        deadline,
                    }
                    .abi_encode(),
                    U256::ZERO,
                )
            } else {
                (
                    IUniswapV2Router::swapExactTokensForTokensCall {
                        amountIn: amount_in,
                        amountOutMin: amount_out_min,
                        path: vec![in_erc20, out_erc20],
                        to: recipient,
                        deadline,
                    }
                    .abi_encode(),
                    U256::ZERO,
                )
            };
            Some(BuiltSwap { router, value, data, approve: approval(in_native, token_in, router, amount_in) })
        }
        Version::V3 => {
            let router = chain.v3_router.as_deref().and_then(parse_addr)?;
            let params = ExactInputSingleParams {
                tokenIn: in_erc20,
                tokenOut: out_erc20,
                fee: U24::from(quote.fee),
                recipient,
                deadline,
                amountIn: amount_in,
                amountOutMinimum: amount_out_min,
                sqrtPriceLimitX96: U160::ZERO,
            };
            let data = ISwapRouter::exactInputSingleCall { params }.abi_encode();
            let value = if in_native { amount_in } else { U256::ZERO };
            Some(BuiltSwap { router, value, data, approve: approval(in_native, token_in, router, amount_in) })
        }
        // V4 swaps are a fast-follow (Universal Router / pool-manager unlock).
        Version::V4 => None,
    }
}

fn approval(in_native: bool, token_in: Address, router: Address, amount_in: U256) -> Option<(Address, Address, Vec<u8>)> {
    if in_native {
        None
    } else {
        Some((token_in, router, erc20_approve_calldata(router, amount_in)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use alloy::sol_types::SolValue;

    const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const ALICE: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");

    fn mainnet() -> ChainUniswap {
        crate::config::default_chains().remove(&1).unwrap()
    }

    #[test]
    fn quote_and_swap_selectors() {
        let q = v2_get_amounts_out_calldata(U256::from(1u64), &[USDC, WETH]);
        assert_eq!(&q[0..4], &[0xd0, 0x6c, 0xa6, 0x1f]); // getAmountsOut
        let a = erc20_approve_calldata(WETH, U256::from(5u64));
        assert_eq!(&a[0..4], &[0x09, 0x5e, 0xa7, 0xb3]); // approve
        let v3 = v3_quote_calldata(USDC, WETH, 500, U256::from(1u64));
        assert_eq!(v3.len() % 32, 4); // selector + 5 static words
    }

    #[test]
    fn amounts_out_decodes_last_hop() {
        let amounts = vec![U256::from(1_000u64), U256::from(7u64), U256::from(42u64)];
        let encoded = IUniswapV2Router::getAmountsOutCall::abi_encode_returns(&amounts);
        assert_eq!(decode_amounts_out(&encoded), Some(U256::from(42u64)));
    }

    #[test]
    fn best_quote_picks_largest_output() {
        let chain = mainnet();
        let batch = build_quote_batch(&chain, USDC, WETH, U256::from(1_000_000u64));
        // mainnet: V2 router + V3 quoter over 4 fee tiers = 5 calls.
        assert_eq!(batch.calls.len(), 5);

        // Fabricate returns: V2 = 10, V3 tiers = [5, 99, 0, 7] → best is V3 fee 500 (=99).
        let mut results: Vec<Option<Vec<u8>>> = Vec::new();
        results.push(Some(IUniswapV2Router::getAmountsOutCall::abi_encode_returns(&vec![U256::from(1u64), U256::from(10u64)])));
        for amt in [5u64, 99, 0, 7] {
            results.push(Some(quote_ret(U256::from(amt))));
        }
        let best = decode_best_quote(&batch, &results).unwrap();
        assert_eq!(best.version, Version::V3);
        assert_eq!(best.fee, 500);
        assert_eq!(best.amount_out, U256::from(99u64));
    }

    // QuoterV2 returns (amountOut, sqrtPriceX96After, ticksCrossed, gasEstimate).
    fn quote_ret(amount_out: U256) -> Vec<u8> {
        (amount_out, U256::ZERO, U256::ZERO, U256::ZERO).abi_encode_params()
    }

    #[test]
    fn build_swap_native_in_sets_value_no_approval() {
        let chain = mainnet();
        let quote = BestQuote { version: Version::V3, fee: 500, amount_out: U256::from(3000u64) };
        let swap = build_swap(&chain, &quote, Address::ZERO, USDC, U256::from(1_000u64), U256::from(2_900u64), ALICE, U256::from(99u64)).unwrap();
        assert_eq!(swap.value, U256::from(1_000u64)); // native in → value carries ETH
        assert!(swap.approve.is_none());
        assert_eq!(&swap.data[0..4], &[0x41, 0x4b, 0xf3, 0x89]); // exactInputSingle
    }

    #[test]
    fn build_swap_token_in_requires_approval() {
        let chain = mainnet();
        let quote = BestQuote { version: Version::V2, fee: 0, amount_out: U256::from(1u64) };
        let swap = build_swap(&chain, &quote, USDC, WETH, U256::from(1_000u64), U256::from(1u64), ALICE, U256::from(99u64)).unwrap();
        assert_eq!(swap.value, U256::ZERO);
        let (token, spender, data) = swap.approve.expect("erc20 input needs approval");
        assert_eq!(token, USDC);
        assert_eq!(spender, swap.router);
        assert_eq!(&data[0..4], &[0x09, 0x5e, 0xa7, 0xb3]);
    }
}
