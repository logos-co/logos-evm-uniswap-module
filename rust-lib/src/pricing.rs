//! Uniswap pricing core — V2/V3/V4, batched via Multicall3, best-rate selection.
//!
//! This module is **pure** (no network, no keys): it derives pool addresses with
//! CREATE2, ABI-encodes the on-chain reads, and decodes + price-maths the
//! results. The actual `eth_call` is issued by `eth_rpc_module` from the glue —
//! we only build the calldata and interpret the bytes, so it is all unit-tested
//! with `cargo test`.
//!
//! Pricing strategy (per the wallet's market view): for each token we read every
//! configured pool against WETH (V2 reserves, V3 `slot0`, V4 `getSlot0`), bundle
//! them all into **one** Multicall3 `aggregate3`, then pick the deepest pool
//! (most WETH locked) as the token's ETH price. Token→USD is then
//! `eth_per_token / eth_per_stablecoin` using a configured stablecoin.

use std::collections::HashMap;

use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};

use crate::config::ChainUniswap;

/// WETH always has 18 decimals; the native-ETH side of a V4 pool likewise.
pub const WETH_DECIMALS: u8 = 18;

sol! {
    #[allow(missing_docs)]
    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    }
    #[allow(missing_docs)]
    interface IUniswapV3PoolState {
        function slot0() external view returns (
            uint160 sqrtPriceX96, int24 tick, uint16 observationIndex,
            uint16 observationCardinality, uint16 observationCardinalityNext,
            uint8 feeProtocol, bool unlocked);
    }
    #[allow(missing_docs)]
    interface IStateView {
        function getSlot0(bytes32 poolId) external view returns (uint160 sqrtPriceX96, int24 tick, uint24 protocolFee, uint24 lpFee);
        function getLiquidity(bytes32 poolId) external view returns (uint128 liquidity);
    }
    #[allow(missing_docs)]
    interface IERC20Bal {
        function balanceOf(address owner) external view returns (uint256);
    }

    #[allow(missing_docs)]
    struct Call3 { address target; bool allowFailure; bytes callData; }
    #[allow(missing_docs)]
    struct Result3 { bool success; bytes returnData; }
    #[allow(missing_docs)]
    interface IMulticall3 {
        function aggregate3(Call3[] calls) external payable returns (Result3[] returnData);
    }
}

// ── Multicall3 batching ──────────────────────────────────────────────────────

/// Encode `aggregate3` over `(target, callData)` pairs, all `allowFailure=true`
/// (one reverting read — e.g. a non-existent pool — won't sink the batch).
pub fn multicall3_aggregate3_calldata(calls: &[(Address, Vec<u8>)]) -> Vec<u8> {
    use alloy::primitives::Bytes;
    let calls3: Vec<Call3> = calls
        .iter()
        .map(|(t, d)| Call3 { target: *t, allowFailure: true, callData: Bytes::from(d.clone()) })
        .collect();
    IMulticall3::aggregate3Call { calls: calls3 }.abi_encode()
}

/// Decode an `aggregate3` return into per-call `returnData` (`None` = reverted).
pub fn decode_aggregate3_returns(data: &[u8]) -> Option<Vec<Option<Vec<u8>>>> {
    let decoded = IMulticall3::aggregate3Call::abi_decode_returns(data).ok()?;
    Some(
        decoded
            .into_iter()
            .map(|r| if r.success { Some(r.returnData.to_vec()) } else { None })
            .collect(),
    )
}

// ── CREATE2 pool / poolId derivation ─────────────────────────────────────────

/// Sort two tokens the way Uniswap does (ascending by address) → (token0, token1).
pub fn sort_tokens(a: Address, b: Address) -> (Address, Address) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Uniswap V2 pair address: `CREATE2(factory, keccak256(token0 ++ token1), initHash)`.
pub fn v2_pair_address(factory: Address, init_hash: B256, a: Address, b: Address) -> Address {
    let (t0, t1) = sort_tokens(a, b);
    let mut packed = Vec::with_capacity(40);
    packed.extend_from_slice(t0.as_slice());
    packed.extend_from_slice(t1.as_slice());
    let salt = keccak256(&packed);
    factory.create2(salt, init_hash)
}

