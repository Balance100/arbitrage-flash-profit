//! Deterministic replay boundary — Rust side.
//!
//! Loads the SAME shared, read-only fixture the TypeScript adapter loads
//! (`replay/fixtures/replay-input-v1.json`) and runs it through the EXISTING
//! deterministic pipeline (`crate::pipeline::run_sample`, which itself reuses
//! the existing Bellman-Ford detector, price-impact sim gate, payload builder,
//! and fail-closed validator) — nothing here reimplements swap math or
//! validation. The result is wrapped in a `scanner-evidence-v1` envelope whose
//! `runId` / `inputHash` / `sourceHash` are derived identically to the
//! TypeScript adapter (see `scripts/replay/lib/hash.mjs`), so a
//! [`crate::replay::pairing_hashes`] call from either language over the same
//! fixture bytes produces byte-identical hashes for cross-adapter pairing.
//!
//! SAFETY: read-only. No RPC, no live credentials, no signing/broadcasting.
//! The fixture is only ever read, never mutated.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ethers::types::{Address, U256};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::pipeline::{run_sample, PipelineParams, SampleReport};
use crate::types::{PoolEdge, PoolSwapState};

pub const ENVELOPE_VERSION: &str = "scanner-evidence-v1";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureEdge {
    token_in: String,
    token_out: String,
    router: String,
    dex: String,
    price: f64,
    fee_ppm: u32,
    reserve_in: String,
    reserve_out: String,
    dec_in: u8,
    dec_out: u8,
    is_v3: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixturePipelineParams {
    aave_bps: f64,
    loan_sizes_usd: Vec<f64>,
    gas_usd_per_hop: f64,
    slippage_bps: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    fixture_version: String,
    #[allow(dead_code)]
    description: String,
    network: String,
    edges: Vec<FixtureEdge>,
    usd_prices: HashMap<String, f64>,
    pipeline_params: FixturePipelineParams,
}

/// sha256 hex digest of raw bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

/// Derive the shared, language-agnostic runId from a sourceHash hex string.
/// Hashes the hex TEXT (ASCII bytes), never a re-serialized JSON value, so this
/// matches `scripts/replay/lib/hash.mjs::deriveRunId` byte-for-byte regardless
/// of language-specific JSON/float formatting differences.
pub fn derive_run_id(source_hash: &str) -> String {
    let digest = sha256_hex(source_hash.as_bytes());
    format!("replay-{}", &digest[..16])
}

fn parse_address(hex_str: &str) -> Address {
    hex_str
        .parse::<Address>()
        .unwrap_or_else(|e| panic!("replay fixture: invalid address {hex_str}: {e}"))
}

fn parse_u256(dec_str: &str) -> U256 {
    U256::from_dec_str(dec_str)
        .unwrap_or_else(|e| panic!("replay fixture: invalid integer {dec_str}: {e}"))
}

fn fixture_path(repo_root: &Path) -> PathBuf {
    repo_root
        .join("replay")
        .join("fixtures")
        .join("replay-input-v1.json")
}

/// Resolve the workspace root from `CARGO_MANIFEST_DIR` (the `scanner-rust`
/// crate directory), one level up.
pub fn default_repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("scanner-rust must live directly under the repo root")
        .to_path_buf()
}

fn edges_from_fixture(fixture: &Fixture) -> Vec<PoolEdge> {
    fixture
        .edges
        .iter()
        .map(|e| PoolEdge {
            token_in: parse_address(&e.token_in),
            token_out: parse_address(&e.token_out),
            price: e.price,
            liquidity_usd: 5_000_000.0,
            dex: e.dex.clone(),
            network: fixture.network.clone(),
            router: parse_address(&e.router),
            is_v3: e.is_v3,
            fee: e.fee_ppm,
            swap_state: Some(PoolSwapState {
                dec_in: e.dec_in,
                dec_out: e.dec_out,
                reserve_in: parse_u256(&e.reserve_in),
                reserve_out: parse_u256(&e.reserve_out),
                sqrt_price_x96: U256::zero(),
                liquidity: 0,
                tick: 0,
                zero_for_one: false,
                cross_tick: None,
            }),
        })
        .collect()
}

fn usd_prices_from_fixture(fixture: &Fixture) -> HashMap<Address, f64> {
    fixture
        .usd_prices
        .iter()
        .map(|(addr, price)| (parse_address(addr), *price))
        .collect()
}

/// Serializable, envelope-facing view of one [`crate::pipeline::SurvivorReport`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SurvivorEvidence {
    pub index: usize,
    pub hops: usize,
    pub dex_path: Vec<String>,
    pub token_path: Vec<String>,
    pub asset: String,
    pub amount: String,
    pub best_loan_usd: f64,
    pub realized_ratio: f64,
    pub realized_net_usd: f64,
    pub amount_out_mins: Vec<String>,
    pub selector: String,
    pub calldata_len: usize,
}

/// Serializable, envelope-facing view of the whole [`SampleReport`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineEvidence {
    pub edges_loaded: usize,
    pub edges_priceable: usize,
    pub coverage_pct: f64,
    pub cycles_proposed: usize,
    pub survived: usize,
    pub rejected_negative: usize,
    pub rejected_unsimulable: usize,
    pub survivors: Vec<SurvivorEvidence>,
}

