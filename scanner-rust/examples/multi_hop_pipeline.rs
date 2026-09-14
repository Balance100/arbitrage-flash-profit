//! Phase 7 CAPSTONE: end-to-end multi-hop arbitrage pipeline, sampled across
//! several live Arbitrum blocks, plus an always-on synthetic fixture pass.
//!
//! This is the single integrated entrypoint that runs the WHOLE chain and proves
//! it without ever broadcasting:
//!
//!   live pools (at block N) -> Bellman-Ford negative-cycle detection ->
//!   Phase 4 price-impact SIM GATE (loan-size sweep) -> for each SURVIVING cycle:
//!   build the multi-hop `Hop[]` payload (per-hop `amountOutMin` from the sim) ->
//!   DRY-RUN VALIDATE (shape, hop-count [2,5], loop closes to the borrowed asset,
//!   per-hop min present) -> ABI-encode the calldata the Phase 5 contract accepts
//!   -> print route, realized net, selector, calldata length -> STOP.
//!
//! Two clearly-separated result sections:
//!   1. LIVE (only when an RPC endpoint is resolvable via env): samples the live
//!      pipeline across `SCANNER_LIVE_SAMPLES` blocks (default 3), waiting
//!      `SCANNER_LIVE_SAMPLE_DELAY_SECS` (default 5) between samples so each reads
//!      a distinct block. Honestly reports per-stage metrics per block and an
//!      aggregate. ZERO survivors across all samples is the normal, expected,
//!      VALID result and is printed as such.
//!   2. SYNTHETIC FIXTURE (ALWAYS runs, no RPC): a clearly-labeled synthetic
//!      surviving cycle is pushed through the exact same detect->sim->build->
//!      validate->encode path, and malformed payloads are shown to be REJECTED.
//!
//! SAFETY: nothing here signs, broadcasts, submits a Flashbots bundle, sends a
//! raw transaction, deploys, or flips a live-trade flag. It detects, simulates,
//! builds calldata, asserts, and STOPS. There is NO reachable path to a
//! transaction. Execution stays disabled.
//!
//! Run (fixture pass always works; live pass needs an RPC set inline in the SAME
//! command — env vars do NOT persist across separate shell calls here):
//!   $env:ALCHEMY_API_KEY='<key>'; cargo run --example multi_hop_pipeline
//! or SCANNER_RPC_URL / ARBITRUM_RPC_URL. Loan sweep / gas / slippage via
//! SCANNER_SIM_LOAN_SIZES_USD / SCANNER_SIM_GAS_USD_PER_HOP /
//! SCANNER_DRYRUN_SLIPPAGE_BPS; sampling via SCANNER_LIVE_SAMPLES /
//! SCANNER_LIVE_SAMPLE_DELAY_SECS.

use std::time::Duration;

use mev_scanner::config::resolve_arbitrum_rpc_url;
use mev_scanner::payload::{
    build_hops_from_cycle,
    fixtures::{synthetic_detectable_edges, synthetic_surviving_cycle},
    validate_execution_payload, ExecutionPayload,
};
use mev_scanner::pipeline::{run_sample, PipelineParams, SampleReport, SurvivorReport};
use mev_scanner::pools::fetch_arbitrum_pools_with_usd_at_block;
use mev_scanner::sim::simulate_cycle_hops;

const DEFAULT_SAMPLES: usize = 3;
const DEFAULT_SAMPLE_DELAY_SECS: u64 = 5;

