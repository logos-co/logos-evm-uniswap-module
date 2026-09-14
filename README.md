# logos-evm-uniswap-module

`uniswap_module` is the Logos EVM wallet's Uniswap **price oracle and swap quoter**. It
derives pool addresses offline, bundles every read into one Multicall3 `eth_call` through
`eth_rpc_module`, and answers with prices, the best swap route, its price impact, and the
calls that make the swap. It holds no key, picks no chain and sends nothing: a swap app
hands the calls it builds to `tx_sender_module`, which asks the keystore for one approval
over all of them.

The full contract — every method, reply shape, seeded chain and the maths — is in
[`docs/specs.md`](docs/specs.md). The executable walk-through is
[`doctests/uniswap-module-runtime.test.yaml`](doctests/uniswap-module-runtime.test.yaml).

## Methods

| Method | What it answers |
|---|---|
| `configure(chainJson)` | add or override a chain's Uniswap deployment |
| `get_chains()` | the seeded chains (Ethereum, Sepolia, Optimism, Arbitrum, Base) plus overrides |
| `get_prices(chainId, tokensJson)` | token→ETH and token→USD, best-rate across V2/V3/V4 |
| `quote_swap(chainId, swapJson)` | best route, output, price impact, gas hint; with an `owner`, balance and whether an approval must go first |
| `build_swap(chainId, swapJson)` | the quote plus `calls: [{kind, to, value, data, gasLimitHint, label}]` in the order they must land |

## Testing

```bash
cargo test --manifest-path rust-lib/Cargo.toml --no-default-features   # the pure cores
nix build .#default                                                     # the module
nix run github:logos-co/logos-doctest -- run doctests/uniswap-module-runtime.test.yaml
```
