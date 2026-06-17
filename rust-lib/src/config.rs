//! Per-chain Uniswap deployment addresses. Multi-chain and configurable: the
//! module ships sensible defaults (Ethereum, Optimism, Arbitrum, Base) and a
//! `configure` method can add/override any chain. Addresses are checksummed
//! strings; the pricing/swap code parses them.

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
    #[serde(default)]
    pub v3_quoter: Option<String>,
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
    eth.v3_quoter = Some("0x61fFE014bA17989E743c5F6cB21bF9697530B21e".into()); // QuoterV2
    eth.v3_router = Some("0xE592427A0AEce92De3Edee1F18E0157C05861564".into()); // SwapRouter
    eth.v4_state_view = Some("0x7fFE42C4a5DEeA5b0feC41C94C136Cf115597227".into());
    eth.v4_quoter = Some("0x52F0E24D1c21C8A0cB1e5a5dD6198556BD9E1203".into());
    m.insert(1, eth);

    // ── Optimism ──
    let mut op = base(
        10,
        "0x4200000000000000000000000000000000000006",
        "0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85",
        Some("0x94b008aA00579c1307B0EF2c499aD98a8ce58e58"),
    );
    op.v3_quoter = Some("0x61fFE014bA17989E743c5F6cB21bF9697530B21e".into());
    op.v3_router = Some("0xE592427A0AEce92De3Edee1F18E0157C05861564".into());
    m.insert(10, op);

    // ── Arbitrum One ──
    let mut arb = base(
        42161,
        "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1",
        "0xaf88d065e77c8cC2239327C5EDb3A432268e5831",
        Some("0xFd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9"),
    );
    arb.v3_quoter = Some("0x61fFE014bA17989E743c5F6cB21bF9697530B21e".into());
    arb.v3_router = Some("0xE592427A0AEce92De3Edee1F18E0157C05861564".into());
    m.insert(42161, arb);

    // ── Base (different V3 factory) ──
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
        for id in [1u64, 10, 42161, 8453] {
            assert!(m.contains_key(&id), "missing chain {id}");
            assert!(m[&id].v3_factory.is_some());
            assert!(!m[&id].stablecoins.is_empty());
        }
        // V2 only seeded on mainnet by default (configurable elsewhere).
        assert!(m[&1].v2_factory.is_some());
        assert!(m[&10].v2_factory.is_none());
    }
}
