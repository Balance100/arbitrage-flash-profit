# Deterministic Replay Boundary

A **read-only, deterministic replay workflow** that proves the TypeScript
scanner-evidence contract and the Rust deterministic pipeline agree on the
same synthetic input, without touching any live RPC, credential, or
production data path.

> **SAFETY**: everything under `replay/` and `scripts/replay/` is
> **read-only** and **synthetic**. Nothing here signs, broadcasts, or executes
> a trade, and none of it may be cited as production/live evidence — every
> generated envelope carries an explicit disclaimer to that effect. The shared
> fixture (`replay/fixtures/replay-input-v1.json`) is never mutated by any
> adapter or tool in this workflow.

## What it proves

1. A shared, version-pinned **input fixture** (`replay/fixtures/replay-input-v1.json`)
   describing one closed 3-hop synthetic arbitrage loop
   (`asset -> tokenB -> tokenC -> asset`), mirroring
   `scanner-rust`'s existing `payload::fixtures::synthetic_detectable_edges`
   test fixture.
2. A **TypeScript adapter** (`scripts/replay/ts-adapter.ts`) that simulates the
   loop and builds a real `CanonicalOpportunity` / `CanonicalExecutionPayload`
   using the EXISTING, already-tested shared contract
   (`supabase/functions/_shared/opportunity-contract.ts`), fail-closed
   validated with `validateOpportunityParity` before being emitted.
3. A **Rust adapter/example** (`scanner-rust/examples/replay_adapter.rs`, logic
   in `scanner-rust/src/replay.rs`) that runs the SAME fixture through the
   EXISTING deterministic pipeline (`mev_scanner::pipeline::run_sample` — the
   same Bellman-Ford detector, price-impact sim gate, payload builder, and
   fail-closed validator the Phase 7 integration tests exercise).
4. Both adapters emit a `scanner-evidence-v1` **envelope** whose `runId`,
   `inputHash`, and `sourceHash` are derived identically from the fixture's raw
   bytes (see "Hash derivation" below), so a genuine pair from the same
   fixture always match exactly.
5. A **fail-closed pairing validator + manifest generator**
   (`scripts/replay/manifest.mjs`) that verifies both envelopes are a genuine,
   untampered pair and performs a **shadow comparison** of the overlapping
   economics the two adapters can agree on: hop count, profitability sign, and
   economic **magnitude** (best loan size, realized ratio, net profit USD,
   within documented tolerances — see "Shadow comparison / handoff" below). It
   writes `replay/out/manifest.json` **only on success**; any pairing or
   magnitude-comparison failure exits non-zero and writes nothing.

## Hash derivation (why TS and Rust match exactly)

- `sourceHash` = `sha256(raw fixture file bytes)`, computed identically by
  `scripts/replay/lib/hash.mjs::sha256Hex` and
  `scanner_rust::replay::sha256_hex`.
- `inputHash` = `sourceHash` by construction: neither adapter transforms the
  fixture before consuming it. The two fields are kept distinct in the schema
  for future-proofing (e.g. a later stage that normalizes input separately
  from hashing the raw source).
- `runId` = `"replay-" + sha256(sourceHash as ASCII hex text).slice(0, 16)`.
  Hashing the hex **text** (not a re-serialized JSON value) sidesteps any
  cross-language JSON/float canonicalization mismatch — both languages hash
  the exact same bytes.

Because all three hashes are pure functions of the immutable fixture file,
re-running either adapter against the unmodified fixture always reproduces the
same `runId`/`inputHash`/`sourceHash`, and the manifest's pairing check is a
hard, fail-closed guarantee rather than a best-effort comparison.

## Shadow comparison / handoff

The TypeScript and Rust adapters model **different domains** — a ranked
`CanonicalOpportunity` vs. an execution-payload `SurvivorReport` — so there is
no existing cross-language, apples-to-apples comparison tool in this repo
(`scripts/execution-policy-report.mjs` and `scripts/profitability-gates.mjs`
are single-language, live-data reports). `scripts/replay/manifest.mjs` performs
the shadow comparison this replay boundary owns today, over the SAME 3-hop
fixture loop:

