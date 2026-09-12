//! Per-chain Uniswap deployment addresses. Multi-chain and configurable: the
//! module ships sensible defaults (Ethereum, Sepolia, Optimism, Arbitrum, Base) and a
//! `configure` method can add/override any chain. Addresses are checksummed
//! strings; the pricing/swap code parses them.
//!
//! `v3Router` is **SwapRouter02** everywhere: the swap encoder wraps every V3 swap in its
//! `multicall(deadline, …)`, which the legacy SwapRouter does not have. A chain configured
//! with the legacy router will revert every V3 swap.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Stablecoins are assumed to use 6 decimals (USDC/USDT on every seeded chain).
pub const STABLE_DECIMALS: u8 = 6;

/// Uniswap addresses for one chain. Any optional field that is `None` simply
/// disables that version's pricing/swaps on that chain.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChainUniswap {
    pub chain_id: u64,
    pub weth: String,
    /// Stablecoins used to express token→USD (first that prices wins). USDC, USDT.
    pub stablecoins: Vec<String>,
    pub multicall3: String,

    // Uniswap V2 (constant product). Pair address is CREATE2(factory, salt, initHash).
    #[serde(default)]
    pub v2_factory: Option<String>,
    #[serde(default)]
    pub v2_init_code_hash: Option<String>,
    #[serde(default)]
    pub v2_router: Option<String>,

    // Uniswap V3 (concentrated liquidity). Pool is CREATE2(factory, salt, initHash).
    #[serde(default)]
    pub v3_factory: Option<String>,
    #[serde(default)]
    pub v3_init_code_hash: Option<String>,
    /// QuoterV2.
    #[serde(default)]
    pub v3_quoter: Option<String>,
    /// SwapRouter02, never the legacy SwapRouter (see the module doc).
    #[serde(default)]
    pub v3_router: Option<String>,
    #[serde(default = "default_v3_fees")]
    pub v3_fee_tiers: Vec<u32>,

    // Uniswap V4 (singleton). Reads via StateView.getSlot0(poolId).
    #[serde(default)]
    pub v4_state_view: Option<String>,
    #[serde(default)]
    pub v4_quoter: Option<String>,
    /// Fee/tickSpacing pairs to probe for V4 pools (hooks = address(0)).
    #[serde(default = "default_v4_pools")]
    pub v4_fee_tick: Vec<(u32, i32)>,
}

fn default_v3_fees() -> Vec<u32> {
    vec![100, 500, 3000, 10000]
}

fn default_v4_pools() -> Vec<(u32, i32)> {
    vec![(500, 10), (3000, 60), (10000, 200)]
}

// Canonical, chain-independent hashes for the original Uniswap deployments.
const V2_INIT_HASH: &str = "0x96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f";
const V3_INIT_HASH: &str = "0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54";
const V3_FACTORY: &str = "0x1F98431c8aD98523631AE4a59f267346ea31F984";
const MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";
/// QuoterV2 and SwapRouter02 sit at the same addresses on Ethereum, Optimism and Arbitrum.
const QUOTER_V2: &str = "0x61fFE014bA17989E743c5F6cB21bF9697530B21e";
const SWAP_ROUTER_02: &str = "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45";

fn base(chain_id: u64, weth: &str, usdc: &str, usdt: Option<&str>) -> ChainUniswap {
    let mut stablecoins = vec![usdc.to_string()];
    if let Some(t) = usdt {
        stablecoins.push(t.to_string());
    }
    ChainUniswap {
        chain_id,
        weth: weth.to_string(),
        stablecoins,
        multicall3: MULTICALL3.to_string(),
        v2_factory: None,
        v2_init_code_hash: None,
        v2_router: None,
        v3_factory: Some(V3_FACTORY.to_string()),
        v3_init_code_hash: Some(V3_INIT_HASH.to_string()),
        v3_quoter: None,
        v3_router: None,
        v3_fee_tiers: default_v3_fees(),
        v4_state_view: None,
        v4_quoter: None,
        v4_fee_tick: default_v4_pools(),
    }
}