/// Uniswap V3 pool address: `CREATE2(factory, keccak256(abi.encode(token0, token1, fee)), initHash)`.
pub fn v3_pool_address(factory: Address, init_hash: B256, a: Address, b: Address, fee: u32) -> Address {
    let (t0, t1) = sort_tokens(a, b);
    let salt = keccak256((t0, t1, U256::from(fee)).abi_encode());
    factory.create2(salt, init_hash)
}

/// Uniswap V4 pool id: `keccak256(abi.encode(PoolKey{c0, c1, fee, tickSpacing, hooks}))`.
/// V4 trades native currencies, so the WETH side is native ETH (`address(0)`),
/// which always sorts first. `hooks` defaults to `address(0)` (vanilla pools).
pub fn v4_pool_id(a: Address, b: Address, fee: u32, tick_spacing: i32, hooks: Address) -> B256 {
    let (c0, c1) = sort_tokens(a, b);
    keccak256((c0, c1, U256::from(fee), U256::from(tick_spacing.max(0) as u64), hooks).abi_encode())
}

// ── Price math ───────────────────────────────────────────────────────────────

fn u256_to_f64(v: U256) -> f64 {
    // Values here (reserves, sqrtPriceX96, liquidity) are < 2^192, well inside
    // f64's range; the ~15-digit mantissa is ample for a display price.
    v.to_string().parse().unwrap_or(f64::INFINITY)
}

/// Read a 32-byte word from an ABI return as a `U256` (right-aligned values:
/// uint112/uint160/uint128/uint256 all live in the low bytes of their word).
fn word(data: &[u8], i: usize) -> Option<U256> {
    let start = i * 32;
    data.get(start..start + 32).map(U256::from_be_slice)
}

/// Convert a raw token1/token0 ratio into a human one, applying decimals.
fn human_price1_per0(raw_ratio: f64, dec0: u8, dec1: u8) -> f64 {
    raw_ratio * 10f64.powi(dec0 as i32 - dec1 as i32)
}

/// `sqrtPriceX96` → raw token1/token0 ratio = `(sqrtPriceX96 / 2^96)^2`.
fn sqrt_price_to_raw_ratio(sqrt_price_x96: U256) -> f64 {
    let ratio = u256_to_f64(sqrt_price_x96) / 2f64.powi(96);
    ratio * ratio
}

/// Given the human token1-per-token0 price and whether the priced token is
/// token0, return WETH-per-token (the token's price denominated in ETH).
fn eth_per_token(price1_per0_human: f64, token_is_token0: bool) -> f64 {
    if token_is_token0 {
        price1_per0_human
    } else if price1_per0_human != 0.0 {
        1.0 / price1_per0_human
    } else {
        0.0
    }
}

// ── Batched, multi-version pricing ───────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    V2,
    V3,
    V4,
}

/// One pool we will read in the multicall. `n_calls` is how many entries it
/// consumes from the aggregate result (V2: just reserves; V3/V4: price + depth).
#[derive(Clone, Debug)]
pub struct Candidate {
    pub token: Address,
    pub version: Version,
    pub fee: u32,
    pub token_is_token0: bool,
    pub dec0: u8,
    pub dec1: u8,
    pub n_calls: usize,
}

/// A built batch: the flat Multicall3 sub-calls plus the parallel candidates that
/// describe how to decode the (same-order) results.
pub struct PricingBatch {
    pub calls: Vec<(Address, Vec<u8>)>,
    pub candidates: Vec<Candidate>,
}

