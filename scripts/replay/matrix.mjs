// Fixture-matrix runner for the deterministic replay boundary.
//
// Runs BOTH adapters (TS + Rust) and the fail-closed pairing/shadow-comparison
// manifest generator over a small, focused matrix of additional read-only
// fixtures covering the contract's supported route-length range
// (MIN_HOPS=2 .. MAX_HOPS=5) and several V2 fee tiers, IN ADDITION to the
// baseline `replay-input-v1.json` fixture (3 hops, uniform 30 bps fee).
//
// Every fixture in the matrix is plain UniswapV2-style constant-product
// swaps, each with a UNIFORM fee across all its hops (never mixed per-hop) —
// mixing fee tiers within one route compounds enough independent
// float-vs-U256 rounding steps to exceed the shadow comparison's documented
// tolerances (see replay/README.md's "Fixture matrix" section). This matrix
// intentionally does NOT include a V3 or cross-tick fixture — see
// replay/README.md "Limitations / next phase" for why that would require
// either faking cross-tick liquidity data (unsafe) or a tick-bounded
// synthetic reserve set this workflow does not yet model with the same
// "mirrors an existing, already-tested Rust fixture" discipline as the
// baseline.
//
// SAFETY: read-only, synthetic, deterministic. No RPC, no live credentials,
// no signing/broadcasting. Fixtures are never mutated.
//
// Run: node scripts/replay/matrix.mjs

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

import { generateManifestForPaths } from './manifest.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, '..', '..');
const OUT_DIR = path.join(REPO_ROOT, 'replay', 'out');
const TS_ADAPTER = path.join(REPO_ROOT, 'scripts', 'replay', 'ts-adapter.ts');

export const MATRIX_MANIFEST_PATH = path.join(OUT_DIR, 'matrix-manifest.json');

/** The fixture matrix: each entry names a fixture already present under
 * `replay/fixtures/`, its expected hop count (a sanity cross-check, not a
 * source of truth), and a short label used to derive this run's output
 * filenames. Keep this list small and deterministic — one fixture per
 * route-length/fee-tier combination this replay boundary can safely model
 * today (see each fixture's own `description` field for the exact rationale).
 */
export const FIXTURE_MATRIX = [
  { label: 'v1', fixture: 'replay/fixtures/replay-input-v1.json', expectedHops: 3 },
  { label: '2hop-fee500', fixture: 'replay/fixtures/replay-input-2hop-fee500-v1.json', expectedHops: 2 },
  { label: '4hop-fee3000', fixture: 'replay/fixtures/replay-input-4hop-fee3000-v1.json', expectedHops: 4 },
  { label: '5hop-fee10000', fixture: 'replay/fixtures/replay-input-5hop-fee10000-v1.json', expectedHops: 5 },
];

function runOne(entry) {
  const tsOutName = `ts-envelope-${entry.label}.json`;
  const rsOutName = `rs-envelope-${entry.label}.json`;
  const tsOutPath = path.join(OUT_DIR, tsOutName);
  const rsOutPath = path.join(OUT_DIR, rsOutName);

  mkdirSync(OUT_DIR, { recursive: true });

  execFileSync('node', ['--experimental-strip-types', TS_ADAPTER, entry.fixture, tsOutName], {
    cwd: REPO_ROOT,
    stdio: 'pipe',
  });
  execFileSync(
    'cargo',
    ['run', '--quiet', '--manifest-path', 'scanner-rust/Cargo.toml', '--example', 'replay_adapter', '--', entry.fixture, rsOutName],
    { cwd: REPO_ROOT, stdio: 'pipe' },
  );

  const manifest = generateManifestForPaths(tsOutPath, rsOutPath);
  const hopsOk = manifest.comparison?.tsHops === entry.expectedHops && manifest.comparison?.rsHops === entry.expectedHops;

  return { ...entry, manifest, hopsOk, pass: manifest.pass && hopsOk };
}

export function runMatrix() {
  return FIXTURE_MATRIX.map(runOne);
}

function main() {
  let results;
  try {
    results = runMatrix();
  } catch (error) {
    // FAIL-CLOSED: an adapter or manifest process failure is also a failed
    // matrix run. Remove any stale PASSING manifest before returning so a
    // prior successful run can never mask an incomplete evaluation.
    if (existsSync(MATRIX_MANIFEST_PATH)) {
      rmSync(MATRIX_MANIFEST_PATH);
    }
    const message = error instanceof Error ? error.message : String(error);
    console.error(`[replay:matrix] FAIL-CLOSED: matrix execution failed. No matrix manifest written: ${message}`);
    process.exitCode = 1;
    return;
  }
  const overallPass = results.every((r) => r.pass);

  const matrixManifest = {
    matrixManifestVersion: 'replay-matrix-manifest-v1',
    generatedAt: new Date().toISOString(),
    pass: overallPass,
    fixtures: results.map((r) => ({
      label: r.label,
      fixture: r.fixture,
      expectedHops: r.expectedHops,
      pass: r.pass,
      hopsOk: r.hopsOk,
      pairing: r.manifest.pairing,
      comparison: r.manifest.comparison,
    })),
    disclaimer:
      'SYNTHETIC REPLAY MATRIX MANIFEST — deterministic fixture replay pairings only. NOT live data, NOT production evidence, NOTHING signed or broadcast.',
  };

  console.log(`[replay:matrix] ${results.length} fixture(s) evaluated`);
  for (const r of results) {
    console.log(`  ${r.pass ? '✓' : '✗'} ${r.label} (${r.fixture}, ${r.expectedHops} hops)`);
    if (!r.pass) {
      for (const c of r.manifest.checks.filter((x) => !x.ok)) {
        console.log(`      ✗ ${c.check}`);
      }
      if (!r.hopsOk) {
        console.log(
          `      ✗ hop count mismatch: expected ${r.expectedHops}, ts=${r.manifest.comparison?.tsHops} rust=${r.manifest.comparison?.rsHops}`,
        );
      }
    }
  }

  if (!overallPass) {
    // FAIL-CLOSED: mirror the single-fixture manifest's behavior — never
    // leave a stale PASSING matrix manifest on disk after a failing run.
    if (existsSync(MATRIX_MANIFEST_PATH)) {
      rmSync(MATRIX_MANIFEST_PATH);
    }
    console.error('[replay:matrix] FAIL-CLOSED: at least one fixture pairing failed. No matrix manifest written.');
    process.exitCode = 1;
    return;
  }

  writeFileSync(MATRIX_MANIFEST_PATH, `${JSON.stringify(matrixManifest, null, 2)}\n`, 'utf8');
  console.log(`[replay:matrix] PASS — wrote ${path.relative(REPO_ROOT, MATRIX_MANIFEST_PATH)}`);
}

const isMain = process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1]);
if (isMain) {
  main();
}
