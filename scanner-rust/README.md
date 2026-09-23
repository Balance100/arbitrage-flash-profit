# Rust MEV Scanner

Production-oriented Rust arbitrage scanner for the repository's `FlashLoanArbitrage` Solidity contract.

## Features
- Bellman-Ford negative-cycle arbitrage detection
- Fee-aware edge weights (`-ln(price * (1 - fee))`) net of every per-hop DEX swap fee
- Live Arbitrum multi-DEX pool data feed (Uniswap V3 + SushiSwap) — Phase 3
- Canonical opportunity payloads matching the TypeScript contract
- Local `revm` simulation before execution
- Flashbots bundle submission flow
- Prometheus metrics and optional Supabase telemetry

## Quick start
```bash
cp .env.example .env
cargo build
cargo test
cargo run
```

## Live Arbitrum pool feed (Phase 3)
`scanner.fetch_pools()` pulls REAL, current pool state from Arbitrum mainnet:

- **Venues / fee tiers:** Uniswap V3 (500 / 3000 / 10000 ppm) and SushiSwap
  (UniswapV2-style, 3000 ppm), pools discovered via factory `getPool`/`getPair`.
- **Tokens:** an on-chain-verified registry of the 11 curated **blue-chips**
  (WETH, USDC native, USDC.e, USDT, ARB, WBTC, GMX, PENDLE, MAGIC, RDNT, DAI) plus
  a **Phase 9 long-tail set** of cross-verified mid/small-cap Arbitrum tokens (see
  *Phase 9* below). Every long-tail address was taken from ≥2 reputable token
  lists — never guessed.
- **Prices:** Uniswap V3 from `slot0().sqrtPriceX96`
  (`(sqrt/2^96)^2 * 10^(dec0-dec1)`); V2 from `getReserves()`
  (`reserve1/reserve0 * 10^(dec0-dec1)`). Both directions are emitted with the
  correct per-direction price and the pool's fee.
- **Decimals:** read **on-chain** via ERC20 `decimals()` for every token. The 11
  blue-chips keep their verified constants (USDC/USDC.e/USDT = 6, WBTC = 8, others
  = 18) as a resilient fallback + cross-check; a long-tail token whose `decimals()`
  cannot be read is **dropped** (fail-closed — never assumed). A wrong decimal
  factor manufactures fake spreads, so implausible prices are logged and skipped.
- **Liquidity guard:** on-chain `balanceOf` reserves are valued in USD (see the
  consensus USD anchor in *Phase 9*) and pools below their floor are dropped as
  dust — `SCANNER_MIN_POOL_LIQUIDITY_USD` (default $5,000) for blue-chip-only
  pools, and the higher `SCANNER_MIN_LONGTAIL_POOL_LIQUIDITY_USD` (default
  $25,000) for any pool touching a long-tail token.
- **RPC resilience:** batched `eth_call`s, a request timeout, retries with
  backoff, and modest sequential chunking for rate-limited free tiers.

Run a detection-only live dry run (no execution, no broadcast):
```bash
# key is read from the environment; it is never hardcoded or printed
export ALCHEMY_API_KEY=...   # or SCANNER_RPC_URL=https://...
cargo run --example sim_gate_scan
```

## Multi-hop pipeline (Phases 1–7)

The scanner pivoted from simple 2-DEX arbitrage (no fee-aware edge on liquid
pairs) to **multi-hop / triangular** arbitrage via a Bellman-Ford negative-cycle
scanner. The full chain is **detection + simulation + payload build ONLY** —
nothing signs, broadcasts, or flips a live flag; execution stays disabled.

```
live pools @ block N            (pools.rs, Phase 3 + Phase 8 cross-tick data)
  -> fee-aware Bellman-Ford      (scanner.rs, Phases 1–2)
  -> price-impact SIM gate       (sim.rs, Phase 4 + 8: V2 exact; V3 exact FULL
                                  cross-tick walk (bounded, fail-closed);
                                  realized net from the sim)
  -> build Hop[] payload         (payload.rs, Phase 6: per-hop amountOutMin = sim
                                  output - slippage)
  -> DRY-RUN validate            (payload.rs: hop count [2,5], nonzero router/
                                  tokenOut, per-hop min>0, loop closes to asset)
  -> ABI-encode executeArbitrage (flashlight.rs, Phase 5 contract)  -> STOP
```

