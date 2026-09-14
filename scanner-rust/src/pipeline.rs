//! Phase 7: reusable end-to-end multi-hop pipeline orchestration.
//!
//! Phases 1–6 built each stage of the multi-hop arbitrage pipeline in its own
//! module. This module ties them into ONE reusable, synchronous pass so both the
//! capstone example (`examples/multi_hop_pipeline.rs`) and the deterministic
//! integration test (`tests/pipeline_integration_tests.rs`) drive the exact same
//! code path:
//!
//!   Bellman-Ford negative-cycle detection (`scanner`) ->
//!   per-hop price-impact SIMULATION gate (`sim`, loan-size sweep) ->
//!   `build_hops_from_cycle` (`payload`, sim-derived per-hop `amountOutMin`) ->
//!   `validate_execution_payload` (`payload`, fail-closed) ->
//!   `FlashlightEncoder::encode_execute_arbitrage` (`flashlight`, `Hop[]` calldata)
//!   -> STOP and record honest metrics.
//!
//! It REUSES the existing detector / simulator / validator / encoder — it never
//! reimplements swap math or validation. It is DETECTION + SIMULATION + PAYLOAD
//! BUILD ONLY: nothing here signs, broadcasts, approves, submits a Flashbots
//! bundle, sends a transaction, or flips a live flag. There is no reachable path
//! from this module to a transaction. Execution stays disabled.
//!
//! `run_sample` is deliberately pure and RPC-free: it takes an already-fetched
//! pool snapshot (edges + USD price map + optional block number) so it can be
//! exercised deterministically against a synthetic fixture with no network.

use std::collections::HashMap;

use ethers::types::{Address, U256};

use crate::flashlight::FlashlightEncoder;
use crate::payload::{build_hops_from_cycle, slippage_bps_from_env, validate_execution_payload};
use crate::scanner::{bellman_ford_detect_cycles, ArbitrageCycle};
use crate::sim::{
    gas_usd_per_hop_from_env, loan_sizes_from_env, simulate_cycle_hops, start_token_usd,
    HopSimTrace,
};
use crate::types::PoolEdge;

/// Aave V3 flash-loan premium (basis points) charged on the borrowed amount.
pub const AAVE_BPS: f64 = 5.0;

/// Tunable economic parameters for one pipeline pass. All fields have env-backed
/// defaults via [`PipelineParams::from_env`], mirroring the Phase 4/6 examples.
#[derive(Debug, Clone)]
pub struct PipelineParams {
    /// Aave flash-loan premium (bps).
    pub aave_bps: f64,
    /// Candidate loan sizes (USD) swept per cycle; the best realized net wins.
    pub loan_sizes_usd: Vec<f64>,
    /// Per-hop gas estimate (USD); scaled by hop count inside the sim.
    pub gas_usd_per_hop: f64,
    /// Per-hop slippage tolerance (bps) applied when deriving `amountOutMin`.
    pub slippage_bps: u32,
}

impl PipelineParams {
    /// Read parameters from the standard scanner env vars
    /// (`SCANNER_SIM_LOAN_SIZES_USD`, `SCANNER_SIM_GAS_USD_PER_HOP`,
    /// `SCANNER_DRYRUN_SLIPPAGE_BPS`), falling back to the module defaults.
    pub fn from_env() -> Self {
        Self {
            aave_bps: AAVE_BPS,
            loan_sizes_usd: loan_sizes_from_env(),
            gas_usd_per_hop: gas_usd_per_hop_from_env(),
            slippage_bps: slippage_bps_from_env(),
        }
    }
}

