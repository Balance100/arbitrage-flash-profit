# Architecture

## Components
- `config.rs`: environment-driven runtime configuration (incl. `resolve_arbitrum_rpc_url`)
- `scanner.rs`: pool graph construction and fee-aware Bellman-Ford cycle detection
- `pools.rs`: live Arbitrum multi-DEX pool feed (Phase 3) + Phase 9 long-tail universe (vetted registry, on-chain `decimals()`, consensus+depth USD anchor, two-tier liquidity floor, token/pool caps, non-standard-token guard) + USD price map + block number + Phase 8 bounded V3 cross-tick data (tick bitmap + `liquidityNet`)
- `sim.rs`: Phase 4/8 price-impact SIMULATION gate (V2 exact; V3 exact FULL cross-tick swap walk, bounded + fail-closed)
- `payload.rs`: Phase 6 N-hop executable-payload builder + fail-closed DRY-RUN validator + synthetic fixtures
- `flashlight.rs`: ABI encoding for the Phase 5 `executeArbitrage(address,uint256,Hop[])` contract
- `pipeline.rs`: Phase 7 reusable end-to-end `run_sample` orchestration + honest metrics
- `types.rs`: canonical JSON payloads shared with Supabase/TypeScript
- `simulator.rs`: local `revm` preflight simulation
- `executor.rs`: Flashbots bundle assembly and relay submission (EXECUTION DISABLED)
- `telemetry.rs`: Prometheus counters/histograms and optional Supabase writes