`pipeline::run_sample` runs steps 2–6 over one snapshot and returns honest
per-stage metrics (edges loaded, USD-priceable coverage, cycles proposed,
survived, rejected_negative, rejected_unsimulable, and each survivor's route +
realized net + encoded calldata).

### V3 cross-tick swap simulation (Phase 8)
The V3 leg of the sim gate walks each swap **tick-by-tick**, exactly like the
real Uniswap V3 pool, instead of quoting only within the current tick-spacing
interval:

- **Exact integer math.** `get_sqrt_ratio_at_tick` (TickMath magic constants),
  `compute_swap_step` (SwapMath: amountIn rounded UP, amountOut rounded DOWN,
  per-step fee), and Q64.96 `getAmount0/1Delta` reproduce the on-chain result.
  A trade that stays inside one interval yields the **identical** output to the
  old within-interval math (asserted by an equivalence test).
- **Real liquidity crossings.** `pools.rs` fetches a bounded window of the pool's
  `tickBitmap(int16)` words around the current tick and each initialized tick's
  `ticks(int24).liquidityNet`. The loop applies `liquidityNet` to in-range `L`
  at every crossing (add moving up / subtract moving down), so multi-interval
  trades quote an exact (or provably conservative) output.
- **Bounded, fail-closed frontier (anti-mirage).** The walk is capped by the
  fetched tick window and `MAX_TICK_CROSSINGS`. Anything genuinely unknowable —
  no liquidity, missing/partial tick data, a degraded RPC batch, an unknown fee
  tier, overflow, or exceeding the crossing cap — returns `None` and the cycle is
  bucketed `rejected_unsimulable`. A pool without complete in-bounds cross-tick
  data simply falls back to the Phase-4 within-interval behaviour (which still
  fail-closes on a crossing). Rounding always biases toward **under**-stating
  output, never toward inventing profit.

The effect on the honest rejection buckets: cycles that Phase 4 rejected as
`rejected_unsimulable` *purely because* a V3 leg would cross a tick are now
simulated and either **survive** or are honestly `rejected_negative`;
`rejected_unsimulable` is reserved for truly unknowable states. Zero surviving
opportunities remains a valid, truthful outcome.

### Long-tail token/pool universe expansion (Phase 9)
The blue-chip-only universe (~110 edges) never surfaces a survivor — those pairs
are the most-arbitraged on Arbitrum, so real price impact erases every spot
spread. Phase 9 **expands the universe toward the long tail** where persistent
inefficiency can live, feeding a bigger but *still rigorously-gated* edge set into
the **unchanged** Bellman-Ford + net-profit + sim-gate + payload pipeline. It adds
**no new venue** (still Uniswap V3 + SushiSwap V2) and does **not** touch
`executor.rs`. Every gate is fail-closed — expansion only *adds* candidates that
pass honest checks; a gate is never loosened to manufacture a survivor, and zero
survivors is still a valid outcome.

- **(a) Honest discovery + on-chain decimals.** Discovery is a **vetted registry
  expansion**: each long-tail address is a real Arbitrum token cross-verified
  across ≥2 reputable lists (never guessed), and every token's `decimals()` is read
  **on-chain**. A long-tail token with unreadable decimals is dropped; blue-chips
  fall back to their verified constant so a throttled batch can never drop a core
  anchor.
- **(b) Liquidity filtering + caps.** Two-tier USD liquidity floor (blue-chip
  $5,000 / long-tail $25,000) plus hard caps `SCANNER_MAX_TOKENS` (default 64) and
  `SCANNER_MAX_POOLS` (default 400, keep the **deepest** by USD) bound the graph
  and RPC. Everything below floor or over cap is dropped.
- **(c) Consensus + depth USD anchoring.** A long-tail token's USD price is the
  **deepest venue among quotes that agree**: gather every implied USD price (one
  per pool routing the token to an already-priced token), require
  `SCANNER_MIN_ANCHOR_VENUES` (default 2) quotes within `SCANNER_ANCHOR_OUTLIER_PCT`
  (default 3%) of their median, then pick the deepest survivor. A token that cannot
  clear this bar is left **un-priced** → its pools fail the USD liquidity gate and
  are dropped (never valued on a guess). Stablecoins stay anchored at $1.