- **Hop count** must match exactly.
- **Profitability sign** must match exactly.
- **Economic magnitude** must match within documented tolerances:
  - **Best loan size** (`trace.loanUsd` vs. `survivors[0].bestLoanUsd`) must
    match **exactly** — both adapters sweep the identical finite loan-size
    candidate list from the fixture (`pipelineParams.loanSizesUsd`), so a
    divergence here means the two simulators disagree about which trade size
    is optimal, not numeric noise.
  - **Realized ratio** (`trace.realizedRatio` vs. `survivors[0].realizedRatio`)
    must match within **1e-6 relative tolerance**.
  - **Net profit USD** (`trace.netProfitUsd` vs. `survivors[0].realizedNetUsd`)
    must match within **1e-4 relative tolerance** (with a `$0.01` absolute
    floor near the profitability breakeven boundary).

Both adapters implement the identical constant-product-with-fee formula
(`out = in*(1-fee)*reserveOut / (reserveIn + in*(1-fee))`; see
`scanner-rust/src/sim.rs`'s "Swap math fidelity" doc and `ts-adapter.ts`'s
`simulateV2Hop`) over the same fixture bytes, so any real divergence beyond
float-vs-U256 rounding indicates the TS estimate no longer tracks `sim.rs`.
On the current fixture the two adapters produce **bit-identical** magnitude
values, so the tolerances above exist purely as headroom for future fixtures
with less "round" numbers — not because drift is currently observed.

**Fail-closed on incompatibility**: if either envelope's schema doesn't expose
all three magnitude fields as finite numbers, the comparison is marked
`economicsComparable: false` and the pairing **fails** — a schema mismatch or
missing field is never silently treated as a pass. `manifest.json` is written
only when hop count, profitability sign, AND economic magnitude all agree; any
mismatch removes a stale passing manifest and exits non-zero.

## Running it

```powershell
npm run replay:ts        # writes replay/out/ts-envelope.json
npm run replay:rs        # writes replay/out/rs-envelope.json (cargo run --example replay_adapter)
npm run replay:manifest  # fail-closed pairing check; writes replay/out/manifest.json on PASS only
npm run replay:all       # runs all three in order
```

Tests:

```powershell
npm run replay:test                                   # Playwright: hash helpers + fail-closed validator + TS adapter determinism
cargo test --manifest-path scanner-rust/Cargo.toml --test replay_tests  # Rust: envelope shape + determinism + genuine survivor
```

## Files

| Path | Role |
| --- | --- |
| `replay/fixtures/replay-input-v1.json` | Shared, read-only, version-pinned input fixture |
| `scripts/replay/lib/hash.mjs` | Shared hash/runId derivation helpers (also mirrored in Rust) |
| `scripts/replay/ts-adapter.ts` | TypeScript adapter (reuses `_shared/opportunity-contract.ts`) |
| `scanner-rust/src/replay.rs` | Rust adapter core logic (reuses `pipeline::run_sample`) |
| `scanner-rust/examples/replay_adapter.rs` | Rust adapter CLI entrypoint |
| `scripts/replay/manifest.mjs` | Fail-closed pairing validator + manifest generator |
| `tests/replay-boundary.spec.ts` | Playwright tests for hashing, validator, and TS adapter determinism |
| `scanner-rust/tests/replay_tests.rs` | Rust integration tests for the adapter |
| `replay/out/` | Generated artifacts (`ts-envelope.json`, `rs-envelope.json`, `manifest.json`) — gitignored |

## Limitations / next phase

- The TS adapter's constant-product simulator is a **replay-only** economic
  estimate (`number`-based) for building a realistic `CanonicalOpportunity`;
  it does not carry the same integer/on-chain-exact fidelity as the Rust
  `sim.rs` engine. The Rust adapter remains the source of truth for
  executable-payload-grade numbers — the magnitude comparison verifies the TS
  estimate stays within tolerance of that source of truth, it does not make
  the TS number authoritative.
- The shadow comparison now checks hop count, profitability sign, AND
  economic magnitude (best loan size, realized ratio, net profit USD), but
  only over the one synthetic 3-hop fixture this workflow ships. A future
  phase could add more fixtures (different hop counts, fee tiers, V3 pools) to
  broaden magnitude-comparison coverage.
- This workflow is entirely synthetic/offline. It does not — and must not —
  replace live scanner readiness checks (`scripts/scanner-readiness.mjs`) or
  production profitability gates (`scripts/profitability-gates.mjs`). Parity is
  demonstrated **only for this deterministic replay fixture**, not for live
  on-chain conditions.
