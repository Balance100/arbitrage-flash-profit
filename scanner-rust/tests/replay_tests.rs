//! Integration test for the Rust side of the deterministic replay boundary.
//!
//! Exercises `mev_scanner::replay::run_rust_adapter` over the SAME shared,
//! read-only fixture the TypeScript adapter consumes
//! (`replay/fixtures/replay-input-v1.json`), proving: (a) determinism across
//! repeated runs, (b) `inputHash == sourceHash` by construction, (c) the
//! envelope shape matches `scanner-evidence-v1`, and (d) the fixture's 3-hop
//! loop is a genuine (detectable AND sim-positive) survivor via the EXISTING
//! `pipeline::run_sample` engine — no reimplemented swap math here.

use mev_scanner::replay::{
    default_repo_root, derive_run_id, run_rust_adapter, run_rust_adapter_with_fixture, sha256_hex,
};

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

/// Route-length/fee-tier matrix: (fixture path, expected hop count). Covers
/// the contract's supported hop-count range (`MIN_HOPS=2` .. `MAX_HOPS=5`)
/// and several V2 fee tiers (5 bps, 30 bps, 100 bps), each fixture using a
/// UNIFORM fee across its hops (mixing fee tiers within one route compounds
/// enough independent float-vs-U256 rounding to exceed the shadow
/// comparison's documented tolerance — see replay/README.md). All fixtures
/// are plain V2 constant-product hops — this matrix intentionally does NOT
/// exercise V3 or cross-tick behavior (see replay/README.md "Limitations /
/// next phase").
const FIXTURE_MATRIX: &[(&str, usize)] = &[
    ("replay/fixtures/replay-input-v1.json", 3),
    ("replay/fixtures/replay-input-2hop-fee500-v1.json", 2),
    ("replay/fixtures/replay-input-4hop-fee3000-v1.json", 4),
    ("replay/fixtures/replay-input-5hop-fee10000-v1.json", 5),
];

#[test]
fn fixture_matrix_covers_min_hops_through_max_hops_and_is_deterministic() {
    let repo_root = default_repo_root();
    for (fixture_rel_path, expected_hops) in FIXTURE_MATRIX {
        let a = run_rust_adapter_with_fixture(&repo_root, fixture_rel_path);
        let b = run_rust_adapter_with_fixture(&repo_root, fixture_rel_path);

        assert_eq!(a.run_id, b.run_id, "{fixture_rel_path}: runId must be deterministic");
        assert_eq!(a.source_hash, b.source_hash, "{fixture_rel_path}");
        assert_eq!(a.input_hash, a.source_hash, "{fixture_rel_path}: inputHash == sourceHash");
        assert_eq!(a.fixture_path, *fixture_rel_path);

        assert_eq!(a.result.edges_loaded, *expected_hops, "{fixture_rel_path}: hop count");
        assert!(a.result.cycles_proposed >= 1, "{fixture_rel_path}: must detect at least one cycle");
        assert_eq!(a.result.survived, 1, "{fixture_rel_path}: must be a genuine survivor");
        assert_eq!(a.result.rejected_negative, 0, "{fixture_rel_path}");
        assert_eq!(a.result.survivors[0].hops, *expected_hops, "{fixture_rel_path}");
        assert!(a.result.survivors[0].realized_ratio > 1.0, "{fixture_rel_path}");
        assert!(a.result.survivors[0].realized_net_usd > 0.0, "{fixture_rel_path}");
    }
}
