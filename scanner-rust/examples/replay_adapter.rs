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

use mev_scanner::replay::{default_repo_root, run_rust_adapter};

fn main() {
    let repo_root = default_repo_root();
    let envelope = run_rust_adapter(&repo_root);

    let out_dir = repo_root.join("replay").join("out");
    fs::create_dir_all(&out_dir).expect("create replay/out directory");
    let out_path = out_dir.join("rs-envelope.json");
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
