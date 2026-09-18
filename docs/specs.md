# `logos-evm-uniswap-module` — Specification & Reference

> Uniswap **price oracle and swap quoter/encoder** for the Logos multi-chain EVM wallet.
> Derives Uniswap V2/V3/V4 pool addresses **offline** (CREATE2), bundles every
> on-chain read into a **single Multicall3 `eth_call`** issued through
> `eth_rpc_module`, and returns best-rate token→ETH / token→USD prices plus
> V2/V3 swap-transaction building. Multi-chain, configurable, `concurrency:multi`.

---

## 1. Purpose & place in the EVM wallet

`logos-evm-uniswap-module` is a **Rust `cdylib` Logos module** that answers two
questions for the wallet: *"what is this token worth?"* and *"how do I swap it?"*.

It is the wallet's **market-data and routing layer**. It does **no networking and
holds no keys**. Instead it:

1. Derives Uniswap V2 pair / V3 pool / V4 pool-id addresses purely from chain
   config using **CREATE2** (offline, deterministic).
2. ABI-encodes the on-chain reads (V2 `getReserves`, V3 `slot0`, V4
   `StateView.getSlot0`/`getLiquidity`, plus quote calls) and packs them into a
   **single Multicall3 `aggregate3` batch**.
3. Issues that one batch as an `eth_call` **through `eth_rpc_module`** — the only
   way it touches the network — so the wallet's fail-closed SOCKS5 proxy still
   governs every request.
4. Decodes the returned bytes, applies the V2/V3/V4 price math, picks the
   **deepest pool** as the token's ETH price, anchors token→USD on a configured
   stablecoin, and (for swaps) ABI-encodes the winning router calldata.

### Where it sits in the 7-repo wallet

```
logos-evm-wallet-ui                (universal C++ ui_qml app; Market tab)
        │  drives over the Logos bridge
        ▼
logos-evm-wallet-backend-module    (coordinator; calls get_prices / quote_swap /
        │                           build_swap for its Market tab + send pipeline)
        ▼
logos-evm-uniswap-module  ◀── THIS REPO  (price oracle + swap quoter)
        │  module→module: modules().eth_rpc_module.call(chainId, callJson)
        ▼
logos-evm-eth-rpc-module           (multi-chain JSON-RPC transport, fail-closed)
        ▼
logos-evm-net-proxy (library)      (fail-closed SOCKS5 chokepoint, vendored by eth-rpc)
        ▼
the chain's JSON-RPC node
```

This module is the wallet's **market view**: `wallet_backend_module` exposes a
`get_market` that fans out into this module's `get_prices`, and the UI's Market
tab renders the result. Swaps flow `wallet-ui → backend → uniswap.build_swap →`
backend signs/broadcasts (via `keystore_module` + `eth_rpc_module`).

**Direct dependency:** `eth_rpc_module` (declared in `metadata.json`
`dependencies`, wired in `flake.nix`). This module is a **leaf** with respect to
other wallet modules — `keystore`, `token-list`, `wallet-backend` do not call it
in reverse; the backend calls *into* it.

---

## 2. Overall architecture

The crate splits into **three pure cores** (no Logos/Qt dependency, unit-tested
with `cargo test --no-default-features`) and a **glue layer** (behind the default
`logos_module` feature) that wires the cores to the Logos runtime.

```mermaid
flowchart TB
    subgraph consumer["Caller (wallet_backend_module / logoscore -c)"]
        C["configure / get_chains / get_prices / quote_swap / build_swap"]
    end

    subgraph module["uniswap_module (Rust cdylib, concurrency: multi)"]
        direction TB

        subgraph glue["glue.rs — Logos transport + orchestration"]
            TR["UniswapModule trait (the LIDL/codegen contract)\nconfigure · get_chains · get_prices · quote_swap · build_swap"]
            IMPL["UniswapModuleImpl\ncfg: RwLock&lt;Option&lt;ConfigStore&gt;&gt;"]
            RM["run_multicall()\nencode aggregate3 → eth_rpc.call → decode"]
            INST["logos_module_install() → install::&lt;UniswapModuleImpl&gt;()\ngenerated provider_gen.rs (modules(), install(), RustModuleContext)"]
        end

        subgraph cores["pure cores (offline, no network, no keys)"]
            CFG["config.rs\nChainUniswap · ConfigStore\ndefault_chains() (1/10/42161/8453)\npersisted config.json"]
            PRI["pricing.rs\nCREATE2 derivation · sqrtPriceX96/reserve math\nMulticall3 encode/decode · pick_best · token_usd_prices"]
            SWP["swap.rs\nV2 getAmountsOut / V3 QuoterV2 quote\nbest-route select · router calldata + approve"]
        end
    end

    EXT["modules().eth_rpc_module.call(chainId, callJson)\n(the ONLY outbound surface)"]

    C --> TR --> IMPL
    IMPL --> CFG
    IMPL --> RM
    RM --> PRI
    IMPL --> PRI
    IMPL --> SWP
    SWP --> PRI
    RM --> EXT
    INST -.installs.-> TR
```