/// The default deployments. Configurable at runtime via `configure`.
pub fn default_chains() -> HashMap<u64, ChainUniswap> {
    let mut m = HashMap::new();

    // ── Ethereum mainnet ──
    let mut eth = base(
        1,
        "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
        Some("0xdAC17F958D2ee523a2206206994597C13D831ec7"),
    );
    eth.v2_factory = Some("0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f".into());
    eth.v2_init_code_hash = Some(V2_INIT_HASH.into());
    eth.v2_router = Some("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D".into());
    eth.v3_quoter = Some(QUOTER_V2.into());
    eth.v3_router = Some(SWAP_ROUTER_02.into());
    eth.v4_state_view = Some("0x7fFE42C4a5DEeA5b0feC41C94C136Cf115597227".into());
    eth.v4_quoter = Some("0x52F0E24D1c21C8A0cB1e5a5dD6198556BD9E1203".into());
    m.insert(1, eth);

    // ── Sepolia ── the wallet's default testnet. Uniswap's own deployment: the canonical
    // V2 and V3 init-code hashes reproduce its pairs and pools (see the tests).
    let mut sep = base(
        11155111,
        "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14",
        "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238",
        None,
    );
    sep.v2_factory = Some("0xF62c03E08ada871A0bEb309762E260a7a6a880E6".into());
    sep.v2_init_code_hash = Some(V2_INIT_HASH.into());
    sep.v2_router = Some("0xeE567Fe1712Faf6149d80dA1E6934E354124CfE3".into());
    sep.v3_factory = Some("0x0227628f3F023bb0B980b67D528571c95c6DaC1c".into());
    sep.v3_quoter = Some("0xEd1f6473345F45b75F8179591dd5bA1888cf2FB3".into());
    sep.v3_router = Some("0x3bFA4769FB09eefC5a80d6E87c3B9C650f7Ae48E".into());
    m.insert(11155111, sep);

    // ── Optimism ──
    let mut op = base(
        10,
        "0x4200000000000000000000000000000000000006",
        "0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85",
        Some("0x94b008aA00579c1307B0EF2c499aD98a8ce58e58"),
    );
    op.v3_quoter = Some(QUOTER_V2.into());
    op.v3_router = Some(SWAP_ROUTER_02.into());
    m.insert(10, op);

    // ── Arbitrum One ──
    let mut arb = base(
        42161,
        "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1",
        "0xaf88d065e77c8cC2239327C5EDb3A432268e5831",
        Some("0xFd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9"),
    );
    arb.v3_quoter = Some(QUOTER_V2.into());
    arb.v3_router = Some(SWAP_ROUTER_02.into());
    m.insert(42161, arb);

    // ── Base (different V3 factory; its router below is SwapRouter02 too) ──
    let mut basec = base(
        8453,
        "0x4200000000000000000000000000000000000006",
        "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
        None,
    );
    basec.v3_factory = Some("0x33128a8fC17869897dcE68Ed026d694621f6FDfD".into());
    basec.v3_quoter = Some("0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a".into());
    basec.v3_router = Some("0x2626664c2603336E57B271c5C0b26F421741e481".into());
    m.insert(8453, basec);

    m
}

/// Per-chain Uniswap config: the seeded [`default_chains`] merged with any
/// runtime overrides, persisted as one JSON map in the module's instance dir.
pub struct ConfigStore {
    chains: HashMap<u64, ChainUniswap>,
    path: Option<PathBuf>,
}

impl ConfigStore {
    /// Defaults only (no persistence) — used in tests.
    pub fn defaults() -> Self {
        ConfigStore { chains: default_chains(), path: None }
    }