- **(d) Non-standard token safety.** Fee-on-transfer / rebasing tokens break the
  constant-product / V3 output math. A V2 **reserves-vs-`balanceOf` divergence
  check** (tolerance `SCANNER_V2_BALANCE_TOLERANCE_PCT`, default 1%) flags a token
  whose live balance is materially below its stored reserve and **drops it from all
  its pools** (including V3); a documented denylist is the extension point for known
  bad tokens; and the consensus/depth gate itself rejects most scam/thin tokens.
  When in doubt, exclude.
- **(e) Bounded RPC.** The new `decimals()` reads and the enlarged discovery/state
  batches all flow through the existing batched `eth_call` + fail-closed
  `MAX_BATCH_FAILURE_FRACTION` guard; cross-tick data is fetched only for pools that
  survive the gate + cap. Any degraded/partial batch drops the affected candidate,
  never fabricates state. Each fetch returns a `DiscoveryTelemetry` **throttle-vs-gate
  proof** — probes attempted vs failed, pools dropped for *incomplete state* (RPC
  throttle) vs *honest gates* (decimals/denylist/bad-price/non-standard/liquidity),
  and `throttle_suspected()` (flags when a batch failure rate exceeds
  `THROTTLE_WARN_PCT`) — so a small/zero edge set can be told apart from a silent
  throttle collapse. The capstone prints it per block.
- **(f) Preserved behavior.** All 11 blue-chips stay in with unchanged
  addresses/decimals; V3 (incl. Phase 8 cross-tick) + Sushi V2 math is unchanged;
  the detection/sim/payload pipeline is reused as-is on the bigger edge set.

### Run the capstone dry run
The capstone example samples the whole pipeline across several live blocks, then
always runs a clearly-labeled **synthetic fixture** pass (works with no RPC):
```bash
# LIVE + SYNTHETIC. Key is read from env, never printed/committed. Set it INLINE
# in the same command (env vars don't persist across shells on Windows):
#   PowerShell:  $env:ALCHEMY_API_KEY='<key>'; cargo run --example multi_hop_pipeline
#   bash:        ALCHEMY_API_KEY=<key> cargo run --example multi_hop_pipeline
cargo run --example multi_hop_pipeline           # no RPC => LIVE pass auto-skips
```
Sampling and economics are env-tunable:
- `SCANNER_LIVE_SAMPLES` (default 3) — number of live block samples.
- `SCANNER_LIVE_SAMPLE_DELAY_SECS` (default 5) — delay between samples.
- `SCANNER_SIM_LOAN_SIZES_USD`, `SCANNER_SIM_GAS_USD_PER_HOP`,
  `SCANNER_DRYRUN_SLIPPAGE_BPS` — loan sweep, per-hop gas, slippage guard.
- **Phase 9 universe knobs:** `SCANNER_MAX_TOKENS`, `SCANNER_MAX_POOLS`,
  `SCANNER_MIN_LONGTAIL_POOL_LIQUIDITY_USD`, `SCANNER_MIN_ANCHOR_VENUES`,
  `SCANNER_ANCHOR_OUTLIER_PCT`, `SCANNER_V2_BALANCE_TOLERANCE_PCT` (see
  `.env.example`).

**ZERO surviving cycles is a valid, expected, honest outcome** — spot spreads on
liquid Arbitrum pairs are mirages that price impact erases. The pipeline never
weakens a gate to manufacture a survivor. `tests/pipeline_integration_tests.rs`
proves the whole path deterministically over a synthetic fixture (no RPC).

### Deterministic replay boundary
`examples/replay_adapter.rs` (logic in `src/replay.rs`) runs the SAME shared
fixture used by the TypeScript adapter through this pipeline and emits a
`scanner-evidence-v1` envelope paired against the TS side by matching
`runId`/`inputHash`/`sourceHash`. See `../replay/README.md` for the full
read-only, cross-language replay workflow.
```bash
cargo run --example replay_adapter                 # writes ../replay/out/rs-envelope.json
cargo test --test replay_tests                      # determinism + envelope-shape tests
```

## Environment
Set at least one RPC URL and the matching contract address for live execution.
For the Arbitrum pool feed set one of `SCANNER_RPC_URL`, `ARBITRUM_RPC_URL`, or
`ALCHEMY_API_KEY` / `VITE_ALCHEMY_API_KEY` (see `.env.example`).

## Contract integration
`FlashlightEncoder::encode_execute_arbitrage` encodes
`executeArbitrage(address,uint256,(address,address,bool,uint24,uint256)[])`
(the Phase 5 N-hop `Hop[]` signature, selector `0xcfaa9316`) exactly as required
by `FlashLoanArbitrage.sol`.