/// A single surviving cycle's full DRY-RUN report: its realized economics plus the
/// executable `Hop[]` calldata that the Phase 5 contract would accept. The
/// calldata is only ever recorded/printed here — never broadcast.
#[derive(Debug, Clone)]
pub struct SurvivorReport {
    /// Index of the cycle within the detector's proposed set.
    pub index: usize,
    /// Number of hops (== number of edges) in the closed loop.
    pub hops: usize,
    /// Per-hop DEX/venue names, in order.
    pub dex_path: Vec<String>,
    /// Token path: the input token of each hop followed by the final `tokenOut`
    /// (which closes back to the borrowed asset).
    pub token_path: Vec<Address>,
    /// Borrowed asset (== `token_path[0]` == final `tokenOut`).
    pub asset: Address,
    /// Raw base-unit borrow amount the winning sim used.
    pub amount: U256,
    /// Loan size (USD) that produced the best realized net.
    pub best_loan_usd: f64,
    /// Realized round-trip ratio from the simulation (>1.0 == gross-positive).
    pub realized_ratio: f64,
    /// Realized NET (USD) after DEX fees + Aave premium + gas — from the sim.
    pub realized_net_usd: f64,
    /// Per-hop sim-derived `amountOutMin` guards, in order.
    pub amount_out_mins: Vec<U256>,
    /// 4-byte selector of the encoded calldata (Phase 5: `0xcfaa9316`).
    pub selector: [u8; 4],
    /// Length of the encoded `executeArbitrage` calldata, in bytes.
    pub calldata_len: usize,
}

impl SurvivorReport {
    /// Selector rendered as `0x`-prefixed hex.
    pub fn selector_hex(&self) -> String {
        format!(
            "0x{:02x}{:02x}{:02x}{:02x}",
            self.selector[0], self.selector[1], self.selector[2], self.selector[3]
        )
    }
}

/// Honest, per-stage metrics for one pipeline pass over one pool snapshot.
#[derive(Debug, Clone, Default)]
pub struct SampleReport {
    /// Arbitrum block number the snapshot was read at (LIVE only; `None` for
    /// synthetic fixtures).
    pub block: Option<u64>,
    /// Total directional pool edges loaded.
    pub edges_loaded: usize,
    /// Edges whose start token has a resolvable USD price (loan-sizeable).
    pub edges_priceable: usize,
    /// Negative cycles proposed by Bellman-Ford on zero-impact spot prices.
    pub cycles_proposed: usize,
    /// Cycles that survived the price-impact sim gate with positive realized net.
    pub survived: usize,
    /// Cycles rejected because their simulated net was <= 0 (a real mirage).
    pub rejected_negative: usize,
    /// Cycles rejected as unsimulable (no start price, missing state, V3 tick
    /// boundary crossed, overflow, or payload build/validate failure).
    pub rejected_unsimulable: usize,
    /// Exact fail-closed reason for each unsimulable cycle, keyed by the
    /// simulator/payload diagnostic and counted across proposed cycles.
    pub unsimulable_reasons: HashMap<String, usize>,
    /// Full DRY-RUN report for each survivor.
    pub survivors: Vec<SurvivorReport>,
}

impl SampleReport {
    /// Fraction (%) of loaded edges whose start token is USD-priceable — a proxy
    /// for how much of the graph the sim gate can actually size loans against.
    pub fn coverage_pct(&self) -> f64 {
        if self.edges_loaded == 0 {
            0.0
        } else {
            100.0 * self.edges_priceable as f64 / self.edges_loaded as f64
        }
    }
}

/// Sweep the configured loan sizes for one cycle and return the trace with the
/// best realized net (or `None` for a structurally empty cycle).
fn best_trace(cycle: &ArbitrageCycle, start_price_usd: f64, params: &PipelineParams) -> Option<HopSimTrace> {
    let hops = cycle.edges.len();
    if hops == 0 {
        return None;
    }
    let gas = params.gas_usd_per_hop * hops as f64;
    let mut best: Option<HopSimTrace> = None;
    for &loan in &params.loan_sizes_usd {
        let trace = simulate_cycle_hops(cycle, loan, start_price_usd, params.aave_bps, gas);
        let replace = match &best {
            None => true,
            Some(b) => trace.outcome.net.net_profit_usd > b.outcome.net.net_profit_usd,
        };
        if replace {
            best = Some(trace);
        }
    }
    best
}