    /// Defaults overlaid with whatever overrides were persisted at `path`.
    pub fn with_path(path: PathBuf) -> Self {
        let mut chains = default_chains();
        if let Some(overrides) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<HashMap<u64, ChainUniswap>>(&t).ok())
        {
            chains.extend(overrides);
        }
        ConfigStore { chains, path: Some(path) }
    }

    pub fn chain(&self, chain_id: u64) -> Option<&ChainUniswap> {
        self.chains.get(&chain_id)
    }

    /// Add or replace a chain's config and persist the full map.
    pub fn set_chain(&mut self, c: ChainUniswap) {
        self.chains.insert(c.chain_id, c);
        self.save();
    }

    /// All configured chains, ascending by id (stable output for `get_chains`).
    pub fn all(&self) -> Vec<ChainUniswap> {
        let mut v: Vec<ChainUniswap> = self.chains.values().cloned().collect();
        v.sort_by_key(|c| c.chain_id);
        v
    }

    fn save(&self) {
        if let Some(p) = &self.path {
            let _ = std::fs::write(p, serde_json::to_string_pretty(&self.chains).unwrap_or_default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_the_seeded_chains() {
        let m = default_chains();
        for id in [1u64, 11155111, 10, 42161, 8453] {
            assert!(m.contains_key(&id), "missing chain {id}");
            assert!(m[&id].v3_factory.is_some());
            assert!(!m[&id].stablecoins.is_empty());
        }
        // V2 is seeded where Uniswap deployed it: mainnet and Sepolia.
        assert!(m[&1].v2_factory.is_some());
        assert!(m[&11155111].v2_router.is_some());
        assert!(m[&10].v2_factory.is_none());
    }

    #[test]
    fn every_seeded_v3_router_is_swaprouter02() {
        let m = default_chains();
        for id in [1u64, 10, 42161] {
            assert_eq!(m[&id].v3_router.as_deref(), Some(SWAP_ROUTER_02));
            assert_eq!(m[&id].v3_quoter.as_deref(), Some(QUOTER_V2));
        }
        assert_eq!(m[&8453].v3_router.as_deref(), Some("0x2626664c2603336E57B271c5C0b26F421741e481"));
        assert_eq!(m[&11155111].v3_router.as_deref(), Some("0x3bFA4769FB09eefC5a80d6E87c3B9C650f7Ae48E"));
        // The legacy SwapRouter has no multicall(deadline, …) and must not be seeded anywhere.
        assert!(m.values().all(|c| c.v3_router.as_deref() != Some("0xE592427A0AEce92De3Edee1F18E0157C05861564")));
    }

    /// Read off the chain on 2026-09-11 (`factory.getPool` / `factory.getPair` on a public
    /// Sepolia node): the canonical hashes derive exactly these addresses, so the seed may
    /// carry them and every pool read on Sepolia goes to a real pool.
    #[test]
    fn sepolia_pools_derive_from_the_canonical_hashes() {
        use crate::pricing::{parse_addr, parse_b256, v2_pair_address, v3_pool_address};
        let sep = &default_chains()[&11155111];
        let usdc = parse_addr(&sep.stablecoins[0]).unwrap();
        let weth = parse_addr(&sep.weth).unwrap();
        let v3 = v3_pool_address(
            parse_addr(sep.v3_factory.as_deref().unwrap()).unwrap(),
            parse_b256(sep.v3_init_code_hash.as_deref().unwrap()).unwrap(),
            usdc,
            weth,
            500,
        );
        assert_eq!(format!("{v3}"), "0x3289680dD4d6C10bb19b899729cda5eEF58AEfF1");
        let v2 = v2_pair_address(
            parse_addr(sep.v2_factory.as_deref().unwrap()).unwrap(),
            parse_b256(sep.v2_init_code_hash.as_deref().unwrap()).unwrap(),
            usdc,
            weth,
        );
        assert_eq!(format!("{v2}"), "0x72e46e15ef83c896de44B1874B4AF7dDAB5b4F74");
    }
}
