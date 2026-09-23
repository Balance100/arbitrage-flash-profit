//! Integration test for the Rust side of the deterministic replay boundary.
//!
//! Exercises `mev_scanner::replay::run_rust_adapter` over the SAME shared,
//! read-only fixture the TypeScript adapter consumes
//! (`replay/fixtures/replay-input-v1.json`), proving: (a) determinism across
//! repeated runs, (b) `inputHash == sourceHash` by construction, (c) the
//! envelope shape matches `scanner-evidence-v1`, and (d) the fixture's 3-hop
//! loop is a genuine (detectable AND sim-positive) survivor via the EXISTING
//! `pipeline::run_sample` engine — no reimplemented swap math here.

use mev_scanner::replay::{default_repo_root, derive_run_id, run_rust_adapter, sha256_hex};

#[test]
fn rust_adapter_envelope_is_well_formed_and_deterministic() {
    let repo_root = default_repo_root();
    let envelope = run_rust_adapter(&repo_root);

    assert_eq!(envelope.envelope_version, "scanner-evidence-v1");
    assert_eq!(envelope.adapter, "rust");
    assert!(envelope.read_only);
    assert_eq!(envelope.fixture_version, "replay-input-v1");
    assert!(!envelope.disclaimer.is_empty());

    // Hashes are 64-char lowercase-hex sha256 digests.
    assert_eq!(envelope.source_hash.len(), 64);
    assert_eq!(envelope.input_hash, envelope.source_hash);
    assert!(envelope.source_hash.chars().all(|c| c.is_ascii_hexdigit()));

    // runId is re-derivable purely from sourceHash (the shared, language-agnostic formula).
    assert_eq!(envelope.run_id, derive_run_id(&envelope.source_hash));

    // Re-running over the identical fixture bytes reproduces every hash exactly.
    let raw = std::fs::read(repo_root.join("replay/fixtures/replay-input-v1.json")).unwrap();
    assert_eq!(sha256_hex(&raw), envelope.source_hash);

    let envelope2 = run_rust_adapter(&repo_root);
    assert_eq!(envelope.run_id, envelope2.run_id);
    assert_eq!(envelope.result.survived, envelope2.result.survived);
    assert_eq!(
        envelope.result.survivors[0].realized_net_usd,
        envelope2.result.survivors[0].realized_net_usd
    );
}

#[test]
fn rust_adapter_surfaces_the_fixtures_genuine_survivor() {
    let repo_root = default_repo_root();
    let envelope = run_rust_adapter(&repo_root);

    assert_eq!(envelope.result.edges_loaded, 3);
    assert!(envelope.result.cycles_proposed >= 1);
    assert_eq!(envelope.result.survived, 1);
    assert_eq!(envelope.result.rejected_negative, 0);

    let survivor = &envelope.result.survivors[0];
    assert_eq!(survivor.hops, 3);
    assert!(survivor.realized_net_usd > 0.0);
    assert!(survivor.realized_ratio > 1.0);
    assert_eq!(survivor.selector, "0xcfaa9316");
    assert_eq!(survivor.amount_out_mins.len(), 3);
    for min in &survivor.amount_out_mins {
        assert_ne!(min, "0");
    }
}
