//! Phase 3: live Arbitrum multi-DEX pool data feed.
//!
//! This module discovers and reads REAL, CURRENT pool state from Arbitrum
//! mainnet and turns it into the [`PoolEdge`] values consumed by the fee-aware
//! Bellman-Ford scanner. It intentionally does NOT touch the arbitrage math or
//! the simulator — it only produces honest, human-unit prices.
//!
//! ## Price derivation
//! * **Uniswap V3** pools expose `slot0().sqrtPriceX96`. The raw ratio is
//!   `(sqrtPriceX96 / 2^96)^2 = amount_token1_raw / amount_token0_raw`. We then
//!   convert to human units with the `10^(dec0 - dec1)` decimal factor so the
//!   value is `token1_human` received per `token0_human` supplied.
//! * **UniswapV2 / SushiSwap** pools expose `getReserves()`. The marginal price
//!   `token0 -> token1` is `(reserve1 / reserve0) * 10^(dec0 - dec1)` in human
//!   units. The pool swap fee (0.30% = 3000 ppm) is carried separately in
//!   [`PoolEdge::fee`] and applied by the fee-aware edge weights, so it is NOT
//!   baked into `price` here.
//!
//! Both V3 and canonical V2 factories sort a pair's tokens by ascending address,
//! so `token0 = min(addr)` and `token1 = max(addr)`. We rely on that ordering
//! (documented and asserted by discovery) instead of spending extra RPC calls on
//! `token0()`/`token1()`.

use std::time::Duration;

use ethers::types::{Address, U256};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::types::{PoolEdge, PoolSwapState, V3CrossTickData, V3Tick};

/// A verified Arbitrum token with its canonical address and ERC-20 decimals.
///
/// `decimals` is the vetted registry value: authoritative for the 11 blue-chips
/// and a cross-check hint for the long-tail set. The live feed still reads every
/// token's `decimals()` ON-CHAIN (see [`read_onchain_decimals`]) and never
/// assumes a long-tail token's decimals — a long-tail token whose `decimals()`
/// cannot be read is dropped (fail-closed).
#[derive(Debug, Clone, Copy)]
pub struct TokenInfo {
    pub symbol: &'static str,
    pub address: Address,
    pub decimals: u8,
    /// True for the 11 curated blue-chips (the USD-anchor backbone, always kept
    /// with a resilient decimals fallback); false for the Phase 9 long-tail set.
    pub blue_chip: bool,
}

/// Uniswap V3 factory (identical address across all chains it is deployed on).
pub const UNISWAP_V3_FACTORY: &str = "0x1F98431c8aD98523631AE4a59f267346ea31F984";
/// SushiSwap (UniswapV2-style constant-product) factory on Arbitrum.
pub const SUSHISWAP_V2_FACTORY: &str = "0xc35DADB65012eC5796536bD9864eD8773aBc74C4";

/// Function selectors (first 4 bytes of keccak256 of the signature).
const SEL_GET_POOL: [u8; 4] = [0x16, 0x98, 0xee, 0x82]; // getPool(address,address,uint24)
const SEL_GET_PAIR: [u8; 4] = [0xe6, 0xa4, 0x39, 0x05]; // getPair(address,address)
const SEL_SLOT0: [u8; 4] = [0x38, 0x50, 0xc7, 0xbd]; // slot0()
const SEL_GET_RESERVES: [u8; 4] = [0x09, 0x02, 0xf1, 0xac]; // getReserves()
const SEL_BALANCE_OF: [u8; 4] = [0x70, 0xa0, 0x82, 0x31]; // balanceOf(address)
const SEL_LIQUIDITY: [u8; 4] = [0x1a, 0x68, 0x65, 0x02]; // liquidity()
const SEL_TICK_BITMAP: [u8; 4] = [0x53, 0x39, 0xc2, 0x96]; // tickBitmap(int16)
const SEL_TICKS: [u8; 4] = [0xf3, 0x0d, 0xba, 0x93]; // ticks(int24)
const SEL_DECIMALS: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67]; // decimals()

/// Default minimum pool liquidity (USD) below which a pool is treated as dust
/// and skipped, so near-empty/stale pools cannot manufacture fake spreads.
/// Overridable via `SCANNER_MIN_POOL_LIQUIDITY_USD`.
pub const DEFAULT_MIN_POOL_LIQUIDITY_USD: f64 = 5_000.0;

/// Minimum pool liquidity (USD) for a pool that touches a NON-blue-chip
/// (long-tail) token. Higher than the blue-chip floor because long-tail prices
/// are easier to manipulate and need more depth to be trusted. Overridable via
/// `SCANNER_MIN_LONGTAIL_POOL_LIQUIDITY_USD`.
pub const DEFAULT_MIN_LONGTAIL_POOL_LIQUIDITY_USD: f64 = 25_000.0;

/// Hard cap on the total token universe (blue-chips + long-tail) to bound the
/// O(N^2) pair-probe discovery and keep RPC sane. `SCANNER_MAX_TOKENS`.
pub const DEFAULT_MAX_TOKENS: usize = 64;

/// Hard cap on the pools carried into edge-building / cross-tick fetch (the
/// deepest by USD liquidity are kept). `SCANNER_MAX_POOLS`.
pub const DEFAULT_MAX_POOLS: usize = 400;

/// Minimum independent venue quotes that must corroborate a token's USD price
/// before it is anchored; below this the token is left un-priced and its pools
/// are dropped (fail-closed). `SCANNER_MIN_ANCHOR_VENUES`.
pub const DEFAULT_MIN_ANCHOR_VENUES: usize = 2;

/// Max % a venue quote may deviate from the median before it is discarded as an
/// outlier when a consensus set exists. `SCANNER_ANCHOR_OUTLIER_PCT`.
pub const DEFAULT_ANCHOR_OUTLIER_PCT: f64 = 3.0;

/// Max % by which a V2 pool's live `balanceOf` may fall below its stored
/// `getReserves()` before the token is treated as non-standard and dropped from
/// ALL its pools (fail-closed). `SCANNER_V2_BALANCE_TOLERANCE_PCT`.
pub const DEFAULT_V2_BALANCE_TOLERANCE_PCT: f64 = 1.0;

/// Sane upper bound on ERC-20 decimals; an on-chain `decimals()` above this is
/// treated as malformed and the token is dropped.
const MAX_PLAUSIBLE_DECIMALS: u64 = 36;

/// If more than this fraction of a discovery or state/balance batch fails, the
/// RPC is treated as degraded/throttled and the fetch fails closed rather than
/// building a partial graph (which would let the dust guard go inert).
pub const MAX_BATCH_FAILURE_FRACTION: f64 = 0.20;

/// Batch-failure fraction (%) above which coverage is flagged as possibly
/// throttle-affected. Well below the hard [`MAX_BATCH_FAILURE_FRACTION`] abort
/// bar: a run can complete "successfully" yet still have lost enough calls that a
/// small/zero edge set is suspect rather than an honest result.
pub const THROTTLE_WARN_PCT: f64 = 5.0;

/// Honest per-fetch discovery telemetry (Phase 9). Lets a caller (and a reviewer)
/// distinguish an HONEST-ZERO outcome — probes succeeded and candidates were
/// removed by real economic / safety gates — from an RPC THROTTLE-COLLAPSE, where
/// probes or state reads silently failed and the universe shrank for the wrong
/// reason. This is a REPORTING artifact only: it is populated by
/// [`fetch_arbitrum_pools_with_usd_at_block`] and never influences any gating
/// decision. Counts are per single fetch (one block).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryTelemetry {
    /// Block the snapshot was read at.
    pub block: u64,
    /// Tokens in the full registry before capping.
    pub registry_tokens: usize,
    /// Effective tokens fed into pair discovery (blue-chip + long-tail).
    pub tokens_loaded: usize,
    /// Blue-chips among `tokens_loaded`.
    pub bluechip_loaded: usize,
    /// Long-tail tokens among `tokens_loaded`.
    pub longtail_loaded: usize,
    /// Long-tail tokens dropped because on-chain `decimals()` was unreadable.
    pub tokens_dropped_no_decimals: usize,
    /// Tokens dropped by the fee-on-transfer / non-standard denylist.
    pub tokens_dropped_denylist: usize,
    /// Factory getPool/getPair probes attempted.
    pub discovery_probes: usize,
    /// Discovery probes that belonged to a FAILED batch (RPC throttle signal).
    pub discovery_probes_failed: usize,
    /// Live pools discovered (non-zero factory addresses).
    pub pools_discovered: usize,
    /// Pool state/balance calls attempted.
    pub state_calls: usize,
    /// State/balance calls that belonged to a FAILED batch (RPC throttle signal).
    pub state_calls_failed: usize,
    /// Pools dropped because a required state read (price/liquidity/balance) was
    /// missing — throttle-sensitive, NOT an honest economic rejection.
    pub pools_dropped_incomplete_state: usize,
    /// Pools dropped because price bytes were undecodable or implausible (a
    /// decimals/ordering data-integrity guard).
    pub pools_dropped_bad_price: usize,
    /// Pools dropped for touching a token flagged non-standard by the V2
    /// reserves-vs-balanceOf check (fee-on-transfer / rebasing guard).
    pub pools_dropped_nonstandard: usize,
    /// Pools dropped by the USD liquidity floor (dust / unvaluable).
    pub pools_dropped_liquidity: usize,
    /// Of the liquidity drops, how many had NO USD-anchored leg (un-priceable) —
    /// informational subset of `pools_dropped_liquidity`.
    pub pools_unpriced_leg: usize,
    /// Pools trimmed by the `SCANNER_MAX_POOLS` depth cap (a bound, not a reject).
    pub pools_capped: usize,
    /// Pools that survived every gate and produced edges.
    pub pools_kept: usize,
    /// Directional edges built from the kept pools.
    pub edges_loaded: usize,
}

impl DiscoveryTelemetry {
    /// Candidates lost to RPC degradation (failed probes + failed state calls +
    /// pools dropped for missing reads). A large value means a small edge set is
    /// a THROTTLE artifact, not an honest result.
    pub fn throttle_drops(&self) -> usize {
        self.discovery_probes_failed + self.state_calls_failed + self.pools_dropped_incomplete_state
    }

    /// Candidates removed by real economic / safety / data-integrity gates
    /// (decimals, denylist, bad price, non-standard, liquidity). A large value
    /// with `throttle_suspected() == false` is an HONEST filtering result.
    pub fn gate_drops(&self) -> usize {
        self.tokens_dropped_no_decimals
            + self.tokens_dropped_denylist
            + self.pools_dropped_bad_price
            + self.pools_dropped_nonstandard
            + self.pools_dropped_liquidity
    }

    /// Percentage of discovery probes that failed.
    pub fn discovery_fail_pct(&self) -> f64 {
        if self.discovery_probes == 0 {
            0.0
        } else {
            100.0 * self.discovery_probes_failed as f64 / self.discovery_probes as f64
        }
    }

    /// Percentage of state/balance calls that failed.
    pub fn state_fail_pct(&self) -> f64 {
        if self.state_calls == 0 {
            0.0
        } else {
            100.0 * self.state_calls_failed as f64 / self.state_calls as f64
        }
    }

    /// Percentage of discovered pools dropped for incomplete state reads.
    pub fn incomplete_state_pct(&self) -> f64 {
        if self.pools_discovered == 0 {
            0.0
        } else {
            100.0 * self.pools_dropped_incomplete_state as f64 / self.pools_discovered as f64
        }
    }

    /// True when the RPC lost enough calls that this fetch's coverage (and hence a
    /// small/zero edge set) should be treated as THROTTLE-affected rather than an
    /// honest result: any of the failure rates exceeds [`THROTTLE_WARN_PCT`].
    pub fn throttle_suspected(&self) -> bool {
        self.discovery_fail_pct() > THROTTLE_WARN_PCT
            || self.state_fail_pct() > THROTTLE_WARN_PCT
            || self.incomplete_state_pct() > THROTTLE_WARN_PCT
    }
}

/// Uniswap V3 fee tiers we probe, in hundredths-of-a-bip (ppm).
pub const V3_FEE_TIERS: [u32; 3] = [500, 3000, 10000];
/// Canonical UniswapV2/SushiSwap swap fee in ppm (0.30%).
pub const V2_FEE_PPM: u32 = 3000;

fn addr(s: &str) -> Address {
    s.parse().expect("hardcoded address is valid")
}