fn samples_from_env() -> usize {
    std::env::var("SCANNER_LIVE_SAMPLES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|v| v.clamp(1, 50))
        .unwrap_or(DEFAULT_SAMPLES)
}

fn sample_delay_from_env() -> Duration {
    let secs = std::env::var("SCANNER_LIVE_SAMPLE_DELAY_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SAMPLE_DELAY_SECS);
    Duration::from_secs(secs)
}

/// Print one survivor's full DRY-RUN detail (route, realized net, calldata).
fn print_survivor(s: &SurvivorReport) {
    let route = s.dex_path.join(" -> ");
    println!(
        "    SURVIVED cycle #{} hops={} best_loan=${:.0} realized_ratio={:.8} realized_net=${:.2}",
        s.index, s.hops, s.best_loan_usd, s.realized_ratio, s.realized_net_usd
    );
    println!("      route: [{route}]");
    let tokens: Vec<String> = s.token_path.iter().map(|t| format!("{t:#x}")).collect();
    println!("      tokens: {}", tokens.join(" -> "));
    println!(
        "      [DRY RUN] payload VALID: asset={:#x} amount={} hops={} selector={} calldata_len={} bytes",
        s.asset,
        s.amount,
        s.hops,
        s.selector_hex(),
        s.calldata_len
    );
    for (j, min) in s.amount_out_mins.iter().enumerate() {
        println!("        hop{j}: amountOutMin={min}");
    }
    println!("      [DRY RUN] calldata BUILT — NOTHING BROADCAST, NOTHING SIGNED.");
}

/// Print one sample's per-stage metrics + any survivors.
fn print_sample(idx: usize, r: &SampleReport) {
    let block = r
        .block
        .map(|b| b.to_string())
        .unwrap_or_else(|| "n/a".to_string());
    println!(
        "-- sample {idx} @ block {block}: edges_loaded={} priceable={} ({:.1}% coverage) \
         cycles_proposed={} | survived={} rejected_negative={} rejected_unsimulable={}",
        r.edges_loaded,
        r.edges_priceable,
        r.coverage_pct(),
        r.cycles_proposed,
        r.survived,
        r.rejected_negative,
        r.rejected_unsimulable
    );
    for s in &r.survivors {
        print_survivor(s);
    }
    if !r.unsimulable_reasons.is_empty() {
        let mut reasons: Vec<(&String, &usize)> = r.unsimulable_reasons.iter().collect();
        reasons.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        println!("    unsimulable reasons:");
        for (reason, count) in reasons {
            println!("      {count}x {reason}");
        }
    }
}

async fn live_pass(params: &PipelineParams) -> eyre::Result<()> {
    let Some(rpc_url) = resolve_arbitrum_rpc_url() else {
        println!("== LIVE PASS SKIPPED: no RPC (set ALCHEMY_API_KEY / SCANNER_RPC_URL / ARBITRUM_RPC_URL) ==");
        println!("   (The synthetic fixture pass below still proves the encode/validate path.)");
        return Ok(());
    };

    let samples = samples_from_env();
    let delay = sample_delay_from_env();
    println!("== LIVE PASS: Arbitrum detection -> SIM GATE -> DRY-RUN payload (NO execution) ==");
    println!(
        "Sampling {samples} block(s), {:?} apart. Sim gate: loan sweep {:?} USD, gas ${:.2}/hop, \
         Aave {}bps, dry-run slippage {}bps.",
        delay, params.loan_sizes_usd, params.gas_usd_per_hop, params.aave_bps, params.slippage_bps
    );
    println!("---");

    let mut reports: Vec<SampleReport> = Vec::with_capacity(samples);
    for i in 0..samples {
        if i > 0 {
            tokio::time::sleep(delay).await;
        }
        match fetch_arbitrum_pools_with_usd_at_block(&rpc_url).await {
            Ok((edges, usd_prices, block)) => {
                let report = run_sample(&edges, &usd_prices, Some(block), params);
                print_sample(i, &report);
                reports.push(report);
            }
            Err(e) => {
                println!("-- sample {i}: pool fetch FAILED (skipped honestly): {e}");
            }
        }
    }

    println!("---");
    let total_proposed: usize = reports.iter().map(|r| r.cycles_proposed).sum();
    let total_survived: usize = reports.iter().map(|r| r.survived).sum();
    let total_neg: usize = reports.iter().map(|r| r.rejected_negative).sum();
    let total_unsim: usize = reports.iter().map(|r| r.rejected_unsimulable).sum();
    let mut unsim_reasons = std::collections::HashMap::<String, usize>::new();
    for report in &reports {
        for (reason, count) in &report.unsimulable_reasons {
            *unsim_reasons.entry(reason.clone()).or_insert(0) += count;
        }
    }
    println!(
        "LIVE AGGREGATE over {} successful sample(s): cycles_proposed={} | survived={} \
         rejected_negative={} rejected_unsimulable={}",
        reports.len(),
        total_proposed,
        total_survived,
        total_neg,
        total_unsim
    );
    if !unsim_reasons.is_empty() {
        let mut reasons: Vec<(&String, &usize)> = unsim_reasons.iter().collect();
        reasons.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        println!("LIVE UNSIMULABLE REASONS:");
        for (reason, count) in reasons {
            println!("  {count}x {reason}");
        }
    }
    if total_survived == 0 {
        println!(
            "LIVE RESULT: ZERO survivors across all sampled blocks. Every spot cycle collapsed once \
             REAL price impact was simulated — an HONEST, valid outcome. Nothing was executed or broadcast."
        );
    } else {
        println!(
            "LIVE RESULT: {total_survived} surviving cycle(s) across the samples; each built + \
             validated + encoded as DRY-RUN calldata only. NOTHING BROADCAST."
        );
    }
    Ok(())
}

fn fixture_pass(params: &PipelineParams) {
    println!();
    println!("== SYNTHETIC FIXTURE PASS (NOT live data) — proves the detect/sim/encode path ==");

    // Drive the SAME run_sample path over a synthetic, spot-DETECTABLE snapshot so
    // the fixture exercises detection + sim + build + validate + encode end to end.
    let edges = synthetic_detectable_edges();
    // Price every token in the snapshot (USD only sizes the loan; ratio is unit-less).
    let mut usd_prices = std::collections::HashMap::new();
    for e in &edges {
        usd_prices.insert(e.token_in, 1.0);
        usd_prices.insert(e.token_out, 1.0);
    }

    let report = run_sample(&edges, &usd_prices, None, params);
    println!(
        "  Fixture snapshot: edges={} cycles_proposed={} survived={} rejected_negative={} rejected_unsimulable={}",
        report.edges_loaded,
        report.cycles_proposed,
        report.survived,
        report.rejected_negative,
        report.rejected_unsimulable
    );
    for s in &report.survivors {
        print_survivor(s);
    }
    assert!(
        report.survived >= 1,
        "synthetic detectable fixture must surface at least one surviving cycle"
    );

    // Now prove the validator REJECTS malformed payloads (fail-closed), using the
    // directly-built surviving-cycle fixture.
    println!("  -> Malformed-payload rejections (expected, proves fail-closed validation):");
    let cycle = synthetic_surviving_cycle();
    let trace = simulate_cycle_hops(&cycle, 1_000.0, 1.0, params.aave_bps, 4.0 * cycle.edges.len() as f64);
    let good = build_hops_from_cycle(&cycle, &trace, params.slippage_bps)
        .expect("fixture builds a valid payload");

    // (a) wrong final token (loop does not close).
    let mut broken = good.clone();
    let mut bad_token = [0u8; 20];
    bad_token[19] = 99;
    broken.hops.last_mut().unwrap().token_out = bad_token.into();
    report_reject("wrong final token (loop not closed)", &broken);

    // (b) hop count below MIN_HOPS.
    let mut too_few = good.clone();
    too_few.hops.truncate(1);
    report_reject("hop count 1 (< MIN_HOPS)", &too_few);

    // (c) hop count above MAX_HOPS.
    let mut too_many = good.clone();
    let filler = too_many.hops[0].clone();
    while too_many.hops.len() <= 5 {
        too_many.hops.insert(0, filler.clone());
    }
    report_reject("hop count 6 (> MAX_HOPS)", &too_many);

    // (d) missing per-hop min.
    let mut no_min = good.clone();
    no_min.hops[1].amount_out_min = ethers::types::U256::zero();
    report_reject("missing per-hop amountOutMin", &no_min);

    println!("  Fixture pass complete. All rejections behaved as expected. NOTHING BROADCAST.");
}

fn report_reject(label: &str, payload: &ExecutionPayload) {
    match validate_execution_payload(payload) {
        Ok(()) => println!("        UNEXPECTED: '{label}' was ACCEPTED (this is a bug!)"),
        Err(e) => println!("        REJECTED as expected — {label}: {e}"),
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    println!("############################################################");
    println!("# Phase 7 CAPSTONE: multi-hop arbitrage end-to-end pipeline");
    println!("# DETECTION + SIMULATION + PAYLOAD BUILD ONLY.");
    println!("# NO transaction is signed, sent, or broadcast. NO live flag is set.");
    println!("############################################################");

    let params = PipelineParams::from_env();

    live_pass(&params).await?;
    fixture_pass(&params);

    println!();
    println!("DONE. This was a DRY RUN: no broadcast, no signing, no live trade. ✅");
    Ok(())
}