/// Build a [`SurvivorReport`] for a cycle that survived the sim gate by running
/// the EXISTING build -> validate -> encode path. Returns `None` (and lets the
/// caller count it as unsimulable) if the payload cannot be built or fails the
/// fail-closed validator, so a malformed cycle can never be reported as a
/// survivor.
fn survivor_report(
    index: usize,
    cycle: &ArbitrageCycle,
    trace: &HopSimTrace,
    params: &PipelineParams,
) -> Option<SurvivorReport> {
    let payload = build_hops_from_cycle(cycle, trace, params.slippage_bps).ok()?;
    // Re-assert at the dry-run boundary (build already validated).
    validate_execution_payload(&payload).ok()?;

    let calldata =
        FlashlightEncoder::encode_execute_arbitrage(payload.asset, payload.amount, &payload.hops);
    let selector = [calldata[0], calldata[1], calldata[2], calldata[3]];

    let dex_path: Vec<String> = cycle.edges.iter().map(|e| e.dex.clone()).collect();
    let mut token_path: Vec<Address> = cycle.edges.iter().map(|e| e.token_in).collect();
    if let Some(last) = cycle.edges.last() {
        token_path.push(last.token_out);
    }

    Some(SurvivorReport {
        index,
        hops: cycle.edges.len(),
        dex_path,
        token_path,
        asset: payload.asset,
        amount: payload.amount,
        best_loan_usd: trace.outcome.loan_usd,
        realized_ratio: trace.outcome.realized_ratio,
        realized_net_usd: trace.outcome.net.net_profit_usd,
        amount_out_mins: payload.hops.iter().map(|h| h.amount_out_min).collect(),
        selector,
        calldata_len: calldata.len(),
    })
}

/// Run the WHOLE multi-hop pipeline over one already-fetched pool snapshot and
/// return honest per-stage metrics. RPC-free and deterministic given its inputs.
///
/// `block` is the Arbitrum block the snapshot was read at for LIVE passes, or
/// `None` for synthetic fixtures. DETECTION + SIMULATION + PAYLOAD BUILD ONLY —
/// nothing is signed or broadcast.
pub fn run_sample(
    edges: &[PoolEdge],
    usd_prices: &HashMap<Address, f64>,
    block: Option<u64>,
    params: &PipelineParams,
) -> SampleReport {
    let cycles = bellman_ford_detect_cycles(edges);
    let edges_priceable = edges
        .iter()
        .filter(|e| start_token_usd(usd_prices, e.token_in).is_some())
        .count();

    let mut report = SampleReport {
        block,
        edges_loaded: edges.len(),
        edges_priceable,
        cycles_proposed: cycles.len(),
        ..Default::default()
    };

    for (i, cycle) in cycles.iter().enumerate() {
        let Some(first) = cycle.edges.first() else {
            report.rejected_unsimulable += 1;
            continue;
        };
        // No USD price for the start token => can't size a loan => unsimulable.
        let Some(price) = start_token_usd(usd_prices, first.token_in) else {
            report.rejected_unsimulable += 1;
            continue;
        };

        let Some(best) = best_trace(cycle, price, params) else {
            report.rejected_unsimulable += 1;
            continue;
        };

        if best.outcome.survives(0.0) {
            match survivor_report(i, cycle, &best, params) {
                Some(sr) => {
                    report.survived += 1;
                    report.survivors.push(sr);
                }
                // Survived the sim but the payload couldn't be built/validated
                // (e.g. hop count out of [2,5]); fail-closed as unsimulable.
                None => {
                    report.rejected_unsimulable += 1;
                    *report
                        .unsimulable_reasons
                        .entry("payload build or validation failed".to_string())
                        .or_insert(0) += 1;
                }
            }
        } else if best.outcome.simulable {
            report.rejected_negative += 1;
        } else {
            report.rejected_unsimulable += 1;
            let reason = best
                .outcome
                .reject_reason
                .clone()
                .unwrap_or_else(|| "unknown simulation rejection".to_string());
            *report.unsimulable_reasons.entry(reason).or_insert(0) += 1;
        }
    }

    report
}