/// The verified Arbitrum token registry: the 11 curated blue-chips (the
/// USD-anchor backbone, always kept) plus a cross-verified long-tail set added in
/// Phase 9 to reach less-arbitraged mid/small-cap pools. Every address is a real,
/// on-chain Arbitrum token: the blue-chips are canonical, and each long-tail
/// address was taken from and CROSS-VERIFIED across >=2 reputable token lists
/// (CoinGecko Arbitrum, the Arbitrum-bridged Uniswap Labs list, SushiSwap's
/// list) — never guessed. Decimals here are vetted values; the live feed still
/// reads `decimals()` on-chain for every token and drops any long-tail token it
/// cannot read (see [`fetch_arbitrum_pools_with_usd_at_block`]).
///
/// The long-tail set is deliberately limited to STANDARD ERC-20s — no
/// fee-on-transfer, no rebasing (e.g. wstETH/weETH/ezETH/cbETH are the
/// NON-rebasing LST wrappers) — so non-standard-token math cannot fabricate
/// profit. See [`fee_on_transfer_denylist`] for the dynamic-discovery guard.
pub fn arbitrum_tokens() -> Vec<TokenInfo> {
    vec![
        // --- 11 curated blue-chips (preserved; the USD-anchor backbone) ---
        TokenInfo { symbol: "WETH", address: addr("0x82aF49447D8a07e3bd95BD0d56f35241523fBab1"), decimals: 18, blue_chip: true },
        TokenInfo { symbol: "USDC", address: addr("0xaf88d065e77c8cC2239327C5EDb3A432268e5831"), decimals: 6, blue_chip: true },
        TokenInfo { symbol: "USDCe", address: addr("0xFF970A61A04b1cA14834A43f5dE4533eBDDB5CC8"), decimals: 6, blue_chip: true },
        TokenInfo { symbol: "USDT", address: addr("0xFd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9"), decimals: 6, blue_chip: true },
        TokenInfo { symbol: "ARB", address: addr("0x912CE59144191C1204E64559FE8253a0e49E6548"), decimals: 18, blue_chip: true },
        TokenInfo { symbol: "WBTC", address: addr("0x2f2a2543B76A4166549F7aaB2e75Bef0aefC5B0f"), decimals: 8, blue_chip: true },
        TokenInfo { symbol: "GMX", address: addr("0xfc5A1A6EB076a2C7aD06eD22C90d7E710E35ad0a"), decimals: 18, blue_chip: true },
        TokenInfo { symbol: "PENDLE", address: addr("0x0c880f6761F1af8d9Aa9C466984b80DAb9a8c9e8"), decimals: 18, blue_chip: true },
        TokenInfo { symbol: "MAGIC", address: addr("0x539bdE0d7Dbd336b79148AA742883198BBF60342"), decimals: 18, blue_chip: true },
        TokenInfo { symbol: "RDNT", address: addr("0x3082CC23568eA640225c2467653dB90e9250AaA0"), decimals: 18, blue_chip: true },
        TokenInfo { symbol: "DAI", address: addr("0xDA10009cBd5D07dd0CeCc66161FC93D7c9000da1"), decimals: 18, blue_chip: true },
        // --- Phase 9 long-tail set (cross-verified across >=2 reputable lists) ---
        TokenInfo { symbol: "1INCH", address: addr("0x6314c31a7a1652ce482cffe247e9cb7c3f4bb9af"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "AAVE", address: addr("0xba5ddd1f9d7f570dc94a51479a000e3bce967196"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "ACX", address: addr("0x53691596d1bce8cea565b84d4915e69e03d9c99d"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "BAL", address: addr("0x040d1edc9569d4bab2d15287dc5a4f10f56a56b8"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "CBETH", address: addr("0x1debd73e752beaf79865fd6446b0c970eae7732f"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "COMP", address: addr("0x354a6da3fcde098f8389cad84b0182725c6c91de"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "CRV", address: addr("0x11cdb42b0eb46d95f990bedd4695a6e3fa034978"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "DODO", address: addr("0x69eb4fa4a2fbd498c257c57ea8b7655a2559a581"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "EZETH", address: addr("0x2416092f143378750bb29b79ed961ab195cceea5"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "FRAX", address: addr("0x17fc002b466eec40dae837fc4be5c67993ddbd6f"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "GRT", address: addr("0x9623063377ad1b27544c965ccd7342f7ea7e88c7"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "JONES", address: addr("0x10393c20975cf177a3513071bc110f7962cd67da"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "LDO", address: addr("0x13ad51ed4f1b7e9dc168d8a00cb3f4ddd85efa60"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "LINK", address: addr("0xf97f4df75117a78c1a5a0dbb814af92458539fb4"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "LQTY", address: addr("0xfb9e5d956d889d91a82737b9bfcdac1dce3e1449"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "LUSD", address: addr("0x93b346b6bc2548da6a1e7d98e9a421b42541425b"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "MIM", address: addr("0xfea7a6a0b346362bf88a9e4a88416b77a57d6c2a"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "PLS", address: addr("0x51318b7d00db7acc4026c88c3952b66278b6a67f"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "PREMIA", address: addr("0x51fc0f6660482ea73330e414efd7808811a57fa2"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "RPL", address: addr("0xb766039cc6db368759c1e56b79affe831d0cc507"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "RSR", address: addr("0xca5ca9083702c56b481d1eec86f1776fdbd2e594"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "SILO", address: addr("0x0341c0c0ec423328621788d4854119b97f44e391"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "SOLVBTC", address: addr("0x3647c54c4c2c65bc7a2d63c0da2809b399dbbdc0"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "SPELL", address: addr("0x3e6648c5a70a150a88bce65f4ad4d506fe15d2af"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "STG", address: addr("0x6694340fc020c5e6b96567843da2df01b2ce1eb6"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "SUSHI", address: addr("0xd4d42f0b6def4ce0383636770ef773390d85c61a"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "TBTC", address: addr("0x6c84a8f1c29108f47a79964b5fe888d4f4d0de40"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "UNI", address: addr("0xfa7f8980b0f1e64a2062791cc3b0871572f1f7f0"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "USDS", address: addr("0x6491c05a82219b8d1479057361ff1654749b876b"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "WEETH", address: addr("0x35751007a407ca6feffe80b3cb397736d2cf4dbe"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "WSTETH", address: addr("0x5979d7b546e38e414f7e9822514be443a4800529"), decimals: 18, blue_chip: false },
        TokenInfo { symbol: "YFI", address: addr("0x82e3a8f066a6989666b031d916c43672085b1582"), decimals: 18, blue_chip: false },
    ]
}

/// Whether `token` is one of the curated blue-chips in `registry`.
pub fn is_blue_chip(token: Address, registry: &[TokenInfo]) -> bool {
    registry.iter().any(|t| t.address == token && t.blue_chip)
}

/// Addresses known to be fee-on-transfer / rebasing / otherwise non-standard
/// ERC-20s whose transfer semantics break constant-product / V3 output math and
/// would fabricate profit. They are excluded from the universe unconditionally.
///
/// The curated Phase 9 registry is already limited to standard ERC-20s, so this
/// list is the extension point for any FUTURE dynamically-discovered token. Add
/// lowercase `0x…` addresses here.
fn fee_on_transfer_denylist() -> &'static [&'static str] {
    &[]
}

/// Case-insensitive membership test against a fee-on-transfer denylist.
fn is_denylisted(token: Address, denylist: &[&str]) -> bool {
    let t = format!("{token:#x}");
    denylist.iter().any(|d| d.eq_ignore_ascii_case(&t))
}

/// Look up a token's decimals from the registry.
pub fn decimals_of(token: Address, registry: &[TokenInfo]) -> Option<u8> {
    registry.iter().find(|t| t.address == token).map(|t| t.decimals)
}

/// Read a `usize` tuning knob from the environment, falling back to `default`.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Read an `f64` tuning knob from the environment, falling back to `default`.
fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Cap the token universe at `max`, ALWAYS keeping every blue-chip (the USD-anchor
/// backbone) and filling any remaining slots with long-tail tokens in registry
/// order. Bounds the O(N^2) pair-probe discovery. If `max` is smaller than the
/// blue-chip count, every blue-chip is still kept (they are never dropped).
fn cap_tokens(registry: &[TokenInfo], max: usize) -> Vec<TokenInfo> {
    if registry.len() <= max {
        return registry.to_vec();
    }
    let mut out: Vec<TokenInfo> = registry.iter().filter(|t| t.blue_chip).copied().collect();
    let remaining = max.saturating_sub(out.len());
    out.extend(registry.iter().filter(|t| !t.blue_chip).copied().take(remaining));
    out
}

/// Resolve the EFFECTIVE decimals for each registry token from on-chain
/// `decimals()` reads, returning `(effective_tokens, dropped_longtail)`:
///  - a blue-chip ALWAYS survives with its verified constant (a mismatch or an
///    unreadable value only warns), so a throttled decimals batch can never drop
///    a core anchor token — preserving existing behaviour;
///  - a long-tail token uses its ON-CHAIN decimals (authoritative) and is DROPPED
///    (fail-closed) when `decimals()` could not be read, so no new token's
///    decimals is ever assumed.
fn resolve_effective_tokens(
    registry: &[TokenInfo],
    onchain_decimals: &std::collections::HashMap<Address, u8>,
) -> (Vec<TokenInfo>, Vec<Address>) {
    let mut effective = Vec::new();
    let mut dropped = Vec::new();
    for t in registry {
        match onchain_decimals.get(&t.address).copied() {
            Some(d) => {
                if t.blue_chip {
                    if d != t.decimals {
                        warn!(
                            "Blue-chip {} on-chain decimals {} != registry {}; keeping verified constant",
                            t.symbol, d, t.decimals
                        );
                    }
                    effective.push(*t);
                } else {
                    if d != t.decimals {
                        debug!(
                            "Long-tail {} on-chain decimals {} override registry hint {}",
                            t.symbol, d, t.decimals
                        );
                    }
                    effective.push(TokenInfo { decimals: d, ..*t });
                }
            }
            None => {
                if t.blue_chip {
                    warn!(
                        "Blue-chip {} decimals() unreadable; falling back to verified constant {}",
                        t.symbol, t.decimals
                    );
                    effective.push(*t);
                } else {
                    dropped.push(t.address);
                }
            }
        }
    }
    (effective, dropped)
}

/// Keep the `max` deepest items by USD depth (descending). `sort_by` is stable, so
/// ties preserve input order for deterministic capping. Bounds the pools carried
/// into cross-tick fetch + edge building.
fn cap_by_depth<T>(mut items: Vec<(T, f64)>, max: usize) -> Vec<(T, f64)> {
    items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    items.truncate(max);
    items
}

/// USD-pegged stablecoins in the registry, each valued at $1 as the anchor for
/// deriving live USD prices of the other tokens.
fn stable_symbols() -> [&'static str; 4] {
    ["USDC", "USDCe", "USDT", "DAI"]
}

fn is_stable(symbol: &str) -> bool {
    stable_symbols().contains(&symbol)
}

/// Convert a raw token amount to human units using its decimals.
fn to_human(raw: U256, decimals: u8) -> f64 {
    u256_to_f64(raw) / 10f64.powi(decimals as i32)
}

/// One pool observation used for USD anchoring: the address-sorted token pair,
/// the `token1`-per-`token0` human price, and each side's human reserve.
#[derive(Debug, Clone, Copy)]
struct PoolObs {
    token0: Address,
    token1: Address,
    price1_per_0: f64,
    human0: f64,
    human1: f64,
}

/// A single USD price quote for a token from one venue, tagged with that venue's
/// USD depth (used for deepest-venue selection).
#[derive(Debug, Clone, Copy)]
struct UsdQuote {
    price: f64,
    depth_usd: f64,
}

/// Median of a non-empty slice of prices.
fn median_of(values: &[f64]) -> f64 {
    let mut v: Vec<f64> = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    let mid = n / 2;
    if n % 2 == 1 {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2.0
    }
}

/// Pick a token's USD anchor from multiple venue quotes with a deepest-venue +
/// median-consensus guard (mirrors the TS `deriveUsdReferenceAnchor`):
///  1. keep finite, positive prices;
///  2. require `>= min_venues` valid quotes, else return `None` — a sub-consensus
///     price is NOT trusted, so the token stays un-priced (fail-closed);
///  3. when a consensus set survives, drop quotes deviating from the median by
///     more than `outlier_pct`;
///  4. among survivors pick the DEEPEST (highest `depth_usd`); ties break toward
///     the price nearest the median.
fn anchor_token_usd(quotes: &[UsdQuote], min_venues: usize, outlier_pct: f64) -> Option<f64> {
    let valid: Vec<UsdQuote> = quotes
        .iter()
        .copied()
        .filter(|q| is_price_plausible(q.price))
        .collect();
    if valid.len() < min_venues.max(1) {
        return None;
    }
    let median = median_of(&valid.iter().map(|q| q.price).collect::<Vec<_>>());
    if median <= 0.0 || !median.is_finite() {
        return None;
    }
    let kept: Vec<UsdQuote> = valid
        .iter()
        .copied()
        .filter(|q| (q.price - median).abs() / median <= outlier_pct / 100.0)
        .collect();
    let considered = if kept.is_empty() { &valid } else { &kept };
    let mut anchor = considered[0];
    for q in considered.iter().copied() {
        let deeper = q.depth_usd > anchor.depth_usd;
        let tie_closer = q.depth_usd == anchor.depth_usd
            && (q.price - median).abs() < (anchor.price - median).abs();
        if deeper || tie_closer {
            anchor = q;
        }
    }
    Some(anchor.price)
}

/// Derive a live USD price for every token by anchoring stablecoins at $1 and
/// relaxing outward across pool observations, guarded by a deepest-venue +
/// median-consensus filter so a single thin/manipulated long-tail pool cannot
/// poison a token's anchor. A token that cannot gather `>= min_venues`
/// corroborating quotes is left UN-PRICED (fail-closed); its pools then fail the
/// USD liquidity gate and are dropped rather than valued on a guessed price.
///
/// For a pool with `token1`-per-`token0` human price `p`:
///   `usd(token0) = p * usd(token1)` and `usd(token1) = usd(token0) / p`.
/// Each quote's depth is the USD value of the already-priced leg (a balanced-pool
/// estimate), so the deepest venue wins. Bounded by `tokens.len()` passes.
fn derive_usd_prices(
    tokens: &[TokenInfo],
    pools: &[PoolObs],
    min_venues: usize,
    outlier_pct: f64,
) -> std::collections::HashMap<Address, f64> {
    use std::collections::HashMap;
    let mut usd: HashMap<Address, f64> = HashMap::new();
    for t in tokens {
        if is_stable(t.symbol) {
            usd.insert(t.address, 1.0);
        }
    }
    // Relaxation: at most `tokens.len()` passes guarantees full propagation.
    for _ in 0..tokens.len() {
        // Gather every corroborating quote for each not-yet-priced token from the
        // current snapshot, then anchor with the consensus + depth guard.
        let mut quotes: HashMap<Address, Vec<UsdQuote>> = HashMap::new();
        for p in pools {
            if !is_price_plausible(p.price1_per_0) {
                continue;
            }
            if let Some(usd1) = usd.get(&p.token1).copied() {
                if !usd.contains_key(&p.token0) {
                    let price = p.price1_per_0 * usd1;
                    if is_price_plausible(price) {
                        quotes
                            .entry(p.token0)
                            .or_default()
                            .push(UsdQuote { price, depth_usd: p.human1 * usd1 * 2.0 });
                    }
                }
            }
            if let Some(usd0) = usd.get(&p.token0).copied() {
                if !usd.contains_key(&p.token1) {
                    let price = usd0 / p.price1_per_0;
                    if is_price_plausible(price) {
                        quotes
                            .entry(p.token1)
                            .or_default()
                            .push(UsdQuote { price, depth_usd: p.human0 * usd0 * 2.0 });
                    }
                }
            }
        }
        let mut changed = false;
        for (token, qs) in &quotes {
            if let Some(price) = anchor_token_usd(qs, min_venues, outlier_pct) {
                usd.insert(*token, price);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    usd
}

/// Convert a `U256` to `f64` losslessly enough for pricing (via decimal string).
fn u256_to_f64(v: U256) -> f64 {
    v.to_string().parse::<f64>().unwrap_or(f64::INFINITY)
}

/// Uniswap V3 human price of `token1` per `token0` from `sqrtPriceX96`.
///
/// `raw = (sqrtPriceX96 / 2^96)^2 = amount1_raw / amount0_raw`, then scaled by
/// `10^(dec0 - dec1)` into human units.
pub fn v3_price_token1_per_token0(sqrt_price_x96: U256, dec0: u8, dec1: u8) -> f64 {
    let sqrt = u256_to_f64(sqrt_price_x96);
    let q96 = 2f64.powi(96);
    let ratio = sqrt / q96;
    let raw = ratio * ratio; // amount1_raw / amount0_raw
    let decimal_factor = 10f64.powi(dec0 as i32 - dec1 as i32);
    raw * decimal_factor
}

/// UniswapV2/Sushi human price of `token1` per `token0` from reserves.
///
/// `price = (reserve1 / reserve0) * 10^(dec0 - dec1)` in human units. The swap
/// fee is NOT applied here (it is carried in `PoolEdge::fee`).
pub fn v2_price_token1_per_token0(reserve0: U256, reserve1: U256, dec0: u8, dec1: u8) -> f64 {
    let r0 = u256_to_f64(reserve0);
    let r1 = u256_to_f64(reserve1);
    if r0 <= 0.0 {
        return 0.0;
    }
    let decimal_factor = 10f64.powi(dec0 as i32 - dec1 as i32);
    (r1 / r0) * decimal_factor
}

/// A price is "plausible" if finite, strictly positive, and within a wide sane
/// band. Values outside this band almost always signal a decimals/ordering bug
/// (the exact class of bug that fabricates fake huge spreads).
pub fn is_price_plausible(price: f64) -> bool {
    price.is_finite() && price > 1e-15 && price < 1e15
}

/// USD valuation of a pool's liquidity from whichever legs are USD-anchored.
///
/// A balanced AMM pool holds ~equal value per side, so when only one leg is
/// priceable we double that leg's value as an estimate of the whole. When
/// NEITHER leg is USD-anchored the pool is unvaluable and this returns `NaN`,
/// which the fail-closed liquidity gate then rejects.
fn value_pool_liquidity_usd(
    usd0: Option<f64>,
    usd1: Option<f64>,
    human0: f64,
    human1: f64,
) -> f64 {
    match (usd0, usd1) {
        (Some(u0), Some(u1)) => human0 * u0 + human1 * u1,
        (Some(u0), None) => human0 * u0 * 2.0,
        (None, Some(u1)) => human1 * u1 * 2.0,
        (None, None) => f64::NAN,
    }
}

/// Fail-closed liquidity gate: a pool passes ONLY if its USD liquidity is
/// provably finite and at or above the floor. `NaN` (unvaluable) and any
/// sub-threshold value both fail, so a degraded/throttled RPC that cannot value
/// pools produces "no data" rather than a dust-filled mirage graph.
fn passes_liquidity_gate(liquidity_usd: f64, min_liquidity_usd: f64) -> bool {
    liquidity_usd >= min_liquidity_usd
}

/// The USD liquidity floor a pool must clear: the blue-chip floor only when BOTH
/// tokens are curated blue-chips, otherwise the higher long-tail floor (long-tail
/// prices are easier to manipulate and need more depth to be trusted).
fn min_liquidity_for_pool(
    token0: Address,
    token1: Address,
    registry: &[TokenInfo],
    blue_chip_floor: f64,
    longtail_floor: f64,
) -> f64 {
    if is_blue_chip(token0, registry) && is_blue_chip(token1, registry) {
        blue_chip_floor
    } else {
        longtail_floor
    }
}

/// V2 reserves-vs-`balanceOf` consistency check (a non-standard-token guard).
///
/// A UniswapV2/Sushi pool sets its stored reserves to its real token balances on
/// every sync, so `balanceOf(pool) >= reserve` holds for a standard ERC-20
/// (direct donations can only push the balance ABOVE the reserve). A live balance
/// materially BELOW the stored reserve signals a fee-on-transfer / rebasing /
/// otherwise non-standard token whose transfer math would fabricate profit, so it
/// fails closed. `reserve == 0` is treated as consistent (an empty leg is handled
/// by the liquidity gate, not here).
fn v2_balance_consistent(reserve_raw: U256, balance_raw: U256, tolerance_pct: f64) -> bool {
    if reserve_raw.is_zero() {
        return true;
    }
    let reserve = u256_to_f64(reserve_raw);
    let balance = u256_to_f64(balance_raw);
    if !reserve.is_finite() || !balance.is_finite() {
        return false;
    }
    balance >= reserve * (1.0 - tolerance_pct / 100.0)
}

/// Static metadata shared by both directional edges of a pool, including the raw
/// integer swap state the Phase 4 simulator needs to model price impact.
struct EdgeMeta<'a> {
    liquidity_usd: f64,
    dex: &'a str,
    is_v3: bool,
    fee: u32,
    router: Address,
    /// Decimals of `token0` / `token1` (the canonical, address-sorted ordering).
    dec0: u8,
    dec1: u8,
    /// V2 raw reserves of `token0` / `token1`; zero for V3.
    reserve0: U256,
    reserve1: U256,
    /// V3 concentrated-liquidity state; defaults for V2.
    sqrt_price_x96: U256,
    liquidity: u128,
    tick: i32,
    /// V3 cross-tick data (initialized ticks + fetched window) enabling the full
    /// tick-by-tick swap walk; `None` for V2 or when no ticks were fetched.
    cross_tick: Option<V3CrossTickData>,
}

/// Build both directional edges for a pool given the `token1`-per-`token0`
/// human price. Returns `None` if either direction is implausible.
fn build_bidirectional_edges(
    token0: Address,
    token1: Address,
    price1_per_0: f64,
    meta: &EdgeMeta<'_>,
) -> Option<[PoolEdge; 2]> {
    let &EdgeMeta {
        liquidity_usd,
        dex,
        is_v3,
        fee,
        router,
        dec0,
        dec1,
        reserve0,
        reserve1,
        sqrt_price_x96,
        liquidity,
        tick,
        ref cross_tick,
    } = meta;
    if !is_price_plausible(price1_per_0) {
        warn!(
            "Skipping {dex} pool {token0:#x}/{token1:#x}: implausible price {price1_per_0:e} (decimals/ordering guard)"
        );
        return None;
    }
    let price0_per_1 = 1.0 / price1_per_0;
    if !is_price_plausible(price0_per_1) {
        warn!(
            "Skipping {dex} pool {token0:#x}/{token1:#x}: implausible inverse price {price0_per_1:e}"
        );
        return None;
    }

    // Forward hop sells token0 for token1 (price DOWN) => zero_for_one = true.
    let forward_state = PoolSwapState {
        dec_in: dec0,
        dec_out: dec1,
        reserve_in: reserve0,
        reserve_out: reserve1,
        sqrt_price_x96,
        liquidity,
        tick,
        zero_for_one: true,
        cross_tick: cross_tick.clone(),
    };
    // Backward hop sells token1 for token0 (price UP) => zero_for_one = false.
    let backward_state = PoolSwapState {
        dec_in: dec1,
        dec_out: dec0,
        reserve_in: reserve1,
        reserve_out: reserve0,
        sqrt_price_x96,
        liquidity,
        tick,
        zero_for_one: false,
        cross_tick: cross_tick.clone(),
    };

    let forward = PoolEdge {
        token_in: token0,
        token_out: token1,
        price: price1_per_0,
        liquidity_usd,
        dex: dex.to_string(),
        network: "arbitrum".to_string(),
        router,
        is_v3,
        fee,
        swap_state: Some(forward_state),
    };
    let backward = PoolEdge {
        token_in: token1,
        token_out: token0,
        price: price0_per_1,
        liquidity_usd,
        dex: dex.to_string(),
        network: "arbitrum".to_string(),
        router,
        is_v3,
        fee,
        swap_state: Some(backward_state),
    };
    Some([forward, backward])
}

// ---------------------------------------------------------------------------
// Resilient JSON-RPC client
// ---------------------------------------------------------------------------

/// A small JSON-RPC client with a request timeout and a bounded retry loop,
/// tuned for a rate-limited free RPC tier. Uses JSON-RPC batch arrays to fold
/// many `eth_call`s into a single HTTP request.
pub struct RpcClient {
    url: String,
    http: reqwest::Client,
    retries: usize,
}

/// One `eth_call` target: contract address + ABI-encoded calldata, tagged with
/// a caller-chosen key so batched results can be matched back.
#[derive(Debug, Clone)]
pub struct Call {
    pub key: usize,
    pub to: Address,
    pub data: Vec<u8>,
}

impl RpcClient {
    pub fn new(url: String, timeout: Duration, retries: usize) -> eyre::Result<Self> {
        let http = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self { url, http, retries })
    }

    fn encode_call(to: Address, data: &[u8]) -> Value {
        serde_json::json!({
            "to": format!("{to:#x}"),
            "data": format!("0x{}", hex::encode(data)),
        })
    }

    /// Execute a batch of `eth_call`s. Returns a vector of `(key, result_bytes)`
    /// for every call that returned non-empty data. Missing/failed calls are
    /// simply omitted (they are treated as "pool absent").
    pub async fn batch_eth_call(&self, calls: &[Call]) -> eyre::Result<Vec<(usize, Vec<u8>)>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }

        let batch: Vec<Value> = calls
            .iter()
            .map(|c| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": c.key,
                    "method": "eth_call",
                    "params": [Self::encode_call(c.to, &c.data), "latest"],
                })
            })
            .collect();

        let body = Value::Array(batch);
        let mut last_err = None;

        for attempt in 0..=self.retries {
            match self.post(&body).await {
                Ok(resp) => return Ok(Self::parse_batch(resp)),
                Err(e) => {
                    warn!("RPC batch attempt {} failed: {e}", attempt + 1);
                    last_err = Some(e);
                    // Small backoff for rate limits.
                    tokio::time::sleep(Duration::from_millis(400 * (attempt as u64 + 1))).await;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| eyre::eyre!("RPC batch failed with no error")))
    }

    async fn post(&self, body: &Value) -> eyre::Result<Value> {
        let resp = self.http.post(&self.url).json(body).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(eyre::eyre!("RPC HTTP {status}: {}", truncate(&text, 200)));
        }
        let json: Value = serde_json::from_str(&text)
            .map_err(|e| eyre::eyre!("RPC bad JSON: {e}: {}", truncate(&text, 200)))?;
        Ok(json)
    }

    fn parse_batch(resp: Value) -> Vec<(usize, Vec<u8>)> {
        let mut out = Vec::new();
        let entries = match resp {
            Value::Array(a) => a,
            other => vec![other],
        };
        for entry in entries {
            let id = entry.get("id").and_then(|v| v.as_u64()).map(|v| v as usize);
            let result = entry.get("result").and_then(|v| v.as_str());
            if let (Some(id), Some(result)) = (id, result) {
                if let Some(bytes) = decode_hex(result) {
                    if !bytes.is_empty() {
                        out.push((id, bytes));
                    }
                }
            } else if let Some(err) = entry.get("error") {
                debug!("RPC call error for id {:?}: {err}", id);
            }
        }
        out
    }

    /// Fetch the latest block number as a liveness/health check.
    pub async fn block_number(&self) -> eyre::Result<u64> {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_blockNumber", "params": []
        });
        let resp = self.post(&body).await?;
        let hex = resp
            .get("result")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("eth_blockNumber returned no result"))?;
        let n = u64::from_str_radix(hex.trim_start_matches("0x"), 16)?;
        Ok(n)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_start_matches("0x");
    hex::decode(s).ok()
}

// ---------------------------------------------------------------------------
// ABI encoding helpers (minimal, no external ABI crate needed)
// ---------------------------------------------------------------------------

fn encode_address(a: Address) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(a.as_bytes());
    out
}

fn encode_uint24(v: u32) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[29] = (v >> 16) as u8;
    out[30] = (v >> 8) as u8;
    out[31] = v as u8;
    out
}

/// Encode a signed integer as a 32-byte big-endian two's-complement ABI word
/// (sign-extended), for `int16` / `int24` parameters.
fn encode_int256(v: i64) -> [u8; 32] {
    let mut out = if v < 0 { [0xFFu8; 32] } else { [0u8; 32] };
    out[24..32].copy_from_slice(&v.to_be_bytes());
    out
}

fn calldata_tick_bitmap(word_pos: i16) -> Vec<u8> {
    let mut data = Vec::with_capacity(4 + 32);
    data.extend_from_slice(&SEL_TICK_BITMAP);
    data.extend_from_slice(&encode_int256(word_pos as i64));
    data
}

fn calldata_ticks(tick: i32) -> Vec<u8> {
    let mut data = Vec::with_capacity(4 + 32);
    data.extend_from_slice(&SEL_TICKS);
    data.extend_from_slice(&encode_int256(tick as i64));
    data
}

fn calldata_get_pool(token_a: Address, token_b: Address, fee: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity(4 + 96);
    data.extend_from_slice(&SEL_GET_POOL);
    data.extend_from_slice(&encode_address(token_a));
    data.extend_from_slice(&encode_address(token_b));
    data.extend_from_slice(&encode_uint24(fee));
    data
}

fn calldata_get_pair(token_a: Address, token_b: Address) -> Vec<u8> {
    let mut data = Vec::with_capacity(4 + 64);
    data.extend_from_slice(&SEL_GET_PAIR);
    data.extend_from_slice(&encode_address(token_a));
    data.extend_from_slice(&encode_address(token_b));
    data
}

fn calldata_balance_of(holder: Address) -> Vec<u8> {
    let mut data = Vec::with_capacity(4 + 32);
    data.extend_from_slice(&SEL_BALANCE_OF);
    data.extend_from_slice(&encode_address(holder));
    data
}

/// `decimals()` takes no arguments, so the calldata is just the 4-byte selector.
fn calldata_decimals() -> Vec<u8> {
    SEL_DECIMALS.to_vec()
}

/// Decode an ERC-20 `decimals()` return (a `uint8` right-aligned in a 32-byte
/// word). Returns `None` for a malformed/oversized value so an unreadable token
/// is dropped rather than assigned a guessed decimals.
fn decode_decimals(bytes: &[u8]) -> Option<u8> {
    let v = decode_uint256(bytes)?;
    let raw = v.low_u64();
    if v > U256::from(MAX_PLAUSIBLE_DECIMALS) || raw > MAX_PLAUSIBLE_DECIMALS {
        return None;
    }
    Some(raw as u8)
}

/// Decode a single uint256 return word.
fn decode_uint256(bytes: &[u8]) -> Option<U256> {
    if bytes.len() < 32 {
        return None;
    }
    Some(U256::from_big_endian(&bytes[0..32]))
}

/// Decode a 32-byte-aligned address from an ABI return word.
fn decode_address(bytes: &[u8]) -> Option<Address> {
    if bytes.len() < 32 {
        return None;
    }
    Some(Address::from_slice(&bytes[12..32]))
}

/// Decode `slot0()` into `(sqrtPriceX96, tick)`. The second 32-byte word holds
/// the current `int24` tick, sign-extended to 256 bits, so we read the low three
/// bytes and sign-extend from bit 23.
fn decode_slot0_sqrt_price_and_tick(bytes: &[u8]) -> Option<(U256, i32)> {
    if bytes.len() < 64 {
        return None;
    }
    let sqrt = U256::from_big_endian(&bytes[0..32]);
    let w1 = &bytes[32..64];
    let mut tick =
        ((w1[29] as i32) << 16) | ((w1[30] as i32) << 8) | (w1[31] as i32);
    if tick & 0x0080_0000 != 0 {
        // Negative int24: sign-extend into the upper bits of the i32.
        tick |= !0x00FF_FFFFi32;
    }
    Some((sqrt, tick))
}

/// Decode a `uint128` return word (e.g. `liquidity()`), taking the low 128 bits.
fn decode_uint128(bytes: &[u8]) -> Option<u128> {
    decode_uint256(bytes).map(|v| v.low_u128())
}

/// Decode the `int128 liquidityNet` (the 2nd field) from a `ticks(int24)` return.
/// It is ABI-encoded as an int128 sign-extended to 256 bits; we validate the sign
/// extension and return `None` (fail closed) on any malformed word.
fn decode_liquidity_net(bytes: &[u8]) -> Option<i128> {
    if bytes.len() < 64 {
        return None;
    }
    let word = &bytes[32..64];
    let low = &word[16..32];
    let negative = low[0] & 0x80 != 0;
    let fill = if negative { 0xFFu8 } else { 0x00u8 };
    if word[0..16].iter().any(|&b| b != fill) {
        return None; // sign extension inconsistent => not a clean int128
    }
    let mut buf = [0u8; 16];
    buf.copy_from_slice(low);
    Some(i128::from_be_bytes(buf))
}

/// Decode `getReserves()` -> (reserve0 uint112, reserve1 uint112, ...).
fn decode_reserves(bytes: &[u8]) -> Option<(U256, U256)> {
    if bytes.len() < 64 {
        return None;
    }
    let r0 = U256::from_big_endian(&bytes[0..32]);
    let r1 = U256::from_big_endian(&bytes[32..64]);
    Some((r0, r1))
}

// ---------------------------------------------------------------------------
// Discovery + state read
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PoolCandidate {
    token0: Address,
    token1: Address,
    dec0: u8,
    dec1: u8,
    is_v3: bool,
    fee: u32,
    dex: String,
    factory: Address,
}

/// Sort a pair by ascending address to match on-chain `token0`/`token1`.
fn sorted_pair(a: TokenInfo, b: TokenInfo) -> (TokenInfo, TokenInfo) {
    if a.address < b.address {
        (a, b)
    } else {
        (b, a)
    }
}

/// Fetch a full set of live Arbitrum pool edges across Uniswap V3 (500/3000/
/// 10000 tiers) and SushiSwap V2 for the registered token universe.
pub async fn fetch_arbitrum_pools(rpc_url: &str) -> eyre::Result<Vec<PoolEdge>> {
    Ok(fetch_arbitrum_pools_with_usd(rpc_url).await?.0)
}

/// Like [`fetch_arbitrum_pools`] but also returns the derived per-token USD price
/// map (stablecoins anchored at $1, relaxed across observed edge prices). Phase 4
/// uses this map to size a USD flash-loan into the start token's raw base units
/// before running the price-impact simulator.
pub async fn fetch_arbitrum_pools_with_usd(
    rpc_url: &str,
) -> eyre::Result<(Vec<PoolEdge>, std::collections::HashMap<Address, f64>)> {
    let (edges, usd_prices, _block, _tele) = fetch_arbitrum_pools_with_usd_at_block(rpc_url).await?;
    Ok((edges, usd_prices))
}

/// Like [`fetch_arbitrum_pools_with_usd`] but also returns the Arbitrum block
/// number the pool state was read at, so a caller (e.g. the Phase 7 multi-block
/// sampler) can report each sample's block honestly. This is the canonical
/// implementation; the two-tuple variant above simply drops the block.
pub async fn fetch_arbitrum_pools_with_usd_at_block(
    rpc_url: &str,
) -> eyre::Result<(
    Vec<PoolEdge>,
    std::collections::HashMap<Address, f64>,
    u64,
    DiscoveryTelemetry,
)> {
    let client = RpcClient::new(rpc_url.to_string(), Duration::from_secs(15), 2)?;

    // Liveness check first so failures are clear and early.
    let block = client.block_number().await?;
    info!("Connected to Arbitrum RPC at block {block}");

    // Honest throttle-vs-gate telemetry for this fetch (reporting only; never
    // influences a gating decision). Populated as the pipeline progresses.
    let mut tele = DiscoveryTelemetry { block, ..Default::default() };

    // 0) Assemble the EFFECTIVE token universe (Phase 9).
    //    0a) Cap the registry (blue-chips always kept) to bound O(N^2) discovery.
    //    0b) Read ERC-20 decimals() ON-CHAIN for every token; blue-chips fall back
    //        to their verified constant, long-tail tokens with unreadable decimals
    //        are DROPPED (fail-closed — a new token's decimals is never assumed).
    //    0c) Drop any denylisted (known non-standard) token unconditionally.
    let registry_all = arbitrum_tokens();
    let max_tokens = env_usize("SCANNER_MAX_TOKENS", DEFAULT_MAX_TOKENS);
    let registry_capped = cap_tokens(&registry_all, max_tokens);
    let capped_addrs: Vec<Address> = registry_capped.iter().map(|t| t.address).collect();
    let onchain_decimals = read_onchain_decimals(&client, &capped_addrs).await;
    let (mut tokens, dropped_no_decimals) =
        resolve_effective_tokens(&registry_capped, &onchain_decimals);
    let denylist = fee_on_transfer_denylist();
    let before_denylist = tokens.len();
    tokens.retain(|t| !is_denylisted(t.address, denylist));
    tele.registry_tokens = registry_all.len();
    tele.tokens_loaded = tokens.len();
    tele.bluechip_loaded = tokens.iter().filter(|t| t.blue_chip).count();
    tele.longtail_loaded = tokens.iter().filter(|t| !t.blue_chip).count();
    tele.tokens_dropped_no_decimals = dropped_no_decimals.len();
    tele.tokens_dropped_denylist = before_denylist - tokens.len();
    info!(
        "Token universe: {} effective ({} blue-chip + {} long-tail) from {} registered; \
         dropped {} long-tail with unreadable on-chain decimals (fail-closed), cap={}",
        tokens.len(),
        tokens.iter().filter(|t| t.blue_chip).count(),
        tokens.iter().filter(|t| !t.blue_chip).count(),
        registry_all.len(),
        dropped_no_decimals.len(),
        max_tokens
    );
    let v3_factory = addr(UNISWAP_V3_FACTORY);
    let sushi_factory = addr(SUSHISWAP_V2_FACTORY);

    // 1) Build discovery candidates for every unordered token pair.
    let mut candidates: Vec<PoolCandidate> = Vec::new();
    for i in 0..tokens.len() {
        for j in (i + 1)..tokens.len() {
            let (t0, t1) = sorted_pair(tokens[i], tokens[j]);
            for fee in V3_FEE_TIERS {
                candidates.push(PoolCandidate {
                    token0: t0.address,
                    token1: t1.address,
                    dec0: t0.decimals,
                    dec1: t1.decimals,
                    is_v3: true,
                    fee,
                    dex: "uniswap-v3".to_string(),
                    factory: v3_factory,
                });
            }
            candidates.push(PoolCandidate {
                token0: t0.address,
                token1: t1.address,
                dec0: t0.decimals,
                dec1: t1.decimals,
                is_v3: false,
                fee: V2_FEE_PPM,
                dex: "sushiswap".to_string(),
                factory: sushi_factory,
            });
        }
    }

    // 2) Discover pool addresses via factory getPool/getPair (batched).
    let discovery_calls: Vec<Call> = candidates
        .iter()
        .enumerate()
        .map(|(idx, c)| Call {
            key: idx,
            to: c.factory,
            data: if c.is_v3 {
                calldata_get_pool(c.token0, c.token1, c.fee)
            } else {
                calldata_get_pair(c.token0, c.token1)
            },
        })
        .collect();

    let (discovery_results, discovery_failed) = batch_in_chunks(&client, &discovery_calls, 40).await;
    tele.discovery_probes = discovery_calls.len();
    tele.discovery_probes_failed = discovery_failed;
    if !discovery_calls.is_empty() {
        info!(
            "Discovery batch: {}/{} probe call(s) failed ({:.1}%)",
            discovery_failed,
            discovery_calls.len(),
            tele.discovery_fail_pct()
        );
    }
    if !discovery_calls.is_empty()
        && discovery_failed as f64 / discovery_calls.len() as f64 > MAX_BATCH_FAILURE_FRACTION
    {
        return Err(eyre::eyre!(
            "Aborting: {}/{} pool-discovery calls failed (> {:.0}%); RPC appears degraded/throttled. \
             Refusing to build a partial pool graph.",
            discovery_failed,
            discovery_calls.len(),
            MAX_BATCH_FAILURE_FRACTION * 100.0
        ));
    }

    // idx -> pool address
    let mut pools: Vec<(PoolCandidate, Address)> = Vec::new();
    for (idx, bytes) in discovery_results {
        if let Some(pool_addr) = decode_address(&bytes) {
            if pool_addr != Address::zero() {
                pools.push((candidates[idx].clone(), pool_addr));
            }
        }
    }
    tele.pools_discovered = pools.len();
    info!(
        "Discovered {} live pools out of {} probed pair/venue combinations",
        pools.len(),
        candidates.len()
    );

    // 3) Read state for each discovered pool (batched). Per pool we issue THREE
    //    calls, keyed `idx*3 + {0,1,2}`:
    //      +0: price   (slot0 for V3, getReserves for V2)
    //      +1: balanceOf(pool) on token0  -> real on-chain reserve of token0
    //      +2: balanceOf(pool) on token1  -> real on-chain reserve of token1
    //      +3: liquidity() (V3 only)      -> in-range L for concentrated swap math
    //    Balances let us value pool liquidity in USD and skip dust pools whose
    //    stale marginal price would otherwise manufacture fake spreads. The
    //    stride-4 keying leaves the +3 slot unused for V2 pools.
    let mut state_calls: Vec<Call> = Vec::with_capacity(pools.len() * 4);
    for (idx, (c, pool_addr)) in pools.iter().enumerate() {
        state_calls.push(Call {
            key: idx * 4,
            to: *pool_addr,
            data: if c.is_v3 {
                SEL_SLOT0.to_vec()
            } else {
                SEL_GET_RESERVES.to_vec()
            },
        });
        state_calls.push(Call {
            key: idx * 4 + 1,
            to: c.token0,
            data: calldata_balance_of(*pool_addr),
        });
        state_calls.push(Call {
            key: idx * 4 + 2,
            to: c.token1,
            data: calldata_balance_of(*pool_addr),
        });
        if c.is_v3 {
            state_calls.push(Call {
                key: idx * 4 + 3,
                to: *pool_addr,
                data: SEL_LIQUIDITY.to_vec(),
            });
        }
    }

    let (state_results, state_failed) = batch_in_chunks(&client, &state_calls, 60).await;
    tele.state_calls = state_calls.len();
    tele.state_calls_failed = state_failed;
    if !state_calls.is_empty() {
        info!(
            "State batch: {}/{} state/balance call(s) failed ({:.1}%)",
            state_failed,
            state_calls.len(),
            tele.state_fail_pct()
        );
    }
    if !state_calls.is_empty()
        && state_failed as f64 / state_calls.len() as f64 > MAX_BATCH_FAILURE_FRACTION
    {
        return Err(eyre::eyre!(
            "Aborting: {}/{} pool state/balance calls failed (> {:.0}%); RPC appears degraded/throttled. \
             Refusing to build a partial pool graph (this is exactly the condition that let the dust \
             guard go inert and surface mirage cycles).",
            state_failed,
            state_calls.len(),
            MAX_BATCH_FAILURE_FRACTION * 100.0
        ));
    }

    // Fold results back per pool index.
    let mut price_bytes: std::collections::HashMap<usize, Vec<u8>> = std::collections::HashMap::new();
    let mut bal0: std::collections::HashMap<usize, U256> = std::collections::HashMap::new();
    let mut bal1: std::collections::HashMap<usize, U256> = std::collections::HashMap::new();
    let mut v3_liquidity: std::collections::HashMap<usize, u128> = std::collections::HashMap::new();
    for (key, bytes) in state_results {
        match key % 4 {
            0 => {
                price_bytes.insert(key / 4, bytes);
            }
            1 => {
                if let Some(v) = decode_uint256(&bytes) {
                    bal0.insert(key / 4, v);
                }
            }
            2 => {
                if let Some(v) = decode_uint256(&bytes) {
                    bal1.insert(key / 4, v);
                }
            }
            _ => {
                if let Some(v) = decode_uint128(&bytes) {
                    v3_liquidity.insert(key / 4, v);
                }
            }
        }
    }

    // Phase 9 tuning knobs (env-overridable), read once.
    let blue_chip_floor = env_f64("SCANNER_MIN_POOL_LIQUIDITY_USD", DEFAULT_MIN_POOL_LIQUIDITY_USD);
    let longtail_floor =
        env_f64("SCANNER_MIN_LONGTAIL_POOL_LIQUIDITY_USD", DEFAULT_MIN_LONGTAIL_POOL_LIQUIDITY_USD);
    let max_pools = env_usize("SCANNER_MAX_POOLS", DEFAULT_MAX_POOLS);
    let min_anchor_venues = env_usize("SCANNER_MIN_ANCHOR_VENUES", DEFAULT_MIN_ANCHOR_VENUES);
    let anchor_outlier_pct = env_f64("SCANNER_ANCHOR_OUTLIER_PCT", DEFAULT_ANCHOR_OUTLIER_PCT);
    let v2_balance_tol =
        env_f64("SCANNER_V2_BALANCE_TOLERANCE_PCT", DEFAULT_V2_BALANCE_TOLERANCE_PCT);

    // 4) Compute each pool's `token1_per_token0` price + human reserves + the raw
    //    integer swap state (V2 reserves / V3 sqrtPrice+L+tick) the simulator needs.
    //    Cross-tick data is fetched later (after the liquidity gate + pool cap) so
    //    that RPC is spent only on pools that actually survive.
    struct PricedPool {
        candidate: PoolCandidate,
        pool_addr: Address,
        price1_per_0: f64,
        human0: f64,
        human1: f64,
        /// V2 raw reserves (reserve0, reserve1); zero for V3.
        reserve0: U256,
        reserve1: U256,
        /// V3 concentrated-liquidity state; defaults for V2.
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
    }
    // (d) Tokens flagged non-standard by the V2 reserves-vs-balanceOf check; every
    //     pool touching one is dropped after this pass (fail-closed).
    let mut nonstandard: std::collections::HashSet<Address> = std::collections::HashSet::new();
    let mut priced: Vec<PricedPool> = Vec::new();
    for (idx, (candidate, pool_addr)) in pools.iter().enumerate() {
        let Some(bytes) = price_bytes.get(&idx) else {
            tele.pools_dropped_incomplete_state += 1;
            continue;
        };
        let mut reserve0 = U256::zero();
        let mut reserve1 = U256::zero();
        let mut sqrt_price_x96 = U256::zero();
        let mut liquidity = 0u128;
        let mut tick = 0i32;
        let price1_per_0 = if candidate.is_v3 {
            match decode_slot0_sqrt_price_and_tick(bytes) {
                Some((sqrt, tk)) => {
                    // A V3 pool with no in-range liquidity cannot be simulated; a
                    // missing liquidity() read must NOT be treated as zero and let
                    // through, so require it to have decoded.
                    let Some(l) = v3_liquidity.get(&idx).copied() else {
                        tele.pools_dropped_incomplete_state += 1;
                        debug!(
                            "Skipping {} pool {:#x}/{:#x}: missing liquidity() result (degraded RPC)",
                            candidate.dex, candidate.token0, candidate.token1
                        );
                        continue;
                    };
                    sqrt_price_x96 = sqrt;
                    liquidity = l;
                    tick = tk;
                    v3_price_token1_per_token0(sqrt, candidate.dec0, candidate.dec1)
                }
                None => {
                    tele.pools_dropped_bad_price += 1;
                    continue;
                }
            }
        } else {
            match decode_reserves(bytes) {
                Some((r0, r1)) => {
                    reserve0 = r0;
                    reserve1 = r1;
                    v2_price_token1_per_token0(r0, r1, candidate.dec0, candidate.dec1)
                }
                None => {
                    tele.pools_dropped_bad_price += 1;
                    continue;
                }
            }
        };
        if !is_price_plausible(price1_per_0) {
            tele.pools_dropped_bad_price += 1;
            warn!(
                "Skipping {} pool {:#x}/{:#x}: implausible price {price1_per_0:e} (decimals/ordering guard)",
                candidate.dex, candidate.token0, candidate.token1
            );
            continue;
        }
        // Require BOTH balance reads to have decoded. A dropped balanceOf on a
        // degraded RPC must NOT be silently treated as a zero reserve — skip the
        // pool so it can never be valued (and emitted) from partial data.
        let (Some(raw0), Some(raw1)) = (bal0.get(&idx), bal1.get(&idx)) else {
            tele.pools_dropped_incomplete_state += 1;
            debug!(
                "Skipping {} pool {:#x}/{:#x}: missing balanceOf result(s) (degraded RPC)",
                candidate.dex, candidate.token0, candidate.token1
            );
            continue;
        };
        let human0 = to_human(*raw0, candidate.dec0);
        let human1 = to_human(*raw1, candidate.dec1);
        // (d) NON-STANDARD TOKEN GUARD (V2 only): a UniswapV2/Sushi pool syncs its
        //     stored reserves to its real balances, so a live balanceOf materially
        //     BELOW the stored reserve signals a fee-on-transfer / rebasing token
        //     whose transfer math would fabricate profit. Flag it; every pool
        //     touching it is dropped below. V3 has no comparable stored reserve, so
        //     those tokens rely on the denylist + consensus-anchor gate instead.
        if !candidate.is_v3 {
            if !v2_balance_consistent(reserve0, *raw0, v2_balance_tol) {
                nonstandard.insert(candidate.token0);
            }
            if !v2_balance_consistent(reserve1, *raw1, v2_balance_tol) {
                nonstandard.insert(candidate.token1);
            }
        }
        priced.push(PricedPool {
            candidate: candidate.clone(),
            pool_addr: *pool_addr,
            price1_per_0,
            human0,
            human1,
            reserve0,
            reserve1,
            sqrt_price_x96,
            liquidity,
            tick,
        });
    }

    // (d) Drop every pool touching a non-standard token (including its V3 pools).
    if !nonstandard.is_empty() {
        let before = priced.len();
        priced.retain(|p| {
            !nonstandard.contains(&p.candidate.token0)
                && !nonstandard.contains(&p.candidate.token1)
        });
        tele.pools_dropped_nonstandard = before - priced.len();
        info!(
            "Dropped {} pool(s) touching {} non-standard token(s) (V2 reserves-vs-balanceOf divergence > {:.2}%)",
            before - priced.len(),
            nonstandard.len(),
            v2_balance_tol
        );
    }

    // 5) Derive live USD prices with a deepest-venue + median-consensus anchor
    //    (Phase 9). A token that cannot gather >= min_anchor_venues corroborating
    //    quotes within the outlier band is left UN-PRICED, so its pools fail the
    //    USD liquidity gate below and are dropped rather than valued on a guess.
    let pool_obs: Vec<PoolObs> = priced
        .iter()
        .map(|p| PoolObs {
            token0: p.candidate.token0,
            token1: p.candidate.token1,
            price1_per_0: p.price1_per_0,
            human0: p.human0,
            human1: p.human1,
        })
        .collect();
    let usd_prices = derive_usd_prices(&tokens, &pool_obs, min_anchor_venues, anchor_outlier_pct);

    // 6) Liquidity gate: value each pool in USD and keep only those at or above the
    //    applicable floor — the higher long-tail floor for any pool touching a
    //    non-blue-chip token, the blue-chip floor for blue-chip-only pools. NaN
    //    (no USD-anchored leg) and sub-threshold values both fail closed, so a
    //    degraded RPC or an un-priceable long-tail token cannot surface a mirage.
    let mut passing: Vec<(usize, f64)> = Vec::new();
    let mut skipped_dust = 0usize;
    let mut unpriced_liquidity = 0usize;
    for (idx, p) in priced.iter().enumerate() {
        let usd0 = usd_prices.get(&p.candidate.token0).copied();
        let usd1 = usd_prices.get(&p.candidate.token1).copied();
        if usd0.is_none() && usd1.is_none() {
            unpriced_liquidity += 1;
        }
        let liquidity_usd = value_pool_liquidity_usd(usd0, usd1, p.human0, p.human1);
        let floor = min_liquidity_for_pool(
            p.candidate.token0,
            p.candidate.token1,
            &tokens,
            blue_chip_floor,
            longtail_floor,
        );
        if !passes_liquidity_gate(liquidity_usd, floor) {
            skipped_dust += 1;
            debug!(
                "Skipping {} pool {:#x}/{:#x}: liquidity {} not >= ${:.0} (fail-closed)",
                p.candidate.dex,
                p.candidate.token0,
                p.candidate.token1,
                if liquidity_usd.is_finite() {
                    format!("${liquidity_usd:.2}")
                } else {
                    "unvaluable".to_string()
                },
                floor
            );
            continue;
        }
        passing.push((idx, liquidity_usd));
    }
    tele.pools_dropped_liquidity = skipped_dust;
    tele.pools_unpriced_leg = unpriced_liquidity;

    // 6b) Cap to the deepest SCANNER_MAX_POOLS pools to bound edge/cross-tick RPC.
    let before_cap = passing.len();
    let passing = cap_by_depth(passing, max_pools);
    tele.pools_capped = before_cap - passing.len();
    if passing.len() < before_cap {
        info!(
            "Capped pool set {} -> {} (kept deepest by USD liquidity, SCANNER_MAX_POOLS={})",
            before_cap,
            passing.len(),
            max_pools
        );
    }

    // 6c) Fetch bounded V3 cross-tick data (Phase 8) for the SURVIVING pools only,
    //     so that RPC is spent only on pools that will build edges. Any pool
    //     without complete, in-bounds data keeps `None` and is simulated exactly
    //     within its current interval (fail-closed — never a guess).
    let v3_refs: Vec<Option<V3PoolRef>> = passing
        .iter()
        .map(|(idx, _)| {
            let p = &priced[*idx];
            if p.candidate.is_v3 {
                Some(V3PoolRef { pool_addr: p.pool_addr, fee: p.candidate.fee, tick: p.tick })
            } else {
                None
            }
        })
        .collect();
    let cross_tick_data = fetch_cross_tick_data(&client, &v3_refs).await;
    let cross_tick_pools = cross_tick_data.iter().filter(|c| c.is_some()).count();
    info!(
        "Fetched V3 cross-tick data for {}/{} surviving V3 pools (others fall back to within-interval sim)",
        cross_tick_pools,
        v3_refs.iter().filter(|r| r.is_some()).count()
    );

    // 6d) Build bidirectional edges for the surviving, capped pools.
    let mut edges: Vec<PoolEdge> = Vec::new();
    let mut priced_pools = 0usize;
    for ((idx, liquidity_usd), cross_tick) in passing.iter().zip(cross_tick_data) {
        let p = &priced[*idx];
        if let Some(pair) = build_bidirectional_edges(
            p.candidate.token0,
            p.candidate.token1,
            p.price1_per_0,
            &EdgeMeta {
                liquidity_usd: *liquidity_usd,
                dex: &p.candidate.dex,
                is_v3: p.candidate.is_v3,
                fee: p.candidate.fee,
                router: p.pool_addr,
                dec0: p.candidate.dec0,
                dec1: p.candidate.dec1,
                reserve0: p.reserve0,
                reserve1: p.reserve1,
                sqrt_price_x96: p.sqrt_price_x96,
                liquidity: p.liquidity,
                tick: p.tick,
                cross_tick,
            },
        ) {
            priced_pools += 1;
            edges.extend(pair);
        }
    }

    info!(
        "Built {} directional edges from {} pools (fail-closed: skipped {} dust/unvaluable pools; \
         {} had no USD-anchored leg; blue-chip floor ${:.0}, long-tail floor ${:.0})",
        edges.len(),
        priced_pools,
        skipped_dust,
        unpriced_liquidity,
        blue_chip_floor,
        longtail_floor
    );
    tele.pools_kept = priced_pools;
    tele.edges_loaded = edges.len();
    info!(
        "Discovery telemetry @ block {}: tokens_loaded={} edges_loaded={} | throttle_drops={} \
         (discovery_failed={}/{} state_failed={}/{} incomplete_state={}) | gate_drops={} \
         (no_decimals={} denylist={} bad_price={} nonstandard={} liquidity={}) | throttle_suspected={}",
        tele.block,
        tele.tokens_loaded,
        tele.edges_loaded,
        tele.throttle_drops(),
        tele.discovery_probes_failed,
        tele.discovery_probes,
        tele.state_calls_failed,
        tele.state_calls,
        tele.pools_dropped_incomplete_state,
        tele.gate_drops(),
        tele.tokens_dropped_no_decimals,
        tele.tokens_dropped_denylist,
        tele.pools_dropped_bad_price,
        tele.pools_dropped_nonstandard,
        tele.pools_dropped_liquidity,
        tele.throttle_suspected()
    );

    Ok((edges, usd_prices, block, tele))
}

/// Split a large set of calls into fixed-size batches to stay friendly with
/// rate limits and payload-size limits. Chunks run sequentially (modest
/// concurrency) to avoid hammering a free RPC tier.
///
/// Returns the decoded results alongside the number of individual calls that
/// belonged to a chunk that failed outright — callers use that count to detect
/// a degraded/throttled RPC and fail closed rather than emitting a graph built
/// from partial data.
async fn batch_in_chunks(
    client: &RpcClient,
    calls: &[Call],
    chunk_size: usize,
) -> (Vec<(usize, Vec<u8>)>, usize) {
    let mut out = Vec::new();
    let mut failed_calls = 0usize;
    for chunk in calls.chunks(chunk_size) {
        match client.batch_eth_call(chunk).await {
            Ok(mut results) => out.append(&mut results),
            Err(e) => {
                failed_calls += chunk.len();
                warn!("Batch chunk failed (skipping {} calls): {e}", chunk.len());
            }
        }
    }
    (out, failed_calls)
}

/// Read ERC-20 `decimals()` ON-CHAIN for every token (batched). Returns
/// `address -> decimals` only for tokens whose value decoded to a plausible
/// number; a token absent from the map could not be read this run. Fail-closed by
/// OMISSION: the caller drops any long-tail token missing here rather than
/// assuming its decimals, while a blue-chip falls back to its verified constant.
/// A degraded/throttled batch therefore shrinks the long-tail toward the safe
/// blue-chip baseline instead of fabricating decimals.
async fn read_onchain_decimals(
    client: &RpcClient,
    tokens: &[Address],
) -> std::collections::HashMap<Address, u8> {
    let calls: Vec<Call> = tokens
        .iter()
        .enumerate()
        .map(|(idx, address)| Call { key: idx, to: *address, data: calldata_decimals() })
        .collect();
    let (results, _failed) = batch_in_chunks(client, &calls, 60).await;
    let mut out = std::collections::HashMap::new();
    for (key, bytes) in results {
        if let Some(d) = decode_decimals(&bytes) {
            if let Some(address) = tokens.get(key) {
                out.insert(*address, d);
            }
        }
    }
    out
}

/// Phase 8 cross-tick fetch bounds. We read `2*V3_WORD_RADIUS+1` tick-bitmap words
/// centred on each V3 pool's current word, decode the initialized ticks in that
/// range, and fetch their `liquidityNet`. A pool with more than
/// `V3_MAX_TICKS_PER_POOL` initialized ticks in range, any decode gap, or a
/// degraded RPC gets NO cross-tick data and falls back to exact within-interval
/// simulation (fail-closed — never a guess). One word spans 256 spacing-intervals,
/// so even a single word each side covers a very wide price range for the
/// canonical 10 / 60 / 200 tick spacings.
const V3_WORD_RADIUS: i32 = 1;
const V3_MAX_TICKS_PER_POOL: usize = 256;
const V3_MIN_TICK: i32 = -887272;
const V3_MAX_TICK: i32 = 887272;

/// Canonical Uniswap V3 tick spacing for a fee tier (ppm); unknown tiers => `None`.
fn spacing_for_fee(fee_ppm: u32) -> Option<i32> {
    match fee_ppm {
        100 => Some(1),
        500 => Some(10),
        3000 => Some(60),
        10000 => Some(200),
        _ => None,
    }
}

/// A discovered V3 pool needing cross-tick data, aligned to a `priced` index.
struct V3PoolRef {
    pool_addr: Address,
    fee: u32,
    tick: i32,
}

/// Per-pool cross-tick fetch plan: derived spacing, the lowest bitmap word we
/// fetch, and the tick window that fetch fully covers.
struct CtPlan {
    spacing: i32,
    lo_word: i32,
    window: (i32, i32),
}

/// Bounded bitmap-word plan + fully-covered tick window for a pool at `tick`.
fn cross_tick_plan(tick: i32, spacing: i32) -> CtPlan {
    let compressed = tick.div_euclid(spacing);
    let word_pos = compressed.div_euclid(256);
    let lo_word = word_pos - V3_WORD_RADIUS;
    let hi_word = word_pos + V3_WORD_RADIUS;
    let window_lo = (lo_word * 256 * spacing).max(V3_MIN_TICK);
    let window_hi = ((hi_word * 256 + 255) * spacing).min(V3_MAX_TICK);
    CtPlan { spacing, lo_word, window: (window_lo, window_hi) }
}

/// Fetch bounded Uniswap V3 cross-tick data (initialized ticks + `liquidityNet`)
/// for each V3 pool, aligned to `refs` (V2 entries are `None`). Every failure mode
/// — degraded RPC, a decode gap, an over-cap pool, an unknown fee tier — yields
/// `None` for that pool (fail-closed to within-interval sim), never a partial or
/// guessed result.
async fn fetch_cross_tick_data(
    client: &RpcClient,
    refs: &[Option<V3PoolRef>],
) -> Vec<Option<V3CrossTickData>> {
    const NWORDS: usize = (2 * V3_WORD_RADIUS + 1) as usize;
    let mut out: Vec<Option<V3CrossTickData>> = (0..refs.len()).map(|_| None).collect();

    // Stage A: one tick-bitmap word per (pool, word offset).
    let mut plans: std::collections::HashMap<usize, CtPlan> = std::collections::HashMap::new();
    let mut bitmap_calls: Vec<Call> = Vec::new();
    for (pi, r) in refs.iter().enumerate() {
        let Some(r) = r else { continue };
        let Some(spacing) = spacing_for_fee(r.fee) else { continue };
        let plan = cross_tick_plan(r.tick, spacing);
        for wi in 0..NWORDS {
            let word = plan.lo_word + wi as i32;
            bitmap_calls.push(Call {
                key: pi * NWORDS + wi,
                to: r.pool_addr,
                data: calldata_tick_bitmap(word as i16),
            });
        }
        plans.insert(pi, plan);
    }
    if bitmap_calls.is_empty() {
        return out;
    }
    let (bitmap_results, bitmap_failed) = batch_in_chunks(client, &bitmap_calls, 60).await;
    if bitmap_failed as f64 / bitmap_calls.len() as f64 > MAX_BATCH_FAILURE_FRACTION {
        warn!(
            "Skipping V3 cross-tick data: {}/{} tick-bitmap calls failed (> {:.0}%); \
             falling back to within-interval simulation.",
            bitmap_failed,
            bitmap_calls.len(),
            MAX_BATCH_FAILURE_FRACTION * 100.0
        );
        return out;
    }

    // Decode bitmaps -> initialized tick indices per pool; count decoded words so a
    // pool missing any word is failed closed (incomplete coverage).
    let mut words_ok: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let mut ticks_found: std::collections::HashMap<usize, Vec<i32>> = std::collections::HashMap::new();
    for (key, bytes) in &bitmap_results {
        let pi = key / NWORDS;
        let wi = key % NWORDS;
        let Some(plan) = plans.get(&pi) else { continue };
        let Some(bitmap) = decode_uint256(bytes) else { continue };
        *words_ok.entry(pi).or_insert(0) += 1;
        if bitmap.is_zero() {
            continue;
        }
        let word = plan.lo_word + wi as i32;
        for b in 0..256usize {
            if bitmap.bit(b) {
                let compressed = word * 256 + b as i32;
                ticks_found.entry(pi).or_default().push(compressed * plan.spacing);
            }
        }
    }

    // Stage B: fetch liquidityNet for each initialized tick of pools that have both
    // COMPLETE bitmap coverage and a tick count within the per-pool cap.
    let mut tick_calls: Vec<Call> = Vec::new();
    let mut tick_keys: Vec<(usize, i32)> = Vec::new(); // key index -> (pi, tick)
    let mut eligible: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for &pi in plans.keys() {
        if words_ok.get(&pi).copied().unwrap_or(0) != NWORDS {
            continue; // incomplete coverage => fail closed for this pool
        }
        let ticks = ticks_found.get(&pi).cloned().unwrap_or_default();
        if ticks.len() > V3_MAX_TICKS_PER_POOL {
            continue; // too many ticks => fail closed (bounded frontier)
        }
        eligible.insert(pi);
        let pool_addr = refs[pi].as_ref().unwrap().pool_addr;
        for t in ticks {
            tick_keys.push((pi, t));
            tick_calls.push(Call {
                key: tick_keys.len() - 1,
                to: pool_addr,
                data: calldata_ticks(t),
            });
        }
    }

    let mut net_by: std::collections::HashMap<(usize, i32), i128> = std::collections::HashMap::new();
    if !tick_calls.is_empty() {
        let (tick_results, tick_failed) = batch_in_chunks(client, &tick_calls, 60).await;
        if tick_failed as f64 / tick_calls.len() as f64 > MAX_BATCH_FAILURE_FRACTION {
            warn!(
                "Skipping V3 cross-tick data: {}/{} ticks() calls failed (> {:.0}%); \
                 falling back to within-interval simulation.",
                tick_failed,
                tick_calls.len(),
                MAX_BATCH_FAILURE_FRACTION * 100.0
            );
            return out;
        }
        for (key, bytes) in &tick_results {
            if let Some((pi, t)) = tick_keys.get(*key).copied() {
                if let Some(net) = decode_liquidity_net(bytes) {
                    net_by.insert((pi, t), net);
                }
            }
        }
    }

    // Assemble per-pool cross-tick data. A pool is included ONLY if every one of its
    // initialized ticks decoded a liquidityNet; otherwise it stays `None`.
    for pi in eligible {
        let plan = &plans[&pi];
        let ticks = ticks_found.get(&pi).cloned().unwrap_or_default();
        let mut v3ticks: Vec<V3Tick> = Vec::with_capacity(ticks.len());
        let mut complete = true;
        for t in &ticks {
            match net_by.get(&(pi, *t)) {
                Some(net) => v3ticks.push(V3Tick { index: *t, liquidity_net: *net }),
                None => {
                    complete = false;
                    break;
                }
            }
        }
        if complete {
            out[pi] = Some(V3CrossTickData { ticks: v3ticks, window: plan.window });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q96() -> f64 {
        2f64.powi(96)
    }

    #[test]
    fn v3_price_equal_decimals_unit() {
        // sqrtPriceX96 == 2^96 encodes raw price 1.0; equal decimals -> 1.0.
        let sqrt = U256::from_big_endian(&{
            let mut b = [0u8; 32];
            U256::from(2u64).pow(U256::from(96)).to_big_endian(&mut b);
            b
        });
        let p = v3_price_token1_per_token0(sqrt, 18, 18);
        assert!((p - 1.0).abs() < 1e-9, "expected 1.0, got {p}");
    }

    #[test]
    fn v3_price_encodes_four() {
        // sqrtPriceX96 = 2 * 2^96 -> raw ratio 2, squared = 4.0 (equal decimals).
        let sqrt = U256::from(2u64) * U256::from(2u64).pow(U256::from(96));
        let p = v3_price_token1_per_token0(sqrt, 18, 18);
        assert!((p - 4.0).abs() < 1e-6, "expected 4.0, got {p}");
    }

    #[test]
    fn v3_price_applies_decimal_factor() {
        // Same raw ratio 1.0, but token0 has 6 decimals and token1 has 18.
        // Human price = 1.0 * 10^(6-18) = 1e-12.
        let sqrt = U256::from(2u64).pow(U256::from(96));
        let p = v3_price_token1_per_token0(sqrt, 6, 18);
        assert!((p - 1e-12).abs() < 1e-24, "expected 1e-12, got {p:e}");
    }

    #[test]
    fn v3_realistic_weth_usdc_ordering() {
        // Construct a sqrtPriceX96 for a raw ratio, then confirm the decimal
        // adjustment yields a human WETH price near 3000 when token0=USDC(6),
        // token1=WETH(18): we want token1_per_token0 (WETH per USDC) ~= 1/3000,
        // so raw = (1/3000) * 10^(18-6) = 1e12/3000.
        let target_human = 1.0f64 / 3000.0; // WETH per USDC
        let raw = target_human * 10f64.powi(18 - 6); // undo the 10^(dec0-dec1) factor
        let sqrt_ratio = raw.sqrt();
        let sqrt_price = sqrt_ratio * q96();
        let sqrt = U256::from((sqrt_price) as u128);
        let p = v3_price_token1_per_token0(sqrt, 6, 18);
        // Within 0.1% of the intended human price.
        assert!((p - target_human).abs() / target_human < 1e-3, "got {p:e}");
    }

    #[test]
    fn v2_price_equal_decimals() {
        // reserve0=1000, reserve1=2000, equal decimals -> price01=2, price10=0.5.
        let p01 = v2_price_token1_per_token0(U256::from(1000u64), U256::from(2000u64), 18, 18);
        assert!((p01 - 2.0).abs() < 1e-9, "got {p01}");
        let p10 = v2_price_token1_per_token0(U256::from(2000u64), U256::from(1000u64), 18, 18);
        assert!((p10 - 0.5).abs() < 1e-9, "got {p10}");
    }

    #[test]
    fn v2_price_applies_decimals() {
        // token0 = USDC (6 dec) reserve 1000 USDC = 1e9 raw.
        // token1 = WETH (18 dec) reserve 1 WETH  = 1e18 raw.
        // price token0->token1 (WETH per USDC) = (1e18/1e9)*10^(6-18) = 1e-3.
        let r0 = U256::from(1_000_000_000u64); // 1e9
        let r1 = U256::exp10(18); // 1e18
        let p01 = v2_price_token1_per_token0(r0, r1, 6, 18);
        assert!((p01 - 1e-3).abs() < 1e-9, "expected 1e-3, got {p01:e}");
        // Inverse: USDC per WETH = 1000.
        let p10 = v2_price_token1_per_token0(r1, r0, 18, 6);
        assert!((p10 - 1000.0).abs() < 1e-6, "expected 1000, got {p10}");
    }

    #[test]
    fn plausibility_guard_rejects_extremes() {
        assert!(is_price_plausible(1.0));
        assert!(is_price_plausible(3000.0));
        assert!(is_price_plausible(1.0 / 3000.0));
        assert!(!is_price_plausible(0.0));
        assert!(!is_price_plausible(-1.0));
        assert!(!is_price_plausible(f64::NAN));
        assert!(!is_price_plausible(f64::INFINITY));
        assert!(!is_price_plausible(1e20));
        assert!(!is_price_plausible(1e-20));
    }

    #[test]
    fn bidirectional_edges_are_reciprocal() {
        let t0 = addr("0x0000000000000000000000000000000000000001");
        let t1 = addr("0x0000000000000000000000000000000000000002");
        let pair = build_bidirectional_edges(
            t0,
            t1,
            2000.0,
            &EdgeMeta {
                liquidity_usd: 0.0,
                dex: "uniswap-v3",
                is_v3: true,
                fee: 500,
                router: Address::zero(),
                dec0: 18,
                dec1: 6,
                reserve0: U256::zero(),
                reserve1: U256::zero(),
                sqrt_price_x96: U256::zero(),
                liquidity: 0,
                tick: 0,
                cross_tick: None,
            },
        )
        .expect("plausible");
        assert_eq!(pair[0].token_in, t0);
        assert_eq!(pair[0].token_out, t1);
        assert!((pair[0].price - 2000.0).abs() < 1e-9);
        assert_eq!(pair[1].token_in, t1);
        assert_eq!(pair[1].token_out, t0);
        assert!((pair[1].price - 1.0 / 2000.0).abs() < 1e-12);
        // Swap state is populated and direction-correct.
        let fwd = pair[0].swap_state.as_ref().unwrap();
        let bwd = pair[1].swap_state.as_ref().unwrap();
        assert!(fwd.zero_for_one);
        assert!(!bwd.zero_for_one);
        assert_eq!(fwd.dec_in, 18);
        assert_eq!(bwd.dec_in, 6);
    }

    #[test]
    fn encode_uint24_layout() {
        let e = encode_uint24(3000);
        assert_eq!(e[29], 0x00);
        assert_eq!(e[30], 0x0b);
        assert_eq!(e[31], 0xb8); // 3000 = 0x0BB8
    }

    #[test]
    fn sorted_pair_orders_by_address() {
        let a = TokenInfo { symbol: "A", address: addr("0x0000000000000000000000000000000000000002"), decimals: 18, blue_chip: false };
        let b = TokenInfo { symbol: "B", address: addr("0x0000000000000000000000000000000000000001"), decimals: 6, blue_chip: false };
        let (t0, t1) = sorted_pair(a, b);
        assert_eq!(t0.symbol, "B");
        assert_eq!(t1.symbol, "A");
    }

    #[test]
    fn to_human_applies_decimals() {
        // 1 WBTC = 1e8 raw at 8 decimals.
        assert!((to_human(U256::from(100_000_000u64), 8) - 1.0).abs() < 1e-9);
        // 1 USDC = 1e6 raw at 6 decimals.
        assert!((to_human(U256::from(1_000_000u64), 6) - 1.0).abs() < 1e-9);
        // 1 DAI = 1e18 raw at 18 decimals.
        assert!((to_human(U256::exp10(18), 18) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn derive_usd_prices_propagates_from_stables() {
        let weth = TokenInfo { symbol: "WETH", address: addr("0x0000000000000000000000000000000000000010"), decimals: 18, blue_chip: true };
        let usdc = TokenInfo { symbol: "USDC", address: addr("0x0000000000000000000000000000000000000020"), decimals: 6, blue_chip: true };
        let arb = TokenInfo { symbol: "ARB", address: addr("0x0000000000000000000000000000000000000030"), decimals: 18, blue_chip: true };
        let tokens = vec![weth, usdc, arb];

        // Address-sorted pools (token1-per-token0 human price):
        //   WETH/USDC: 3000 USDC per WETH  => usd(WETH) = 3000 * 1     = 3000
        //   WETH/ARB : 2000 ARB  per WETH  => usd(ARB)  = 3000 / 2000  = 1.5
        // min_venues = 1 here isolates the propagation math; the consensus/outlier
        // behaviour is covered by its own tests below.
        let pools = vec![
            PoolObs { token0: weth.address, token1: usdc.address, price1_per_0: 3000.0, human0: 10.0, human1: 30_000.0 },
            PoolObs { token0: weth.address, token1: arb.address, price1_per_0: 2000.0, human0: 10.0, human1: 20_000.0 },
        ];
        let usd = derive_usd_prices(&tokens, &pools, 1, 100.0);
        assert!((usd[&usdc.address] - 1.0).abs() < 1e-9);
        assert!((usd[&weth.address] - 3000.0).abs() < 1e-6, "got {}", usd[&weth.address]);
        assert!((usd[&arb.address] - 1.5).abs() < 1e-6, "got {}", usd[&arb.address]);
    }

    #[test]
    fn is_stable_recognizes_registry_stables() {
        assert!(is_stable("USDC"));
        assert!(is_stable("USDCe"));
        assert!(is_stable("USDT"));
        assert!(is_stable("DAI"));
        assert!(!is_stable("WETH"));
        assert!(!is_stable("WBTC"));
    }

    #[test]
    fn only_four_canonical_stables_are_hard_pinned() {
        // Phase 9 honesty invariant: NO new $1 pin. Only the four canonical
        // stablecoins are anchored at $1; every other token — including real
        // long-tail stables that can depeg (FRAX/LUSD/MIM/USDS) — must be priced
        // via the consensus anchor, never hard-pinned.
        assert_eq!(stable_symbols().len(), 4);
        for depeggable in ["FRAX", "LUSD", "MIM", "USDS"] {
            assert!(
                !is_stable(depeggable),
                "{depeggable} must NOT be hard-pinned to $1 (it can depeg; price it via consensus)"
            );
        }
        // And these long-tail stables really are in the registry (so the guard matters).
        let syms: std::collections::HashSet<&str> =
            arbitrum_tokens().iter().map(|t| t.symbol).collect();
        for depeggable in ["FRAX", "LUSD", "MIM", "USDS"] {
            assert!(syms.contains(depeggable), "{depeggable} should be a registered long-tail token");
        }
    }

    #[test]
    fn registry_has_expected_decimals() {
        let t = arbitrum_tokens();
        let d = |sym: &str| t.iter().find(|x| x.symbol == sym).unwrap().decimals;
        assert_eq!(d("USDC"), 6);
        assert_eq!(d("USDCe"), 6);
        assert_eq!(d("USDT"), 6);
        assert_eq!(d("WBTC"), 8);
        assert_eq!(d("WETH"), 18);
        assert_eq!(d("DAI"), 18);
        assert_eq!(d("ARB"), 18);
    }

    #[test]
    fn liquidity_gate_excludes_unvaluable_nan() {
        // FAIL CLOSED: a pool we cannot value (no USD-anchored leg -> NaN) must
        // be rejected, never passed. This is the degraded-RPC mirage guard.
        let liq = value_pool_liquidity_usd(None, None, 1_000.0, 1_000.0);
        assert!(liq.is_nan(), "no USD-anchored leg must yield NaN");
        assert!(
            !passes_liquidity_gate(liq, DEFAULT_MIN_POOL_LIQUIDITY_USD),
            "unvaluable/NaN liquidity must be excluded (fail-closed)"
        );
        // Explicit NaN also fails the gate directly.
        assert!(!passes_liquidity_gate(f64::NAN, DEFAULT_MIN_POOL_LIQUIDITY_USD));
    }

    #[test]
    fn liquidity_gate_excludes_below_threshold() {
        // A genuinely valued but sub-threshold (dust) pool is excluded.
        let liq = value_pool_liquidity_usd(Some(1.0), Some(1.0), 10.0, 10.0);
        assert!((liq - 20.0).abs() < 1e-9);
        assert!(
            !passes_liquidity_gate(liq, DEFAULT_MIN_POOL_LIQUIDITY_USD),
            "finite liquidity below the floor must be excluded"
        );
    }

    #[test]
    fn liquidity_gate_admits_at_or_above_threshold() {
        // At/above the floor a valuable pool passes; single-leg valuation doubles.
        let both = value_pool_liquidity_usd(Some(1.0), Some(2000.0), 5_000.0, 5.0);
        assert!((both - 15_000.0).abs() < 1e-9);
        assert!(passes_liquidity_gate(both, DEFAULT_MIN_POOL_LIQUIDITY_USD));

        let one_leg = value_pool_liquidity_usd(Some(1.0), None, 4_000.0, 1.5);
        assert!((one_leg - 8_000.0).abs() < 1e-9, "single priceable leg doubles");
        assert!(passes_liquidity_gate(one_leg, DEFAULT_MIN_POOL_LIQUIDITY_USD));

        // Exactly at the floor passes (>=).
        assert!(passes_liquidity_gate(
            DEFAULT_MIN_POOL_LIQUIDITY_USD,
            DEFAULT_MIN_POOL_LIQUIDITY_USD
        ));
    }

    #[test]
    fn v3_cross_tick_selectors_match_signatures() {
        // The Phase-8 selectors must be the real first-4-bytes of keccak256 of the
        // Uniswap V3 pool signatures, or every cross-tick fetch would read garbage.
        let bitmap = ethers::utils::keccak256(b"tickBitmap(int16)");
        assert_eq!(SEL_TICK_BITMAP, bitmap[..4], "tickBitmap selector mismatch");
        let ticks = ethers::utils::keccak256(b"ticks(int24)");
        assert_eq!(SEL_TICKS, ticks[..4], "ticks selector mismatch");
    }

    #[test]
    fn encode_int256_sign_extends() {
        // Positive int16 word position: zero-padded.
        let p = encode_int256(5);
        assert!(p[..31].iter().all(|&b| b == 0));
        assert_eq!(p[31], 5);
        // Negative int16 word position: sign-extended with 0xFF (two's complement).
        let n = encode_int256(-1);
        assert!(n.iter().all(|&b| b == 0xFF));
        // -256 as a 32-byte two's-complement word.
        let m = encode_int256(-256);
        assert!(m[..30].iter().all(|&b| b == 0xFF));
        assert_eq!(m[30], 0xFF);
        assert_eq!(m[31], 0x00);
    }

    #[test]
    fn decode_liquidity_net_handles_sign() {
        // Build a ticks() return: 8 words; liquidityNet is word index 1 (an int128
        // sign-extended to 256 bits). We only need the first two words present.
        let mk = |net: i128| -> Vec<u8> {
            let mut buf = vec![0u8; 64];
            // word 0 (liquidityGross) left zero; word 1 = int128 net sign-extended.
            let neg = net < 0;
            if neg {
                for b in buf[32..48].iter_mut() {
                    *b = 0xFF;
                }
            }
            buf[48..64].copy_from_slice(&net.to_be_bytes());
            buf
        };
        assert_eq!(decode_liquidity_net(&mk(0)), Some(0));
        assert_eq!(decode_liquidity_net(&mk(123_456_789)), Some(123_456_789));
        assert_eq!(decode_liquidity_net(&mk(-987_654_321)), Some(-987_654_321));
        assert_eq!(
            decode_liquidity_net(&mk(i128::MIN)),
            Some(i128::MIN),
            "extreme negative round-trips"
        );
        assert_eq!(decode_liquidity_net(&mk(i128::MAX)), Some(i128::MAX));
        // Too-short return => fail closed.
        assert_eq!(decode_liquidity_net(&[0u8; 40]), None);
        // Inconsistent sign extension (high word not all-fill) => fail closed.
        let mut bad = mk(-1);
        bad[40] = 0x00; // corrupt the sign-extension region
        assert_eq!(decode_liquidity_net(&bad), None);
    }

    #[test]
    fn cross_tick_plan_window_covers_current_tick() {
        // The fetched window must always contain the current tick, for every spacing,
        // otherwise the sim's window guard would fail-close a swap that hasn't moved.
        for &(tick, spacing) in &[
            (0i32, 60i32),
            (12_345, 60),
            (-12_345, 60),
            (100_000, 200),
            (-100_000, 200),
            (7, 1),
            (-7, 10),
        ] {
            let plan = cross_tick_plan(tick, spacing);
            let (lo, hi) = plan.window;
            assert!(lo <= tick && tick <= hi, "window {lo}..{hi} excludes tick {tick} (spacing {spacing})");
            assert_eq!(plan.spacing, spacing);
            // Window is clamped to the valid tick range.
            assert!(lo >= V3_MIN_TICK && hi <= V3_MAX_TICK);
        }
        // Near the min tick the window clamps rather than underflowing.
        let plan = cross_tick_plan(V3_MIN_TICK + 1, 200);
        assert_eq!(plan.window.0, V3_MIN_TICK);
    }

    // ================= Phase 9: long-tail universe expansion =================

    // ---- (b) per-pool liquidity floor selection ----
    #[test]
    fn longtail_floor_applies_when_any_leg_is_longtail() {
        let bc0 = TokenInfo { symbol: "WETH", address: addr("0x0000000000000000000000000000000000000a01"), decimals: 18, blue_chip: true };
        let bc1 = TokenInfo { symbol: "USDC", address: addr("0x0000000000000000000000000000000000000a02"), decimals: 6, blue_chip: true };
        let lt = TokenInfo { symbol: "LONG", address: addr("0x0000000000000000000000000000000000000a03"), decimals: 18, blue_chip: false };
        let registry = vec![bc0, bc1, lt];
        // Blue-chip-only pool -> blue-chip floor.
        assert_eq!(
            min_liquidity_for_pool(bc0.address, bc1.address, &registry, 5_000.0, 25_000.0),
            5_000.0
        );
        // Any long-tail leg -> the higher long-tail floor.
        assert_eq!(
            min_liquidity_for_pool(bc0.address, lt.address, &registry, 5_000.0, 25_000.0),
            25_000.0
        );
        // A token absent from the registry is treated as non-blue-chip (long-tail).
        let unknown = addr("0x0000000000000000000000000000000000000a99");
        assert_eq!(
            min_liquidity_for_pool(bc0.address, unknown, &registry, 5_000.0, 25_000.0),
            25_000.0
        );
    }

    #[test]
    fn longtail_pool_below_its_higher_floor_is_dropped() {
        // A $10k pool clears the $5k blue-chip floor but NOT the $25k long-tail floor.
        let liq = 10_000.0;
        assert!(passes_liquidity_gate(liq, DEFAULT_MIN_POOL_LIQUIDITY_USD));
        assert!(!passes_liquidity_gate(liq, DEFAULT_MIN_LONGTAIL_POOL_LIQUIDITY_USD));
    }

    // ---- (c) consensus + depth USD anchoring ----
    fn q(price: f64, depth_usd: f64) -> UsdQuote {
        UsdQuote { price, depth_usd }
    }

    #[test]
    fn anchor_requires_min_venues() {
        // A single un-corroborated quote is NOT trusted (fail-closed).
        assert_eq!(anchor_token_usd(&[q(10.0, 1_000.0)], 2, 3.0), None);
        // Two agreeing quotes anchor.
        assert_eq!(anchor_token_usd(&[q(10.0, 1_000.0), q(10.0, 500.0)], 2, 3.0), Some(10.0));
    }

    #[test]
    fn anchor_rejects_outlier_then_picks_deepest() {
        // Median = 10.1; the 50.0 quote is a >3% outlier and is dropped BEFORE depth
        // selection, even though it is the deepest — proving an outlier cannot win.
        let anchor = anchor_token_usd(&[q(10.0, 100.0), q(10.1, 90.0), q(50.0, 1_000.0)], 2, 3.0);
        assert_eq!(anchor, Some(10.0), "deepest in-consensus quote wins, outlier rejected");
    }

    #[test]
    fn derive_usd_prices_drops_uncorroborated_token() {
        let usdc = TokenInfo { symbol: "USDC", address: addr("0x0000000000000000000000000000000000000b01"), decimals: 6, blue_chip: true };
        let x = TokenInfo { symbol: "X", address: addr("0x0000000000000000000000000000000000000b02"), decimals: 18, blue_chip: false };
        let y = TokenInfo { symbol: "Y", address: addr("0x0000000000000000000000000000000000000b03"), decimals: 18, blue_chip: false };
        let tokens = vec![usdc, x, y];
        // token0 = USDC (its address sorts first). price1_per_0 = tokenX-per-USDC, so
        // usd(X) = usd(USDC)/price1_per_0. X: two deep/correct pools at $10 plus one
        // thin outlier at $20 (rejected). Y: only ONE pool -> below min_venues.
        let pools = vec![
            PoolObs { token0: usdc.address, token1: x.address, price1_per_0: 0.1, human0: 100_000.0, human1: 10_000.0 },
            PoolObs { token0: usdc.address, token1: x.address, price1_per_0: 0.1, human0: 50_000.0, human1: 5_000.0 },
            PoolObs { token0: usdc.address, token1: x.address, price1_per_0: 0.05, human0: 1_000.0, human1: 50.0 },
            PoolObs { token0: usdc.address, token1: y.address, price1_per_0: 0.2, human0: 40_000.0, human1: 8_000.0 },
        ];
        let usd = derive_usd_prices(&tokens, &pools, 2, 3.0);
        assert!((usd[&usdc.address] - 1.0).abs() < 1e-12);
        assert!(
            (usd[&x.address] - 10.0).abs() < 1e-9,
            "X anchored to consensus $10, got {:?}",
            usd.get(&x.address)
        );
        assert!(
            !usd.contains_key(&y.address),
            "Y had < 2 corroborating venues and must stay un-priced (fail-closed drop)"
        );
    }

    // ---- (a) on-chain decimals ----
    fn word_u64(v: u64) -> Vec<u8> {
        let mut w = vec![0u8; 32];
        w[24..32].copy_from_slice(&v.to_be_bytes());
        w
    }

    #[test]
    fn decode_decimals_accepts_plausible_rejects_garbage() {
        assert_eq!(decode_decimals(&word_u64(18)), Some(18));
        assert_eq!(decode_decimals(&word_u64(6)), Some(6));
        assert_eq!(decode_decimals(&word_u64(0)), Some(0));
        assert_eq!(decode_decimals(&word_u64(36)), Some(36)); // boundary kept
        assert_eq!(decode_decimals(&word_u64(37)), None, "above MAX_PLAUSIBLE_DECIMALS -> dropped");
        assert_eq!(decode_decimals(&[]), None, "empty return -> fail closed");
        assert_eq!(decode_decimals(&[0u8; 10]), None, "short return -> fail closed");
        // A huge value in the high bytes is rejected too.
        let mut huge = vec![0u8; 32];
        huge[0] = 0x01;
        assert_eq!(decode_decimals(&huge), None);
    }

    #[test]
    fn resolve_effective_tokens_drops_unreadable_longtail_keeps_bluechip() {
        let bc = TokenInfo { symbol: "USDC", address: addr("0x0000000000000000000000000000000000000c01"), decimals: 6, blue_chip: true };
        let lt = TokenInfo { symbol: "LT", address: addr("0x0000000000000000000000000000000000000c02"), decimals: 18, blue_chip: false };
        let lt2 = TokenInfo { symbol: "LT2", address: addr("0x0000000000000000000000000000000000000c03"), decimals: 18, blue_chip: false };
        let registry = vec![bc, lt, lt2];
        let mut onchain = std::collections::HashMap::new();
        onchain.insert(bc.address, 8u8); // deliberate mismatch: blue-chip keeps verified 6
        onchain.insert(lt.address, 6u8); // long-tail: on-chain overrides the 18 hint
        // lt2 absent -> unreadable -> dropped (fail-closed).
        let (effective, dropped) = resolve_effective_tokens(&registry, &onchain);
        assert_eq!(effective.len(), 2);
        let dec = |a: Address| effective.iter().find(|t| t.address == a).map(|t| t.decimals);
        assert_eq!(dec(bc.address), Some(6), "blue-chip keeps verified constant despite on-chain mismatch");
        assert_eq!(dec(lt.address), Some(6), "long-tail uses authoritative on-chain decimals");
        assert_eq!(dropped, vec![lt2.address]);
    }

    #[test]
    fn resolve_effective_tokens_bluechip_survives_unreadable() {
        // A throttled decimals batch (no entries) must NOT drop a blue-chip.
        let bc = TokenInfo { symbol: "WETH", address: addr("0x0000000000000000000000000000000000000c10"), decimals: 18, blue_chip: true };
        let registry = vec![bc];
        let (effective, dropped) =
            resolve_effective_tokens(&registry, &std::collections::HashMap::new());
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].decimals, 18);
        assert!(dropped.is_empty());
    }

    // ---- (d) non-standard token guards ----
    #[test]
    fn v2_balance_below_reserve_flags_nonstandard() {
        let reserve = U256::from(1_000_000u64);
        // Within tolerance (0.5% below) -> consistent (standard token).
        assert!(v2_balance_consistent(reserve, U256::from(995_000u64), 1.0));
        // Exactly at reserve -> consistent.
        assert!(v2_balance_consistent(reserve, reserve, 1.0));
        // Donation ABOVE reserve -> consistent (balance can only exceed reserve).
        assert!(v2_balance_consistent(reserve, U256::from(2_000_000u64), 1.0));
        // Materially BELOW reserve (2% short) -> NON-standard (flagged, dropped).
        assert!(!v2_balance_consistent(reserve, U256::from(980_000u64), 1.0));
        // Empty reserve is handled by the liquidity gate; treated consistent here.
        assert!(v2_balance_consistent(U256::zero(), U256::zero(), 1.0));
    }

    #[test]
    fn denylist_membership_is_case_insensitive() {
        let a = addr("0x00000000000000000000000000000000000000aa");
        let denylist = ["0x00000000000000000000000000000000000000AA"];
        assert!(is_denylisted(a, &denylist));
        let b = addr("0x00000000000000000000000000000000000000bb");
        assert!(!is_denylisted(b, &denylist));
        // The production denylist is intentionally empty (curated registry is clean).
        assert!(fee_on_transfer_denylist().is_empty());
    }

    // ---- (f) blue-chip preservation ----
    #[test]
    fn all_eleven_bluechips_preserved() {
        let t = arbitrum_tokens();
        let blue: Vec<_> = t.iter().filter(|x| x.blue_chip).collect();
        assert_eq!(blue.len(), 11, "the 11 curated blue-chips must be preserved");
        for sym in ["WETH", "USDC", "USDCe", "USDT", "ARB", "WBTC", "GMX", "PENDLE", "MAGIC", "RDNT", "DAI"] {
            let tok = t
                .iter()
                .find(|x| x.symbol == sym)
                .unwrap_or_else(|| panic!("missing blue-chip {sym}"));
            assert!(tok.blue_chip, "{sym} must be flagged blue_chip");
            assert!(is_blue_chip(tok.address, &t), "{sym} must pass is_blue_chip");
        }
        // Canonical addresses locked so a future edit cannot silently swap them.
        assert_eq!(
            t.iter().find(|x| x.symbol == "WETH").unwrap().address,
            addr("0x82aF49447D8a07e3bd95BD0d56f35241523fBab1")
        );
        assert_eq!(
            t.iter().find(|x| x.symbol == "USDC").unwrap().address,
            addr("0xaf88d065e77c8cC2239327C5EDb3A432268e5831")
        );
        // The long-tail set was actually added and is NOT flagged blue-chip.
        let longtail: Vec<_> = t.iter().filter(|x| !x.blue_chip).collect();
        assert!(longtail.len() >= 20, "Phase 9 must add a real long-tail set");
        let link = t.iter().find(|x| x.symbol == "LINK").unwrap();
        assert!(!is_blue_chip(link.address, &t), "LINK is long-tail, not blue-chip");
    }

    #[test]
    fn registry_addresses_are_unique() {
        // Guards against a copy-paste dup that would double-probe / double-count.
        let t = arbitrum_tokens();
        let mut seen = std::collections::HashSet::new();
        for tok in &t {
            assert!(seen.insert(tok.address), "duplicate address for {}", tok.symbol);
        }
    }

    // ---- (b/e) universe + pool caps ----
    #[test]
    fn cap_tokens_keeps_all_bluechips_then_fills_longtail() {
        let bc0 = TokenInfo { symbol: "B0", address: addr("0x0000000000000000000000000000000000000d01"), decimals: 18, blue_chip: true };
        let bc1 = TokenInfo { symbol: "B1", address: addr("0x0000000000000000000000000000000000000d02"), decimals: 18, blue_chip: true };
        let l0 = TokenInfo { symbol: "L0", address: addr("0x0000000000000000000000000000000000000d03"), decimals: 18, blue_chip: false };
        let l1 = TokenInfo { symbol: "L1", address: addr("0x0000000000000000000000000000000000000d04"), decimals: 18, blue_chip: false };
        let l2 = TokenInfo { symbol: "L2", address: addr("0x0000000000000000000000000000000000000d05"), decimals: 18, blue_chip: false };
        let registry = vec![bc0, bc1, l0, l1, l2];
        // cap = 3 -> both blue-chips + the FIRST long-tail (registry order).
        let capped = cap_tokens(&registry, 3);
        assert_eq!(capped.len(), 3);
        assert_eq!(capped.iter().filter(|t| t.blue_chip).count(), 2);
        assert_eq!(capped[2].symbol, "L0");
        // cap larger than the set -> everything.
        assert_eq!(cap_tokens(&registry, 100).len(), 5);
        // cap smaller than the blue-chip count -> blue-chips are STILL all kept.
        let tiny = cap_tokens(&registry, 1);
        assert_eq!(tiny.len(), 2, "must never drop a blue-chip");
        assert!(tiny.iter().all(|t| t.blue_chip));
    }

    #[test]
    fn cap_tokens_on_real_registry_preserves_bluechips() {
        let capped = cap_tokens(&arbitrum_tokens(), 15);
        assert_eq!(capped.len(), 15);
        assert_eq!(capped.iter().filter(|t| t.blue_chip).count(), 11);
    }

    #[test]
    fn cap_by_depth_keeps_deepest_stable_order() {
        let items = vec![("a", 10.0), ("b", 50.0), ("c", 30.0), ("d", 50.0)];
        let kept = cap_by_depth(items, 2);
        // Descending by depth; ties (b,d both 50) keep input order (b before d).
        assert_eq!(kept, vec![("b", 50.0), ("d", 50.0)]);
        // max >= len returns all (still sorted desc).
        let all = cap_by_depth(vec![("x", 1.0), ("y", 2.0)], 9);
        assert_eq!(all, vec![("y", 2.0), ("x", 1.0)]);
    }

    // ---- (e) throttle-vs-gate discovery telemetry ----
    #[test]
    fn telemetry_honest_zero_is_not_throttle_suspected() {
        // Every probe/state call succeeded; a small edge set here is HONEST: the
        // universe shrank only via real gates (decimals/liquidity/etc.), so a
        // reviewer must NOT read it as a throttle collapse.
        let t = DiscoveryTelemetry {
            block: 100,
            registry_tokens: 43,
            tokens_loaded: 43,
            bluechip_loaded: 11,
            longtail_loaded: 32,
            tokens_dropped_no_decimals: 0,
            tokens_dropped_denylist: 0,
            discovery_probes: 3600,
            discovery_probes_failed: 0,
            pools_discovered: 300,
            state_calls: 1000,
            state_calls_failed: 0,
            pools_dropped_incomplete_state: 0,
            pools_dropped_bad_price: 2,
            pools_dropped_nonstandard: 4,
            pools_dropped_liquidity: 250,
            pools_unpriced_leg: 40,
            pools_capped: 0,
            pools_kept: 44,
            edges_loaded: 88,
        };
        assert!(!t.throttle_suspected(), "clean batches must not be flagged as throttle");
        assert_eq!(t.throttle_drops(), 0);
        // gate_drops = no_decimals(0)+denylist(0)+bad_price(2)+nonstandard(4)+liquidity(250).
        assert_eq!(t.gate_drops(), 256);
        assert_eq!(t.discovery_fail_pct(), 0.0);
        assert_eq!(t.state_fail_pct(), 0.0);
    }

    #[test]
    fn telemetry_flags_throttle_on_failed_discovery_probes() {
        // 10% of discovery probes failed (> THROTTLE_WARN_PCT 5%): a shrunken
        // universe here is a THROTTLE artifact and must be flagged as such, even
        // though it stays under the hard MAX_BATCH_FAILURE_FRACTION abort bar.
        let t = DiscoveryTelemetry {
            discovery_probes: 1000,
            discovery_probes_failed: 100,
            state_calls: 500,
            state_calls_failed: 0,
            pools_discovered: 50,
            ..Default::default()
        };
        assert!(t.discovery_fail_pct() > THROTTLE_WARN_PCT);
        assert!(t.throttle_suspected(), "high discovery failure must flag throttle");
        assert_eq!(t.throttle_drops(), 100);
    }

    #[test]
    fn telemetry_flags_throttle_on_incomplete_state_reads() {
        // Batches "passed" but a large share of discovered pools lost a required
        // state read (missing balanceOf/liquidity) — a partial-graph throttle
        // symptom that must be flagged, not silently treated as honest zero.
        let t = DiscoveryTelemetry {
            discovery_probes: 1000,
            discovery_probes_failed: 0,
            state_calls: 400,
            state_calls_failed: 0,
            pools_discovered: 100,
            pools_dropped_incomplete_state: 20, // 20% of discovered pools
            ..Default::default()
        };
        assert_eq!(t.discovery_fail_pct(), 0.0);
        assert_eq!(t.state_fail_pct(), 0.0);
        assert!(t.incomplete_state_pct() > THROTTLE_WARN_PCT);
        assert!(t.throttle_suspected(), "high incomplete-state rate must flag throttle");
        assert_eq!(t.throttle_drops(), 20);
    }

    #[test]
    fn telemetry_percentages_are_zero_safe() {
        // An all-empty snapshot must not divide by zero.
        let t = DiscoveryTelemetry::default();
        assert_eq!(t.discovery_fail_pct(), 0.0);
        assert_eq!(t.state_fail_pct(), 0.0);
        assert_eq!(t.incomplete_state_pct(), 0.0);
        assert!(!t.throttle_suspected());
        assert_eq!(t.throttle_drops(), 0);
        assert_eq!(t.gate_drops(), 0);
    }
}
