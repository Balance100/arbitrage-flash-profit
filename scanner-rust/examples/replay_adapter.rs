//! Rust replay adapter CLI — thin wrapper over `mev_scanner::replay`.
//!
//! Reads the SAME shared, read-only fixture the TypeScript adapter reads
//! (`replay/fixtures/replay-input-v1.json`), runs it through the EXISTING
//! deterministic pipeline, and writes a `scanner-evidence-v1` envelope to
//! `replay/out/rs-envelope.json`. Pair with `npm run replay:manifest` to
//! fail-closed validate that this envelope's `runId`/`inputHash`/`sourceHash`
//! match the TypeScript adapter's envelope.
//!
//! SAFETY: read-only, deterministic, no RPC. Nothing is signed or broadcast.
//!
//! Run: cargo run --example replay_adapter

use std::fs;

use mev_scanner::replay::{default_repo_root, run_rust_adapter_with_fixture, DEFAULT_FIXTURE_REL_PATH};

/// Optional positional args (both optional): a fixture path relative to the
/// repo root, and an output envelope filename (written under `replay/out/`).
/// Omitting both preserves the original single-fixture behavior exactly
/// (`DEFAULT_FIXTURE_REL_PATH` / `rs-envelope.json`); the matrix runner
/// (`scripts/replay/matrix.mjs`) passes both explicitly per fixture.
fn main() {
    let mut args = std::env::args().skip(1);
    let fixture_rel_path = args.next().unwrap_or_else(|| DEFAULT_FIXTURE_REL_PATH.to_string());
    let out_name = args.next().unwrap_or_else(|| "rs-envelope.json".to_string());

    let repo_root = default_repo_root();
    let envelope = run_rust_adapter_with_fixture(&repo_root, &fixture_rel_path);

    let out_dir = repo_root.join("replay").join("out");
    fs::create_dir_all(&out_dir).expect("create replay/out directory");
    let out_path = out_dir.join(&out_name);
    let json = serde_json::to_string_pretty(&envelope).expect("serialize envelope");
    fs::write(&out_path, format!("{json}\n")).expect("write rs-envelope.json");

    println!("[replay:rs] wrote {}", out_path.display());
    println!(
        "[replay:rs] runId={} inputHash={} sourceHash={}",
        envelope.run_id, envelope.input_hash, envelope.source_hash
    );
    println!(
        "[replay:rs] survived={} cycles_proposed={} edges_loaded={}",
        envelope.result.survived, envelope.result.cycles_proposed, envelope.result.edges_loaded
    );
}
