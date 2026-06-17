//! `uniswap_module` — Uniswap V2/V3/V4 prices and swap building for the Logos
//! EVM wallet.
//!
//! The pure cores (`config`, `pricing`, `swap`) carry no Logos/Qt dependency and
//! are unit-tested with `cargo test --no-default-features`. The `glue` module
//! (behind the default `logos_module` feature) wires the contract trait to the
//! Logos runtime via `logos-rust-sdk`.

pub mod config;
pub mod pricing;
pub mod swap;

#[cfg(feature = "logos_module")]
mod glue;