/// One successfully-priced pool observation.
#[derive(Clone, Copy, Debug)]
pub struct PricePoint {
    pub eth_per_token: f64,
    /// Pool depth used to choose the best quote: human WETH locked (V2/V3) or
    /// in-range liquidity (V4, only compared against other V4 points).
    pub weight: f64,
    pub is_v4: bool,
}

/// Build the full pricing batch for a chain: for every `(token, decimals)` (which
/// should already exclude WETH), enumerate its V2/V3/V4 pools against WETH and
/// emit the reads. Issue `calls` as one Multicall3, then feed the results to
/// [`decode_prices`].
pub fn build_pricing_batch(chain: &ChainUniswap, weth: Address, tokens: &[(Address, u8)]) -> PricingBatch {
    let mut calls: Vec<(Address, Vec<u8>)> = Vec::new();
    let mut candidates: Vec<Candidate> = Vec::new();

    let v2_factory = chain.v2_factory.as_deref().and_then(parse_addr);
    let v2_hash = chain.v2_init_code_hash.as_deref().and_then(parse_b256);
    let v3_factory = chain.v3_factory.as_deref().and_then(parse_addr);
    let v3_hash = chain.v3_init_code_hash.as_deref().and_then(parse_b256);
    let v4_state_view = chain.v4_state_view.as_deref().and_then(parse_addr);

    for &(token, dec) in tokens {
        if token == weth {
            continue;
        }
        let token_is_token0 = token < weth;
        let (t0_dec, t1_dec) = if token_is_token0 { (dec, WETH_DECIMALS) } else { (WETH_DECIMALS, dec) };

        // ── V2: a single getReserves gives both the price and the WETH depth ──
        if let (Some(factory), Some(hash)) = (v2_factory, v2_hash) {
            let pool = v2_pair_address(factory, hash, token, weth);
            calls.push((pool, IUniswapV2Pair::getReservesCall {}.abi_encode()));
            candidates.push(Candidate {
                token,
                version: Version::V2,
                fee: 0,
                token_is_token0,
                dec0: t0_dec,
                dec1: t1_dec,
                n_calls: 1,
            });
        }

        // ── V3: slot0 for the price + WETH.balanceOf(pool) for the depth ──
        if let (Some(factory), Some(hash)) = (v3_factory, v3_hash) {
            for &fee in &chain.v3_fee_tiers {
                let pool = v3_pool_address(factory, hash, token, weth, fee);
                calls.push((pool, IUniswapV3PoolState::slot0Call {}.abi_encode()));
                calls.push((weth, IERC20Bal::balanceOfCall { owner: pool }.abi_encode()));
                candidates.push(Candidate {
                    token,
                    version: Version::V3,
                    fee,
                    token_is_token0,
                    dec0: t0_dec,
                    dec1: t1_dec,
                    n_calls: 2,
                });
            }
        }

        // ── V4: native ETH side (address(0)) → token sorts second ──
        if let Some(state_view) = v4_state_view {
            // For V4 the WETH side is native ETH (0x0), which always sorts first.
            let token_is_token0_v4 = false;
            for &(fee, tick) in &chain.v4_fee_tick {
                let pool_id = v4_pool_id(Address::ZERO, token, fee, tick, Address::ZERO);
                calls.push((state_view, IStateView::getSlot0Call { poolId: pool_id }.abi_encode()));
                calls.push((state_view, IStateView::getLiquidityCall { poolId: pool_id }.abi_encode()));
                candidates.push(Candidate {
                    token,
                    version: Version::V4,
                    fee,
                    token_is_token0: token_is_token0_v4,
                    dec0: WETH_DECIMALS,
                    dec1: dec,
                    n_calls: 2,
                });
            }
        }
    }

    PricingBatch { calls, candidates }
}