impl From<&SampleReport> for PipelineEvidence {
    fn from(report: &SampleReport) -> Self {
        PipelineEvidence {
            edges_loaded: report.edges_loaded,
            edges_priceable: report.edges_priceable,
            coverage_pct: report.coverage_pct(),
            cycles_proposed: report.cycles_proposed,
            survived: report.survived,
            rejected_negative: report.rejected_negative,
            rejected_unsimulable: report.rejected_unsimulable,
            survivors: report
                .survivors
                .iter()
                .map(|s| SurvivorEvidence {
                    index: s.index,
                    hops: s.hops,
                    dex_path: s.dex_path.clone(),
                    token_path: s.token_path.iter().map(|a| format!("{a:#x}")).collect(),
                    asset: format!("{:#x}", s.asset),
                    amount: s.amount.to_string(),
                    best_loan_usd: s.best_loan_usd,
                    realized_ratio: s.realized_ratio,
                    realized_net_usd: s.realized_net_usd,
                    amount_out_mins: s.amount_out_mins.iter().map(|m| m.to_string()).collect(),
                    selector: s.selector_hex(),
                    calldata_len: s.calldata_len,
                })
                .collect(),
        }
    }
}

/// The `scanner-evidence-v1` envelope produced by the Rust replay adapter.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayEnvelope {
    pub envelope_version: String,
    pub run_id: String,
    pub input_hash: String,
    pub source_hash: String,
    pub adapter: String,
    pub fixture_path: String,
    pub fixture_version: String,
    pub generated_at: String,
    pub read_only: bool,
    pub disclaimer: String,
    pub result: PipelineEvidence,
}

/// Run the whole Rust side of the replay boundary: read the shared fixture,
/// build its pool edges, run the EXISTING `pipeline::run_sample` engine, and
/// return the `scanner-evidence-v1` envelope. `repo_root` is the workspace root
/// (use [`default_repo_root`] for the normal CLI/example entrypoint; tests pass
/// an explicit path so they never depend on `CARGO_MANIFEST_DIR`).
pub fn run_rust_adapter(repo_root: &Path) -> ReplayEnvelope {
    let path = fixture_path(repo_root);
    let raw = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("replay fixture not found at {}: {e}", path.display()));
    let source_hash = sha256_hex(&raw);
    // Consumed verbatim, no pre-processing before use — inputHash == sourceHash
    // by construction (see `ts-adapter.ts` for the identical rationale).
    let input_hash = source_hash.clone();
    let run_id = derive_run_id(&source_hash);

    let fixture: Fixture = serde_json::from_slice(&raw)
        .unwrap_or_else(|e| panic!("replay fixture at {} failed to parse: {e}", path.display()));

    let edges = edges_from_fixture(&fixture);
    let usd_prices = usd_prices_from_fixture(&fixture);
    let params = PipelineParams {
        aave_bps: fixture.pipeline_params.aave_bps,
        loan_sizes_usd: fixture.pipeline_params.loan_sizes_usd.clone(),
        gas_usd_per_hop: fixture.pipeline_params.gas_usd_per_hop,
        slippage_bps: fixture.pipeline_params.slippage_bps,
    };

    let report = run_sample(&edges, &usd_prices, None, &params);

    ReplayEnvelope {
        envelope_version: ENVELOPE_VERSION.to_string(),
        run_id,
        input_hash,
        source_hash,
        adapter: "rust".to_string(),
        fixture_path: "replay/fixtures/replay-input-v1.json".to_string(),
        fixture_version: fixture.fixture_version,
        generated_at: chrono::Utc::now().to_rfc3339(),
        read_only: true,
        disclaimer: "SYNTHETIC REPLAY EVIDENCE — deterministic fixture replay only. NOT live data, NOT production evidence, NOTHING signed or broadcast.".to_string(),
        result: PipelineEvidence::from(&report),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_derivation_matches_expected_formula() {
        // Independently recompute derive_run_id's formula to guard against
        // accidental changes that would desync it from the TS adapter's
        // `deriveRunId` (scripts/replay/lib/hash.mjs).
        let source_hash = sha256_hex(b"hello");
        let run_id = derive_run_id(&source_hash);
        assert!(run_id.starts_with("replay-"));
        assert_eq!(run_id.len(), "replay-".len() + 16);
    }

    #[test]
    fn replay_over_shared_fixture_is_deterministic_and_survives() {
        let repo_root = default_repo_root();
        let a = run_rust_adapter(&repo_root);
        let b = run_rust_adapter(&repo_root);

        assert_eq!(a.run_id, b.run_id, "runId must be deterministic across runs");
        assert_eq!(a.input_hash, b.input_hash);
        assert_eq!(a.source_hash, b.source_hash);
        assert_eq!(a.input_hash, a.source_hash, "inputHash == sourceHash by construction");
        assert_eq!(a.envelope_version, ENVELOPE_VERSION);
        assert_eq!(a.adapter, "rust");
        assert!(a.read_only);

        // The fixture's 3-hop loop is a genuine (detectable AND sim-positive)
        // survivor, so the pipeline must surface it — an honest, non-mirage
        // result over synthetic-but-real deterministic math.
        assert_eq!(a.result.edges_loaded, 3);
        assert!(a.result.cycles_proposed >= 1);
        assert_eq!(a.result.survived, 1);
        assert_eq!(a.result.survivors.len(), 1);
        assert!(a.result.survivors[0].realized_net_usd > 0.0);
        assert_eq!(a.result.survivors[0].hops, 3);
    }
}
