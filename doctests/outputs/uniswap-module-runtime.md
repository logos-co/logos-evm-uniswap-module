# Pricing Tokens with the Uniswap Module on logoscore

`logos-evm-uniswap-module` is the **price oracle and swap router** for the
Logos multi-chain EVM wallet. It derives Uniswap pool addresses offline
(CREATE2), bundles every on-chain read into a **single Multicall3 `eth_call`**,
and returns best-rate token→ETH / token→USD prices across **Uniswap V2, V3 and
V4** — plus V2/V3 swap-transaction building. It is multi-chain and fully
configurable, and it talks to the network only through `eth_rpc_module`, so the
wallet's fail-closed proxy still governs every request.

This doc-test drives the module through a `logoscore` daemon. The pure pricing
math (CREATE2 derivation, `sqrtPriceX96`/reserve maths, best-rate selection,
Multicall3 encode/decode) is covered by the crate's unit tests; here we prove
the **runtime** path end-to-end against a **local mock JSON-RPC node** (no
external network, reproducible in CI):

1. Build/install the module (and its `eth_rpc_module` dependency) and start a
   daemon.
2. Read the seeded multi-chain config — Ethereum, Optimism, Arbitrum, Base.
3. Configure a local chain and ask for a token price: the module builds a
   Multicall3 batch, issues one `eth_call` through `eth_rpc_module` to the mock
   node, decodes the pool reserves, and reports the USD price — a real
   `get_prices` round-trip.

**What you'll build:** This `uniswap_module`, packaged as `.lgx`, installed with `lgpm` alongside `eth_rpc_module`, and driven through a `logoscore` daemon against a local mock node.

**What you'll learn:**

- How the module ships a seeded, configurable multi-chain Uniswap deployment map
- How a token price is computed from a single Multicall3 batch issued via eth_rpc
- How token→USD is anchored on a stablecoin/WETH rate

## Prerequisites

- **Nix** with flakes enabled. Install from [nixos.org](https://nixos.org/download.html), then enable flakes:

```bash
mkdir -p ~/.config/nix
echo 'experimental-features = nix-command flakes' >> ~/.config/nix/nix.conf
```

- **A Linux or macOS machine** with `python3` available (used to run the local mock JSON-RPC node).

---

## Step 1: Build logoscore and lgpm

### 1.1 Build logoscore

```bash
nix build 'github:logos-co/logos-logoscore-cli#cli' --out-link ./logos
```

### 1.2 Build lgpm

```bash
nix build 'github:logos-co/logos-package-manager#cli' -o lgpm
```

---

## Step 2: Build and install the modules

The Uniswap module depends on `eth_rpc_module` (it issues its batched
`eth_call` through it), so we install both.

### 2.1 Build the eth-rpc module's .lgx

```bash
nix build 'github:logos-co/logos-evm-eth-rpc-module#lgx' -o eth-rpc-lgx
```

```bash
ls eth-rpc-lgx/*.lgx
```

### 2.2 Build the uniswap module's .lgx

```bash
nix build 'github:logos-co/logos-evm-uniswap-module#lgx' -o uniswap-lgx
```

```bash
ls uniswap-lgx/*.lgx
```

### 2.3 Seed the capability module

```bash
mkdir -p modules
cp -RL ./logos/modules/. ./modules/

```

### 2.4 Install eth_rpc_module

```bash
./lgpm/bin/lgpm --modules-dir ./modules --allow-unsigned install --file eth-rpc-lgx/*.lgx
```

### 2.5 Install uniswap_module

```bash
./lgpm/bin/lgpm --modules-dir ./modules --allow-unsigned install --file uniswap-lgx/*.lgx
```

### 2.6 Confirm the installs

```bash
./lgpm/bin/lgpm --modules-dir ./modules list
```

---

## Step 3: Start a mock JSON-RPC node

A tiny local node that answers `eth_call` with a canned **Multicall3
`aggregate3` result**: a single Uniswap V2 `getReserves` return of
6,000,000 USDC against 2,000 WETH — i.e. 1 WETH = 3,000 USDC. The price the
module reports is derived from exactly these reserves, so the round-trip is
deterministic and offline.

### 3.1 Write the mock node

```
import http.server, json
# aggregate3([{success:true, returnData: getReserves()}]) with
# reserve0 = 6_000_000e6 USDC, reserve1 = 2_000e18 WETH (1 WETH = 3000 USDC).
AGG3 = "0x00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000574fbde600000000000000000000000000000000000000000000000006c6b935b8bbd4000000000000000000000000000000000000000000000000000000000000000000000"
RES = {"eth_chainId": "0x7a69", "eth_call": AGG3, "eth_blockNumber": "0x10"}
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get('content-length', 0))
        req = json.loads(self.rfile.read(n) or b'{}')
        body = json.dumps({"jsonrpc": "2.0", "id": req.get("id", 1),
                           "result": RES.get(req.get("method"), "0x0")}).encode()
        self.send_response(200)
        self.send_header('content-length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass
http.server.HTTPServer(('127.0.0.1', 8601), H).serve_forever()
```

### 3.2 Start the mock node

```bash
python3 mock_node.py &
```

```bash
sleep 2
```

---

## Step 4: Run the daemon and price a token

### 4.1 Start the daemon

```bash
logoscore -D -m ./modules > logs.txt &
```

```bash
sleep 3
```

### 4.2 Load the modules

```bash
./logos/bin/logoscore load-module eth_rpc_module
./logos/bin/logoscore load-module uniswap_module

```

### 4.3 Read the seeded multi-chain config

The module ships sensible defaults for Ethereum, Optimism, Arbitrum and Base (chainId 8453).

```bash
logoscore call uniswap_module get_chains
```

### 4.4 Configure the local chain (Uniswap side: V2-only)

A local chain `31337` with just a V2 deployment and USDC as the
stablecoin — enough to price USDC→ETH from one pool read.

```json
{
  "chainId": 31337,
  "weth": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
  "stablecoins": ["0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"],
  "multicall3": "0xcA11bde05977b3631167028862bE2a173976CA11",
  "v2Factory": "0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f",
  "v2InitCodeHash": "0x96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f"
}
```

### 4.5 Apply the Uniswap chain config

```bash
logoscore call uniswap_module configure @uni_chain.json
```

### 4.6 Point eth_rpc chain 31337 at the mock node

```json
{ "endpoint": "http://127.0.0.1:8601", "proxyRequired": false }
```

### 4.7 Apply the eth_rpc chain config

```bash
logoscore call eth_rpc_module set_chain_config 31337 @rpc_chain.json
```

### 4.8 Price USDC (Multicall3 round-trip through eth_rpc)

`get_prices` builds the Multicall3 batch, issues one `eth_call` via
`eth_rpc_module` to the mock node, decodes the V2 reserves, and reports
token→ETH and token→USD. With 1 WETH = 3,000 USDC, USDC anchors USD and
**WETH prices at ≈ $3000**.

```json
{ "tokens": [ { "address": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", "decimals": 6 } ] }
```

### 4.9 Call get_prices

```bash
logoscore call uniswap_module get_prices 31337 @tokens.json
```

### 4.10 Stop the daemon and the mock node

```bash
./logos/bin/logoscore stop
pkill -f mock_node.py 2>/dev/null || true

```

```bash
sleep 2
```

### 4.11 Confirm the daemon has stopped

```bash
./logos/bin/logoscore status || true
```

The reported $3000 WETH price is computed entirely from the mock's
reserves, decoded in-process — proof that the full path (Multicall3
batch → `eth_rpc_module` → node → reserve maths → USD anchoring) runs
end-to-end through the live module.