/// Decode one candidate's slice of the aggregate results into a [`PricePoint`].
fn decode_candidate(c: &Candidate, rets: &[Option<Vec<u8>>]) -> Option<PricePoint> {
    match c.version {
        Version::V2 => {
            let data = rets.first()?.as_ref()?;
            let r0 = word(data, 0)?;
            let r1 = word(data, 1)?;
            if r0.is_zero() || r1.is_zero() {
                return None;
            }
            let raw_ratio = u256_to_f64(r1) / u256_to_f64(r0); // token1 per token0 (raw)
            let price = eth_per_token(human_price1_per0(raw_ratio, c.dec0, c.dec1), c.token_is_token0);
            // WETH side reserve = token1 if token is token0, else token0.
            let weth_raw = if c.token_is_token0 { r1 } else { r0 };
            let weight = u256_to_f64(weth_raw) / 10f64.powi(WETH_DECIMALS as i32);
            finite_point(price, weight, false)
        }
        Version::V3 => {
            let slot0 = rets.first()?.as_ref()?;
            let sqrt_price = word(slot0, 0)?;
            if sqrt_price.is_zero() {
                return None;
            }
            let raw_ratio = sqrt_price_to_raw_ratio(sqrt_price);
            let price = eth_per_token(human_price1_per0(raw_ratio, c.dec0, c.dec1), c.token_is_token0);
            // Depth = WETH the pool actually holds (balanceOf in the 2nd slot).
            let weight = rets
                .get(1)
                .and_then(|o| o.as_ref())
                .and_then(|d| word(d, 0))
                .map(|w| u256_to_f64(w) / 10f64.powi(WETH_DECIMALS as i32))
                .unwrap_or(0.0);
            finite_point(price, weight, false)
        }
        Version::V4 => {
            let slot0 = rets.first()?.as_ref()?;
            let sqrt_price = word(slot0, 0)?;
            if sqrt_price.is_zero() {
                return None;
            }
            let raw_ratio = sqrt_price_to_raw_ratio(sqrt_price);
            let price = eth_per_token(human_price1_per0(raw_ratio, c.dec0, c.dec1), c.token_is_token0);
            let weight = rets
                .get(1)
                .and_then(|o| o.as_ref())
                .and_then(|d| word(d, 0))
                .map(u256_to_f64)
                .unwrap_or(0.0);
            finite_point(price, weight, true)
        }
    }
}

fn finite_point(price: f64, weight: f64, is_v4: bool) -> Option<PricePoint> {
    if price.is_finite() && price > 0.0 {
        Some(PricePoint { eth_per_token: price, weight, is_v4 })
    } else {
        None
    }
}

/// Across a token's pool observations pick the best ETH price: prefer V2/V3
/// (directly comparable WETH depth) by deepest liquidity, and only fall back to
/// V4 (whose weight is in-range liquidity units) when no V2/V3 pool priced.
pub fn pick_best(points: &[PricePoint]) -> Option<f64> {
    let best_non_v4 = points
        .iter()
        .filter(|p| !p.is_v4)
        .max_by(|a, b| a.weight.total_cmp(&b.weight));
    if let Some(p) = best_non_v4 {
        return Some(p.eth_per_token);
    }
    points
        .iter()
        .filter(|p| p.is_v4)
        .max_by(|a, b| a.weight.total_cmp(&b.weight))
        .map(|p| p.eth_per_token)
}

/// Decode the Multicall3 aggregate results into one ETH price per token.
/// `results` must be the per-sub-call return data, in the same order as
/// `batch.calls` (i.e. the output of `decode_aggregate3_returns`).
pub fn decode_prices(batch: &PricingBatch, results: &[Option<Vec<u8>>]) -> HashMap<Address, f64> {
    let mut per_token: HashMap<Address, Vec<PricePoint>> = HashMap::new();
    let mut idx = 0usize;
    for c in &batch.candidates {
        let end = (idx + c.n_calls).min(results.len());
        let slice = &results[idx.min(results.len())..end];
        if let Some(point) = decode_candidate(c, slice) {
            per_token.entry(c.token).or_default().push(point);
        }
        idx += c.n_calls;
    }
    per_token
        .into_iter()
        .filter_map(|(t, points)| pick_best(&points).map(|p| (t, p)))
        .collect()
}