**Key structural facts**

| Concern | Where | Notes |
|---|---|---|
| Public API contract | `glue.rs` `pub trait UniswapModule` | 5 methods + `on_context_ready` lifecycle hook |
| Transport / codegen | `generated/provider_gen.rs` (built, gitignored) | provides `modules()`, `install::<T>()`, `RustModuleContext`; `include!`d into `glue.rs` |
| Module state | `UniswapModuleImpl.cfg: RwLock<Option<ConfigStore>>` | `None` until `on_context_ready`; read under shared lock, written only by `configure` |
| Config + persistence | `config.rs` | seeded defaults overlaid with persisted overrides at `<instance>/config.json` |
| Price math (pure) | `pricing.rs` | CREATE2, Multicall3, V2/V3/V4 math, best-rate |
| Swap building (pure) | `swap.rs` | V2/V3 quote + router calldata; V4 swaps are a fast-follow |
| Outbound calls | only `run_multicall` → `eth_rpc_module.call` | no other network surface exists |

---

## 3. Communication with dependencies

Every price/quote/swap method ultimately performs **exactly one** Multicall3
`eth_call` through `eth_rpc_module`. The sequence below is the real
`get_prices` path (the same shape applies to `quote_swap`/`build_swap`, which
batch quote calls instead of pool reads).

```mermaid
sequenceDiagram
    autonumber
    participant Caller as Caller<br/>(backend / logoscore)
    participant Uni as uniswap_module<br/>(glue.rs)
    participant Pri as pricing.rs<br/>(pure)
    participant Eth as eth_rpc_module<br/>(modules().eth_rpc_module)
    participant Node as JSON-RPC node<br/>(via net-proxy)

    Caller->>Uni: get_prices(chainId, {tokens:[{address,decimals}]})
    Uni->>Uni: chain_cfg(chainId) under RwLock read → clone, drop lock
    Uni->>Pri: build_pricing_batch(chain, weth, tokens + stablecoins)
    Pri-->>Uni: PricingBatch { calls:[(target,callData)], candidates }
    Note over Uni,Pri: derives V2 pair / V3 pool / V4 poolId via CREATE2 (offline)
    Uni->>Pri: multicall3_aggregate3_calldata(calls)
    Pri-->>Uni: aggregate3 calldata (one blob, allowFailure=true)
    Uni->>Eth: eth_rpc_module.call(chainId,<br/>{ to: multicall3, data: "0x..." })
    Eth->>Node: eth_call (single round-trip, fail-closed via proxy)
    Node-->>Eth: 0x… (aggregate3 Result3[])
    Eth-->>Uni: { ok:true, result:"0x…" }
    Uni->>Pri: decode_aggregate3_returns + decode_prices + token_usd_prices
    Pri-->>Uni: per-token ETH price (deepest pool) + USD (stablecoin anchor)
    Uni-->>Caller: { ok:true, chainId, prices:[{address,eth,usd}, …] }
```

### The module→module contract (exact)

The glue reaches its dependency through the generated typed client:

```rust
// glue.rs :: run_multicall
let call_json = json!({ "to": multicall3, "data": format!("0x{}", hex::encode(data)) }).to_string();
let resp = modules().eth_rpc_module.call(chain_id, &call_json).map_err(|e| e.to_string())?;
let v: Value = serde_json::from_str(&resp)?;
if v.get("ok").and_then(Value::as_bool) == Some(false) {
    return Err(resp);
}
let result_hex = v.get("result").and_then(Value::as_str).ok_or("multicall: no result")?;
```

| Item | Value |
|---|---|
| Dependency module | `eth_rpc_module` (`logos-co/logos-evm-eth-rpc-module`) |
| Method called | `call(chain_id: i64, call_json: String) -> String` |
| `call_json` shape | `{ "to": <multicall3 addr>, "data": "0x<aggregate3 calldata>" }` (an `eth_call`) |
| Success return | `{ "ok": true, "result": "0x<Result3[] bytes>" }` |
| Error return | `{ "ok": false, "error": "<message>" }` (e.g. `RPC_FAILED`, proxy refusal) |
| Chain addressing | by `chainId`; the **caller must have run `eth_rpc_module.set_chain_config(chainId, …)`** first |