## Multi-hop dry-run flow (Phases 1–7)
The multi-hop / triangular arbitrage pipeline is detection + simulation + payload
build ONLY — it never signs or broadcasts (execution stays disabled):
1. Fetch live Arbitrum pool state + USD prices + block (`pools.rs`).
2. Fee-aware Bellman-Ford proposes negative cycles on zero-impact spot prices (`scanner.rs`).
3. Price-impact SIM gate sweeps loan sizes and keeps only cycles whose REALIZED
   net (from the simulation, never the detector's guess) is positive (`sim.rs`).
   The V3 leg walks the swap tick-by-tick (Phase 8) — see below.
4. For each survivor, build the `Hop[]` payload with sim-derived per-hop
   `amountOutMin` (`payload.rs::build_hops_from_cycle`).
5. Fail-closed DRY-RUN validate the payload (`payload.rs::validate_execution_payload`).
6. ABI-encode the `executeArbitrage` calldata (`flashlight.rs`) and STOP.

`pipeline.rs::run_sample` performs steps 2–6 over one already-fetched snapshot and
returns honest per-stage metrics. The capstone example
`examples/multi_hop_pipeline.rs` samples this across several live blocks plus an
always-on synthetic fixture; `tests/pipeline_integration_tests.rs` exercises it
deterministically with no RPC. See `README.md` for run commands.

## Uniswap V3 cross-tick swap simulation (Phase 8)
The V3 branch of the sim gate reproduces the real pool's swap loop instead of
quoting only within the current tick-spacing interval:

- **State fetch (`pools.rs::fetch_cross_tick_data`).** For each V3 pool it reads a
  bounded window of `tickBitmap(int16)` words centred on the current tick to find
  initialized ticks, then `ticks(int24)` for each tick's `int128 liquidityNet`.
  The result is a per-pool `V3CrossTickData { ticks, window }` carried on
  `PoolSwapState.cross_tick`.
- **Swap walk (`sim.rs::v3_amount_out`).** While input remains and the price is
  inside the fetched window: find the next initialized tick
  (`next_initialized_tick_within_one_word`), `compute_swap_step` toward it, add
  `amountIn+fee` / `amountOut`, and on reaching an initialized tick apply
  `liquidityNet` to in-range `L` (+ moving up for oneForZero, − moving down for
  zeroForOne). Uses exact TickMath/SwapMath integer math (Q64.96, 512-bit
  `mul_div`), rounding amountIn UP and amountOut DOWN.
- **Bounded, fail-closed frontier.** Capped by the fetched window and
  `MAX_TICK_CROSSINGS`. No liquidity, missing/partial tick data, degraded RPC,
  unknown fee tier, overflow, or exceeding the cap ⇒ `None` ⇒ the cycle is
  `rejected_unsimulable`. A pool without complete in-bounds data falls back to the
  Phase-4 within-interval behaviour. Every rounding choice biases toward
  under-stating output (anti-mirage).
- **Bucket effect.** Cycles previously `rejected_unsimulable` *only* because a V3
  leg crossed a tick are now simulated → **survive** or honest `rejected_negative`.
  Within-interval trades produce byte-identical output to the old math (tested).

## Long-tail token/pool universe expansion (Phase 9)
Phase 9 grows the edge set from the 11 blue-chips (~110 edges) toward the long
tail where persistent inefficiency lives, while keeping every anti-mirage gate
fail-closed. It adds **no new venue** (Uniswap V3 + SushiSwap V2 only) and does
**not** touch `executor.rs`; it feeds a bigger, still-honest edge set into the
unchanged Bellman-Ford + net-profit + sim-gate + payload pipeline. All logic lives
in `pools.rs::fetch_arbitrum_pools_with_usd_at_block` and its helpers.

- **Vetted registry + on-chain decimals (`arbitrum_tokens`, `read_onchain_decimals`,
  `resolve_effective_tokens`).** Long-tail addresses are cross-verified across ≥2
  reputable token lists (never guessed). `decimals()` is read on-chain for every
  token; blue-chips fall back to their verified constant (cross-checked, never
  dropped), long-tail tokens with unreadable decimals are dropped fail-closed.
- **Consensus + depth USD anchor (`derive_usd_prices`, `anchor_token_usd`).** Ports
  the proven TS `deriveUsdReferenceAnchor` to Rust: for each token, gather every
  implied USD quote (tagged with its pool's USD depth), require
  `SCANNER_MIN_ANCHOR_VENUES` quotes within `SCANNER_ANCHOR_OUTLIER_PCT` of their
  median, then pick the deepest survivor. Un-corroborated tokens stay un-priced and
  are dropped by the liquidity gate — never valued on a guess. Deterministic:
  quotes are gathered from a per-pass snapshot, so HashMap order cannot change the
  result.
- **Two-tier liquidity floor + caps (`min_liquidity_for_pool`, `cap_tokens`,
  `cap_by_depth`).** Blue-chip floor `SCANNER_MIN_POOL_LIQUIDITY_USD` vs the higher
  long-tail floor `SCANNER_MIN_LONGTAIL_POOL_LIQUIDITY_USD`; `SCANNER_MAX_TOKENS`
  (blue-chips always kept) and `SCANNER_MAX_POOLS` (deepest-by-USD) bound the graph.
  Cross-tick data is fetched only for pools that survive the gate + cap.
- **Non-standard token guard (`v2_balance_consistent`, `is_denylisted`).** A V2
  reserves-vs-`balanceOf` divergence beyond `SCANNER_V2_BALANCE_TOLERANCE_PCT` flags
  a fee-on-transfer/rebasing token and drops it from ALL its pools (incl. V3); a
  documented denylist covers known-bad tokens; the consensus/depth gate rejects the
  rest. When in doubt, exclude.
- **Bounded, fail-closed RPC.** New `decimals()` reads and the enlarged
  discovery/state batches reuse `batch_in_chunks` + the `MAX_BATCH_FAILURE_FRACTION`
  guard; a degraded batch shrinks the long-tail toward the safe blue-chip baseline
  rather than fabricating state. Blue-chips, venue math, and all downstream gates
  are unchanged.
- **Throttle-vs-gate telemetry (`DiscoveryTelemetry`).** `fetch_arbitrum_pools_with_usd_at_block`
  returns a per-fetch, reporting-only telemetry record (it never influences gating):
  probes attempted/failed, state calls attempted/failed, pools dropped for
  *incomplete state* (throttle-sensitive) vs *honest gates*
  (`no_decimals`/`denylist`/`bad_price`/`nonstandard`/`liquidity`), and
  `throttle_suspected()` (any failure rate above `THROTTLE_WARN_PCT`). This lets a
  reviewer distinguish an HONEST-ZERO edge set from a silent RPC throttle-collapse;
  the capstone example prints it per block and an aggregate baseline-vs-expanded line.

## Legacy single-shot flow
1. Load config and logging.
2. Fetch pool quotes.
3. Convert pools into weighted graph edges.
4. Detect negative cycles.
5. Build canonical opportunities.
6. Simulate active opportunities.
7. Submit execution bundles.
8. Emit metrics and telemetry.