/// Express tokens in USD using a stablecoin's own ETH price: a token is worth
/// `eth_per_token / eth_per_stable` stablecoins. WETH (`weth`) is added at its
/// own ETH price (1.0). Returns USD price per token; tokens with no priced
/// stablecoin available are omitted.
pub fn token_usd_prices(
    eth_prices: &HashMap<Address, f64>,
    weth: Address,
    stablecoins: &[Address],
) -> HashMap<Address, f64> {
    // First stablecoin that itself priced against ETH anchors USD.
    let stable_eth = stablecoins
        .iter()
        .find_map(|s| eth_prices.get(s).copied().filter(|p| *p > 0.0));
    let mut out = HashMap::new();
    let Some(stable_eth) = stable_eth else {
        return out;
    };
    out.insert(weth, 1.0 / stable_eth); // WETH in USD = 1 ETH / (ETH per stable)
    for (t, ep) in eth_prices {
        if *ep > 0.0 {
            out.insert(*t, ep / stable_eth);
        }
    }
    out
}

// ── parsing helpers (checksummed strings → typed) ────────────────────────────

pub fn parse_addr(s: &str) -> Option<Address> {
    s.parse().ok()
}

pub fn parse_b256(s: &str) -> Option<B256> {
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const V3_FACTORY: Address = address!("1F98431c8aD98523631AE4a59f267346ea31F984");
    const V2_FACTORY: Address = address!("5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f");

    fn v2_hash() -> B256 {
        "0x96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f".parse().unwrap()
    }
    fn v3_hash() -> B256 {
        "0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54".parse().unwrap()
    }

    #[test]
    fn v3_pool_address_matches_known_mainnet_pool() {
        // Canonical USDC/WETH 0.05% pool.
        let pool = v3_pool_address(V3_FACTORY, v3_hash(), USDC, WETH, 500);
        assert_eq!(pool, address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"));
    }

    #[test]
    fn v2_pair_address_matches_known_mainnet_pair() {
        // Canonical USDC/WETH V2 pair.
        let pair = v2_pair_address(V2_FACTORY, v2_hash(), USDC, WETH);
        assert_eq!(pair, address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"));
    }

    #[test]
    fn token_ordering_is_symmetric() {
        assert_eq!(sort_tokens(USDC, WETH), (USDC, WETH));
        assert_eq!(sort_tokens(WETH, USDC), (USDC, WETH));
        assert_eq!(v3_pool_address(V3_FACTORY, v3_hash(), WETH, USDC, 500), v3_pool_address(V3_FACTORY, v3_hash(), USDC, WETH, 500));
    }

    #[test]
    fn v4_pool_id_is_deterministic_and_order_independent() {
        let a = v4_pool_id(Address::ZERO, USDC, 500, 10, Address::ZERO);
        let b = v4_pool_id(USDC, Address::ZERO, 500, 10, Address::ZERO);
        assert_eq!(a, b);
        // A different fee tier yields a different id.
        assert_ne!(a, v4_pool_id(Address::ZERO, USDC, 3000, 60, Address::ZERO));
    }

    #[test]
    fn v3_price_math_recovers_a_known_price() {
        // Target: 1 WETH = 3000 USDC, so 1 USDC = 1/3000 ETH.
        // token0=USDC(6), token1=WETH(18). raw_ratio = (1/3000) * 10^(18-6).
        let target_eth_per_usdc = 1.0 / 3000.0;
        let raw_ratio = target_eth_per_usdc * 10f64.powi(18 - 6);
        let sqrt_price_x96 = (raw_ratio.sqrt() * 2f64.powi(96)) as u128;
        let sp = U256::from(sqrt_price_x96);

        let recovered = sqrt_price_to_raw_ratio(sp);
        let human = human_price1_per0(recovered, 6, 18); // WETH per USDC (token is token0)
        let eth = eth_per_token(human, true);
        assert!((eth - target_eth_per_usdc).abs() / target_eth_per_usdc < 0.001, "eth={eth}");
    }

    #[test]
    fn v2_reserves_price_and_depth() {
        // token0=USDC(6) reserve 6,000,000 USDC ; token1=WETH(18) reserve 2000 WETH
        // → 1 WETH = 3000 USDC → 1 USDC = 1/3000 ETH ; depth = 2000 WETH.
        let r0 = U256::from(6_000_000u64) * U256::from(10u64).pow(U256::from(6));
        let r1 = U256::from(2_000u64) * U256::from(10u64).pow(U256::from(18));
        let mut data = vec![0u8; 96];
        data[..32].copy_from_slice(&r0.to_be_bytes::<32>());
        data[32..64].copy_from_slice(&r1.to_be_bytes::<32>());

        let c = Candidate { token: USDC, version: Version::V2, fee: 0, token_is_token0: true, dec0: 6, dec1: 18, n_calls: 1 };
        let p = decode_candidate(&c, &[Some(data)]).unwrap();
        assert!((p.eth_per_token - 1.0 / 3000.0).abs() / (1.0 / 3000.0) < 1e-6);
        assert!((p.weight - 2000.0).abs() < 1e-6);
        assert!(!p.is_v4);
    }

    #[test]
    fn pick_best_prefers_deepest_non_v4() {
        let points = vec![
            PricePoint { eth_per_token: 0.00033, weight: 10.0, is_v4: false },
            PricePoint { eth_per_token: 0.00040, weight: 500.0, is_v4: false }, // deepest
            PricePoint { eth_per_token: 0.00031, weight: 9e18, is_v4: true },   // ignored when non-v4 exists
        ];
        assert_eq!(pick_best(&points), Some(0.00040));

        // Only V4 → falls back to deepest V4.
        let only_v4 = vec![
            PricePoint { eth_per_token: 0.00031, weight: 1.0, is_v4: true },
            PricePoint { eth_per_token: 0.00032, weight: 5.0, is_v4: true },
        ];
        assert_eq!(pick_best(&only_v4), Some(0.00032));
    }

    #[test]
    fn token_usd_uses_stablecoin_anchor() {
        let mut eth = HashMap::new();
        eth.insert(USDC, 1.0 / 3000.0); // 1 USDC = 1/3000 ETH
        let token = address!("1111111111111111111111111111111111111111");
        eth.insert(token, 1.0 / 1500.0); // worth 2 USDC
        let usd = token_usd_prices(&eth, WETH, &[USDC]);
        assert!((usd[&token] - 2.0).abs() < 1e-6, "got {}", usd[&token]);
        assert!((usd[&USDC] - 1.0).abs() < 1e-6);
        assert!((usd[&WETH] - 3000.0).abs() < 1e-3); // WETH ≈ $3000
    }

    #[test]
    fn build_batch_enumerates_versions() {
        let chain = crate::config::default_chains();
        let eth = &chain[&1];
        let token = address!("1111111111111111111111111111111111111111");
        let batch = build_pricing_batch(eth, WETH, &[(token, 18)]);
        // mainnet seeds V2 (1 call) + V3 over 4 fee tiers (2 calls each) + V4 over
        // 3 fee/tick pairs (2 calls each) = 1 + 8 + 6 = 15 sub-calls.
        assert_eq!(batch.calls.len(), 15);
        assert!(batch.candidates.iter().any(|c| c.version == Version::V2));
        assert!(batch.candidates.iter().any(|c| c.version == Version::V3));
        assert!(batch.candidates.iter().any(|c| c.version == Version::V4));
        // WETH itself is skipped (no self-pool).
        let weth_batch = build_pricing_batch(eth, WETH, &[(WETH, 18)]);
        assert!(weth_batch.calls.is_empty());
    }
}