> ⚠️ **Do NOT hand-split / hand-parse the `aggregate3` hex.** The whole batch is a
> *single* ABI-encoded `Result3[]` blob — offsets and per-call lengths are encoded
> dynamically. Treat it as one opaque payload and let
> `pricing::decode_aggregate3_returns` (alloy's `abi_decode_returns`) do the
> splitting. Slicing the hex at fixed 32-byte boundaries silently corrupts every
> downstream read (this was a real 256× pricing bug class in the wallet). The
> design is deliberately **one batch in, one blob out, one decoder.**

---

## 4. Full API reference

The public API is the `pub trait UniswapModule` in `rust-lib/src/glue.rs`. Every
method is exposed to other modules and to `logoscore` (`call <module> <method>
[args…]`). Unless noted, the return is a **JSON string**; the error convention is
`{ "ok": false, "error": "<message>" }` (`err()` helper). A refusal from `eth_rpc_module`
is relayed verbatim, not flattened to its `error` text: its `code` and, for
`verified_blocked`, the `verifiedProxy` verdict reach the caller. Native ETH is
written as `"ETH"`, `"native"`, `""`, or `0x000…0`; everything else is a 20-byte
address.

### Method index

| Method | Signature | Returns |
|---|---|---|
| [`configure`](#41-configure) | `configure(chain_json: String) -> bool` | `bool` |
| [`get_chains`](#42-get_chains) | `get_chains() -> String` | `{ ok, chains: [ChainUniswap…] }` |
| [`get_prices`](#43-get_prices) | `get_prices(chain_id: i64, tokens_json: String) -> String` | `{ ok, chainId, prices:[…] }` |
| [`quote_swap`](#44-quote_swap) | `quote_swap(chain_id: i64, params_json: String) -> String` | `{ ok, version, fee, amountOut }` |
| [`build_swap`](#45-build_swap) | `build_swap(chain_id: i64, params_json: String) -> String` | `{ ok, version, fee, router, value, data, amountOut, amountOutMin, approve }` |

Plus the lifecycle hook `on_context_ready(&self, ctx: &RustModuleContext)` (not a
callable RPC) — invoked once by the runtime to load persisted config from
`ctx.instance_persistence_path`.

---

### 4.1 `configure`

```rust
fn configure(&self, chain_json: String) -> bool
```

Add or override one chain's Uniswap config. **The only writer** of module state.

| Param | Type | Meaning |
|---|---|---|
| `chain_json` | `String` (JSON of [`ChainUniswap`](#51-chainuniswap)) | Full per-chain config; `chainId` selects the slot |

- **Returns** `true` on success; `false` if the JSON fails to parse into
  `ChainUniswap` **or** if the config store is not yet initialized
  (`on_context_ready` hasn't run).
- **Side effect:** inserts/replaces the chain in `ConfigStore` and **persists the
  whole map** to `<instance>/config.json` (pretty JSON).

```bash
logoscore call uniswap_module configure @uni_chain.json   # → true
```

---

### 4.2 `get_chains`

```rust
fn get_chains(&self) -> String
```

Return **all** configured chains — the seeded defaults (1, 10, 42161, 8453)
merged with any runtime overrides, **sorted ascending by `chainId`**.

- **Success:** `{ "ok": true, "chains": [ <ChainUniswap>, … ] }`
- **Error:** `{ "ok": false, "error": "uniswap not initialized (context not ready)" }`

```bash
logoscore call uniswap_module get_chains
# → {"ok":true,"chains":[{"chainId":1,"weth":"0xC02a…","stablecoins":[…], …}, …, {"chainId":8453, …}]}
```

---

### 4.3 `get_prices`

```rust
fn get_prices(&self, chain_id: i64, tokens_json: String) -> String
```

The core oracle call. For each requested token it enumerates **every configured
V2/V3/V4 pool against WETH**, batches them into one Multicall3 `eth_call`, and
returns token→ETH and token→USD prices.

| Param | Type | Meaning |
|---|---|---|
| `chain_id` | `i64` | Chain to price on; must be configured here **and** in `eth_rpc_module` |
| `tokens_json` | `String` | `{ "tokens": [ { "address": "0x…", "decimals": 18 } … ] }` — `decimals` defaults to `18` |

**Behaviour**

1. Resolves chain config, WETH, and the configured stablecoins.
2. Prices the user's tokens **plus the stablecoins** (the stablecoin must price
   against ETH to anchor USD).
3. Builds the pricing batch, issues one Multicall3 `eth_call` via `eth_rpc`.
4. `decode_prices` picks the **deepest pool** per token as its ETH price;
   `token_usd_prices` divides by the stablecoin's ETH price for USD.

**Success shape** — note `eth`/`usd` are JSON numbers and may be `null` when no
pool priced / no stablecoin anchored:

```json
{
  "ok": true,
  "chainId": 31337,
  "prices": [
    { "address": "ETH",  "eth": 1.0,                  "usd": 3000.0 },
    { "address": "0xA0b8…eB48", "eth": 0.00033333,    "usd": 1.0 }
  ]
}
```

- The first entry is always native **ETH** (`eth: 1.0`, `usd` = the WETH USD
  price), followed by the user's tokens **in request order**.
- **Error** (bad JSON, unknown chain, bad WETH addr, RPC failure):
  `{ "ok": false, "error": "<message>" }`.

```bash
logoscore call uniswap_module get_prices 31337 @tokens.json
```

---

### 4.4 `quote_swap`

```rust
fn quote_swap(&self, chain_id: i64, params_json: String) -> String
```

The **best route** for `amountIn` of `tokenIn → tokenOut`, in **one Multicall3 round
trip**. Candidates: V2 direct, V2 via WETH, V3 direct on every configured fee tier, and V3
via WETH on every pair of tiers (22 routes on a chain with both versions and four tiers).
Beside every candidate rides a **probe** quote of a thousandth of the amount; the winning
route's probe gives the marginal rate, and the shortfall of the real rate against it is the
**price impact**. With an `owner`, the same batch reads that account's balance of the input
token and its allowance for each router, so the reply also says whether the swap can be
paid for and whether an approval must go first.

| Param | Type | Meaning |
|---|---|---|
| `chain_id` | `i64` | Chain to quote on |
| `params_json` | `String` ([`SwapReq`](#52-swapreq)) | `tokenIn`, `tokenOut`, `amountIn`; `owner` optional |

`amountIn` is decimal digits or `0x`-hex, in base units. Anything else — including `0` —
is **refused** (`"not an amount"`, `"amount must be greater than zero"`), never read as zero.

- **Success:**

  ```json
  { "ok": true, "chainId": 1,
    "tokenIn": "0xA0b8…eB48", "tokenOut": "ETH",
    "amountIn": "1000000000", "amountOut": "333277787035494084",
    "route": { "version": "V3", "viaWeth": false,
               "hops": [ { "tokenIn": "0xA0b8…eB48", "tokenOut": "0xC02a…6Cc2", "fee": 500 } ] },
    "feeBps": 5, "priceImpactBps": 2, "gasLimitHint": 235000,
    "spender": "0x68b3…Fc45",
    "balanceIn": "5000000000000", "allowance": "0",
    "needsApproval": true, "approval": "set",
    "rpcRoute": "direct" }
  ```

  `route.hops` names the ERC-20 legs (WETH stands in for ether). `feeBps` is the pool fee
  summed over the hops (V2: 30 per hop). `priceImpactBps` is `null` when the probe did not
  answer. `gasLimitHint` is a gas limit for the swap leg, generous on purpose (see 6.5).
  `balanceIn`, `allowance`, `needsApproval` and `approval` are `null` without an `owner`;
  `approval` is `none`, `set` (approve exactly `amountIn`) or `resetThenSet` (a non-zero
  allowance too small for the amount is zeroed first — USDT refuses the direct change).
  `rpcRoute` is eth_rpc's own label for how the read was served.
- **Error:** `{ "ok": false, "error": "no route found" }` when no candidate answered, or a
  parse/chain error.

```bash
# 1000 USDC → ETH for an account, best of V2 + V3 tiers, direct or via WETH
logoscore call uniswap_module quote_swap 1 '{"tokenIn":"0xA0b8…eB48","tokenOut":"ETH","amountIn":"1000000000","owner":"0xf39F…2266"}'
```

---

### 4.5 `build_swap`

```rust
fn build_swap(&self, chain_id: i64, params_json: String) -> String
```

The quote (same selection as `quote_swap`) plus the **calls that make the swap, in the
order they must land**, in the shape `tx_sender_module` takes. The module holds no key and
sends nothing; the consumer hands `calls` to the sender, which asks the keystore for one
approval over all of them.

| Field in `params_json` ([`SwapReq`](#52-swapreq)) | Type | Meaning / default |
|---|---|---|
| `tokenIn`, `tokenOut` | string | `"ETH"` = native — **required** |
| `amountIn` | string | base units, decimal or `0x`-hex — **required** |
| `owner` | string (addr) | the account that pays — **required** (`"owner required"`) |
| `recipient` | string (addr) | who receives; defaults to `owner` |
| `slippageBps` | u64 | default `50`; above `5000` refused |
| `amountOutMin` | string | overrides the slippage floor |
| `deadline` | u64 | unix seconds; default now + 30 min |
| `symbolIn`, `symbolOut` | string | only name the calls' labels |

- **Success:** every field of `quote_swap`, plus

  ```json
  { "owner": "0xf39F…2266", "recipient": "0xf39F…2266",
    "amountOutMin": "331611398100316613", "slippageBps": 50, "deadline": 1789200000,
    "calls": [
      { "kind": "approve", "to": "0xA0b8…eB48", "value": "0x0", "data": "0x095ea7b3…",
        "gasLimitHint": 60000, "label": "Approve USDC for Uniswap" },
      { "kind": "swap", "to": "0x68b3…Fc45", "value": "0x0", "data": "0x5ae401dc…",
        "gasLimitHint": 235000, "label": "Swap USDC for ETH on Uniswap V3" } ] }
  ```

  An ether input needs no approval and rides as the swap's `value`. A `resetThenSet`
  approval is two `approve` calls (`0`, then the amount). Approvals are for **exactly the
  amount**: no infinite allowances.
- **Encoding.** V2 routes call the legacy V2 router (`swapExactETHForTokens`,
  `swapExactTokensForETH`, `swapExactTokensForTokens`, deadline inline). V3 routes call
  **SwapRouter02** through `multicall(deadline, bytes[])`, which is where the deadline is
  checked: `exactInputSingle` for one hop, `exactInput` with the packed path
  (`token | fee | token | …`) for two. An ether **output** on V3 is swapped to the router
  itself (`address(2)`) and `unwrapWETH9(amountOutMin, recipient)` in the same multicall
  pays the recipient in ether.
- **Error:** as `quote_swap`, or `"could not build the swap for the best route"` when the
  chain has no router for the winning version.

---

## 5. Configuration & data model

### 5.1 `ChainUniswap`

Per-chain Uniswap deployment, serialized **camelCase** (`config.rs`). Any
optional field left `None` disables that version's pricing/swaps on that chain.

| Field (JSON) | Type | Required | Meaning |
|---|---|---|---|
| `chainId` | `u64` | ✅ | EVM chain id (the map key) |
| `weth` | string (addr) | ✅ | Wrapped-native token; the quote/price denominator |
| `stablecoins` | string[] | ✅ | USD anchors, tried in order (first that prices wins). Assumed **6 decimals** (`STABLE_DECIMALS`) |
| `multicall3` | string (addr) | ✅ | Multicall3 contract (canonical `0xcA11…CA11`) |
| `v2Factory` | string? | — | Enables V2 pricing (CREATE2 pair derivation) |
| `v2InitCodeHash` | string? | — | V2 pair init-code hash |
| `v2Router` | string? | — | Enables V2 quoting/swaps |
| `v3Factory` | string? | — | Enables V3 pricing |
| `v3InitCodeHash` | string? | — | V3 pool init-code hash |
| `v3Quoter` | string? | — | QuoterV2 for V3 quotes/swaps |
| `v3Router` | string? | — | **SwapRouter02** — V3 swaps go through its `multicall(deadline, …)` |
| `v3FeeTiers` | `u32[]` | default `[100,500,3000,10000]` | Fee tiers probed for V3 pools |
| `v4StateView` | string? | — | Enables V4 pricing (`getSlot0`/`getLiquidity`) |
| `v4Quoter` | string? | — | V4 quoter (reserved; V4 swaps fast-follow) |
| `v4FeeTick` | `[u32,i32][]` | default `[[500,10],[3000,60],[10000,200]]` | `(fee, tickSpacing)` pairs probed for V4 pools (hooks = `address(0)`) |

### 5.2 `SwapReq`

```json
{ "tokenIn": "ETH" | "<addr>", "tokenOut": "ETH" | "<addr>", "amountIn": "<base units>",
  "owner": "<addr>", "recipient": "<addr>", "slippageBps": 50, "amountOutMin": "<base units>",
  "deadline": <unix secs>, "symbolIn": "USDC", "symbolOut": "ETH" }
```

Native ether is `"ETH"`, `"native"`, `""` or `0x0…0`. Everything but the first three
fields is optional; `owner` is required by `build_swap`. Every amount is base units.

### 5.3 Seeded default chains (`default_chains()`)

| Chain | V2 | V3 | V4 pricing |
|---|---|---|---|
| Ethereum (1) | factory, Router02, init hash | factory, QuoterV2, **SwapRouter02** `0x68b3…Fc45` | StateView + Quoter |
| Sepolia (11155111) | factory `0xF62c…80E6`, Router02 `0xeE56…CfE3`, init hash | factory `0x0227…AC1c`, QuoterV2 `0xEd1f…2FB3`, SwapRouter02 `0x3bFA…e48E` | — |
| Optimism (10) | — | QuoterV2, SwapRouter02 `0x68b3…Fc45` | — |
| Arbitrum One (42161) | — | QuoterV2, SwapRouter02 `0x68b3…Fc45` | — |
| Base (8453) | — | factory `0x3312…FDfD`, QuoterV2 `0x3d4e…B76a`, SwapRouter02 `0x2626…e481` | — |

Stablecoins: USDC (+ USDT where deployed); Sepolia uses Circle's USDC
`0x1c7D…7238`. Multicall3 is the canonical `0xcA11…CA11` everywhere. The canonical V2 and
V3 init-code hashes derive Sepolia's real pairs and pools (the config tests pin the
USDC/WETH pair and the 0.05% pool read off the chain).

**`v3Router` is SwapRouter02 on every chain.** The encoder wraps V3 swaps in its
`multicall(deadline, …)`, which the legacy SwapRouter (`0xE592…1564`) does not have; a
chain configured with the legacy router reverts every V3 swap.

### 5.4 Persisted state

- **What:** the **full** chain map (defaults + overrides) as one pretty-JSON
  object keyed by chain id.
- **Where:** `<RustModuleContext.instance_persistence_path>/config.json`.
- **When written:** on every `configure` (via `ConfigStore::set_chain → save`).
- **When read:** on `on_context_ready` (`ConfigStore::with_path` overlays the
  persisted overrides on top of `default_chains()`).

---

## 6. Pricing & math (the hard parts)

All math lives in `pricing.rs` and is **pure** (no network, no keys); the
`eth_call` is issued by the glue.

### 6.1 CREATE2 pool-address derivation

Pool addresses are computed locally so a single batch can read them all — no
factory round-trips.

- **V2 pair:** `CREATE2(factory, keccak256(token0 ++ token1), v2InitHash)`
  (`v2_pair_address`). `token0/token1` are the two tokens **sorted ascending by
  address** (`sort_tokens`).
- **V3 pool:** `CREATE2(factory, keccak256(abi.encode(token0, token1, fee)), v3InitHash)`
  (`v3_pool_address`).
- **V4 pool id:** `keccak256(abi.encode(PoolKey{currency0, currency1, fee, tickSpacing, hooks}))`
  (`v4_pool_id`). V4 trades **native** currencies, so the WETH side is native ETH
  (`address(0)`), which always sorts first; `hooks` defaults to `address(0)`
  (vanilla pools). It is a *pool id* (B256), not an address — read via `StateView`.

> Unit-verified against real mainnet pools: USDC/WETH 0.05% V3 pool
> `0x88e6A0c2…F5640` and USDC/WETH V2 pair `0xB4e16d01…28C9Dc`.

### 6.2 Price math per version

For a token priced against WETH, with `token_is_token0 = token < weth`:

- **V2** (`getReserves` → `reserve0, reserve1`):
  `raw_ratio = reserve1 / reserve0` (token1-per-token0), then
  `human = raw_ratio · 10^(dec0 − dec1)`, then
  `eth_per_token = human` if token is token0 else `1/human`.
  **Depth (weight)** = the WETH-side reserve in human units.
- **V3** (`slot0.sqrtPriceX96`):
  `raw_ratio = (sqrtPriceX96 / 2^96)^2`, then the same decimal + side adjustment.
  **Depth** = `WETH.balanceOf(pool)` (the 2nd sub-call), in human WETH.
- **V4** (`StateView.getSlot0.sqrtPriceX96`): same `sqrtPriceX96` math.
  **Depth** = `StateView.getLiquidity(poolId)` (in-range liquidity units, **only
  comparable to other V4 pools**).

A point is kept only if its price `is_finite() && > 0` (`finite_point`).

### 6.3 Best-rate selection (`pick_best`)

Per token, across all its pool observations:

1. **Prefer V2/V3** (their weights are directly-comparable human WETH depth):
   take the **deepest** (max `weight`) — that pool's ETH price wins.
2. **Only if no V2/V3 priced**, fall back to the **deepest V4** pool (its weight
   is liquidity units, not WETH, so it's never mixed with V2/V3).

This is "deepest pool wins" — the most-liquid pool is the most reliable mid price.

### 6.4 token→USD anchoring (`token_usd_prices`)

USD is derived, not fetched: the **first stablecoin that itself priced against
ETH** anchors the dollar. For each token,
`usd = eth_per_token / eth_per_stable`; WETH is added at
`usd = 1 / eth_per_stable`. If **no** configured stablecoin priced, USD is
omitted entirely (prices come back with `usd: null`).

**Worked example (the doctest's numbers):** reserves 6,000,000 USDC (6 dec) vs
2,000 WETH (18 dec) → `eth_per_usdc = 1/3000`; with USDC the stablecoin,
`WETH usd = 1 / (1/3000) = $3000`, `USDC usd = $1`.

### 6.5 Swap quoting and encoding (`swap.rs`)

- **Candidates.** `candidate_routes` enumerates V2 direct, V2 via WETH, V3 direct per fee
  tier, V3 via WETH per tier pair — 22 on a full chain. Nothing goes "via WETH" when one
  side already is WETH, and ether against WETH is a wrap, not a swap (no routes).
- **The batch.** `build_quote_batch` emits, per route, the quote and its probe
  (`probe_amount` = `amountIn / 1000`, at least 1): V2 `getAmountsOut(path)`, V3
  `QuoterV2.quoteExactInputSingle` or `quoteExactInput(path)`. With an owner it appends
  `balanceOf(owner)` (or Multicall3 `getEthBalance`) and, via `with_allowance_read`, one
  `allowance(owner, router)` per router the chain has — the winner's router is not known
  until the quotes are back.
- **Decoding.** `decode_quotes` reads every route that answered non-zero (a reverted call
  is a pool that does not exist) and QuoterV2's `gasEstimate`, which is word 3 in both
  return shapes. `pick_best` takes the largest output; between equals, the fewer hops.
- **Price impact.** `price_impact_bps = 10000 × (1 − (amountOut/amountIn) / (probeOut/probeIn))`
  in integer arithmetic, clamped at 0, `None` without a probe answer. The probe pays the
  same pool fees, so the figure is the amount's own weight, fee excluded.
- **Gas hints.** V3: the quoter's estimate + 25% + 70k router overhead (+30k wrapping ether
  in, +40k unwrapping it out); 150k per hop when the quoter gave none. V2: 180k single,
  260k two-hop. Approve: 60k. A limit too low burns the fee and swaps nothing, so these
  err high; the sender estimates the legs it can, and takes the hint where it cannot
  (a swap leg behind an approval reverts under `eth_estimateGas` until that lands).
- **Approvals.** `approval_needed`: ether → none; allowance ≥ amount → none; zero or unread
  → approve the amount; non-zero and short → zero it, then approve the amount.
- **Encoding.** See 4.5. Selectors: V2 `getAmountsOut 0xd06ca61f`; SwapRouter02
  `exactInputSingle 0x04e45aaf`, `exactInput 0xb858183f`, `unwrapWETH9 0x49404b7c`,
  `multicall(uint256,bytes[]) 0x5ae401dc`; QuoterV2 `quoteExactInput 0xcdca1753`.

## 7. Build, run & test

### 7.1 Build (Nix)

```bash
# Build the module package (default output)
nix build 'github:logos-co/logos-evm-uniswap-module'

# Build the installable .lgx
nix build 'github:logos-co/logos-evm-uniswap-module#lgx' -o uniswap-lgx
ls uniswap-lgx/*.lgx
```

The `flake.nix` calls `logos-module-builder.lib.mkLogosModule` with `./metadata.json`;
the dependency `eth_rpc_module` `follows` the same `logos-module-builder` so the
generated `modules().eth_rpc_module` client matches its published `.lidl`
contract. `CMakeLists.txt` drives the build via the `logos_module(NAME
uniswap_module)` macro from `LogosModule.cmake`.

### 7.2 Unit-test the pure cores

```bash
cd rust-lib
cargo test --no-default-features        # config + pricing + swap + reply, no Logos/Qt
```

Covered: CREATE2 against known mainnet pools, V2/V3 price recovery, V2 reserve
depth, `pick_best` preference rules, USD anchoring, batch enumeration counts
(mainnet = 1 V2 + 8 V3 + 6 V4 = 15 sub-calls; WETH self-pool skipped), quote
selection, and swap-calldata selectors (`getAmountsOut` `0xd06ca61f`, `approve`
`0x095ea7b3`, `exactInputSingle` `0x414bf389`).

### 7.3 Drive it via `logoscore`

Because the module is `concurrency: "multi"`, build `logoscore` against
`logos-protocol ≥ 0.2` (it resolves the deferred replies):

```bash
nix build 'github:logos-co/logos-logoscore-cli#cli' --out-link ./logos
# install eth_rpc_module + uniswap_module .lgx via lgpm into ./modules, then:
./logos/bin/logoscore -D -m ./modules &           # daemon
./logos/bin/logoscore load-module eth_rpc_module
./logos/bin/logoscore load-module uniswap_module
./logos/bin/logoscore call uniswap_module get_chains
./logos/bin/logoscore call uniswap_module configure @uni_chain.json
./logos/bin/logoscore call eth_rpc_module set_chain_config 31337 @rpc_chain.json
./logos/bin/logoscore call uniswap_module get_prices 31337 @tokens.json
```

### 7.4 The executable doc-test

`doctests/uniswap-module-runtime.test.yaml` (rendered to
`doctests/outputs/uniswap-module-runtime.md`) proves the **full runtime path**
end-to-end against a **local mock JSON-RPC node** — no external network,
reproducible in CI:

1. Builds + installs `eth_rpc_module` and `uniswap_module` `.lgx` via `lgpm`.
2. Starts a tiny Python mock node that returns a **canned Multicall3 `aggregate3`
   result**: one V2 `getReserves` = 6,000,000 USDC vs 2,000 WETH (1 WETH = 3000 USDC).
3. Configures a local chain `31337` (V2-only, USDC stablecoin) in **both** the
   uniswap module (`configure`) and `eth_rpc_module` (`set_chain_config`, pointed
   at the mock node, `proxyRequired:false`).
4. Calls `get_prices 31337 @tokens.json` and asserts the output contains
   `"prices"`, `"31337"`, and `"3000"` — i.e. WETH prices at **≈ $3000**,
   computed entirely from the mock reserves decoded in-process.

The `get_prices` step retries up to 6× on a transient `RPC_FAILED` (a cold-replica
transport race on loaded CI runners); the call is a pure read, so retrying is
side-effect-free. Run it locally with `doctests/run.sh`
(`DOCTEST="nix run path:../logos-doctest --" ./run.sh` to use a local runner).
CI (`.github/workflows/doctests.yml`) runs it on ubuntu + macOS and publishes a
two-column HTML report to GitHub Pages.

---

## 8. Concurrency (`concurrency: "multi"`)

`metadata.json` declares `concurrency: "multi"`. Every price/quote/swap method
**blocks on a Multicall3 `eth_call` through `eth_rpc`**, so the module opts into
**concurrent handler dispatch**: pricing several chains (or several callers) at
once no longer serializes behind one in-flight RPC. The runtime returns each
result via a pending-sentinel that the consumer transport resolves transparently.

The multi contract makes the generated trait take `&self` and require
`Send + Sync`. Concurrency safety is upheld by:

- **`cfg: RwLock<Option<ConfigStore>>`** — readers (`get_prices`, `quote_swap`,
  `build_swap`, `get_chains`) take the **shared read lock**, clone the chain they
  need, and **drop the guard before** the blocking `eth_rpc` call (`with_cfg` /
  `chain_cfg`). `run_multicall` touches no module state, so **no lock is held
  across the network call**.
- **`configure` is the only writer** — it takes the exclusive write lock
  (`with_cfg_mut`), the sole mutator of the config map.

This is the same pattern as the wallet's other `concurrency:multi` module
(`eth_rpc_module`): read-lock, clone, drop, call.

---

## 9. Security & invariants

| Invariant | Enforced by |
|---|---|
| **No direct network access.** The module never opens a socket. | All on-chain reads go through `modules().eth_rpc_module.call`; the crate pulls in no HTTP client (`alloy` is `default-features = false`, `sol-types` only). |
| **Fail-closed privacy preserved.** | Because the only egress is `eth_rpc_module`, the wallet's `net-proxy` SOCKS5 chokepoint still governs every request — this module can't bypass it. |
| **No keys / no signing.** | The module builds **unsigned** calldata only (`build_swap` returns `(router, value, data, approve)`); signing/broadcast is the backend + `keystore_module`. |
| **One batch, one decoder.** | `run_multicall` encodes a single `aggregate3` and decodes via `decode_aggregate3_returns` — never hand-split the hex (see §3 warning). |
| **`allowFailure = true` per sub-call.** | A non-existent pool reverting one read won't sink the whole batch (`Call3.allowFailure = true`); reverted reads decode to `None` and are simply skipped. |
| **Offline address derivation is checksum-agnostic but exact.** | CREATE2 derivation is verified against known mainnet pools in unit tests; a wrong factory/init-hash in config just yields empty pools (priced as `null`), never a wrong-but-plausible address from an untrusted source. |
| **Config-not-ready is a hard error, not a silent default.** | Reads error with `"uniswap not initialized (context not ready)"`; `configure` returns `false` until `on_context_ready` has run. |

---

## 10. File map

| Path | Role |
|---|---|
| `metadata.json` | Module manifest: name, `interface: cdylib`, `concurrency: multi`, `dependencies: [eth_rpc_module]`, `codegen.rust` (trait `UniswapModule`, source `src/glue.rs`) |
| `flake.nix` | Nix build via `mkLogosModule`; declares the `eth_rpc_module` input with a `follows` on `logos-module-builder` |
| `CMakeLists.txt` | `logos_module(NAME uniswap_module)` |
| `rust-lib/Cargo.toml` | Crate (`alloy` sol-types only, `hex`, `serde`; optional `logos-rust-sdk` behind `logos_module`) |
| `rust-lib/src/lib.rs` | Crate root; exposes `config`/`pricing`/`reply`/`swap`, gates `glue` behind `logos_module` |
| `rust-lib/src/glue.rs` | **Public API** (`pub trait UniswapModule`), `UniswapModuleImpl`, `run_multicall`, the `modules().eth_rpc_module.call` site |
| `rust-lib/src/config.rs` | `ChainUniswap`, `ConfigStore`, `default_chains()`, persistence |
| `rust-lib/src/pricing.rs` | CREATE2, Multicall3 encode/decode, V2/V3/V4 price math, `pick_best`, `token_usd_prices` |
| `rust-lib/src/swap.rs` | V2/V3 quote calldata + decode, `decode_best_quote`, router `build_swap` + approvals |
| `rust-lib/src/reply.rs` | `err`, the error reply: a dependency's `{ ok:false }` refusal verbatim, anything else wrapped |
| `doctests/uniswap-module-runtime.test.yaml` | Executable end-to-end doc-test (mock-node `get_prices` round-trip) |
| `doctests/outputs/uniswap-module-runtime.md` | Rendered doc-test output |
| `doctests/run.sh` | Local doc-test runner |
| `.github/workflows/doctests.yml` | CI: run doc-tests on ubuntu+macOS, publish report to GitHub Pages |

> **Generated, gitignored:** `rust-lib/generated/provider_gen.rs` (the codegen'd
> `modules()`, `install::<T>()`, `RustModuleContext`) and `logos-rust-sdk-src`
> (the SDK path dep) are produced at build time by `logos-module-builder` from
> the `.lidl` contracts — they are not checked in.
