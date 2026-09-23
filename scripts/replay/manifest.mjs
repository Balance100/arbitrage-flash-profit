// Fail-closed pairing validator + manifest generator for the replay boundary.
//
// Reads BOTH adapter envelopes (replay/out/ts-envelope.json,
// replay/out/rs-envelope.json), verifies they are a genuine, unmodified pair
// (same envelopeVersion, same runId/inputHash/sourceHash/fixtureVersion, one
// "ts" adapter and one "rust" adapter — never two of the same), and performs a
// shadow comparison of the overlapping economics the two adapters can agree on:
// hop count, profitability sign, AND economic MAGNITUDE (best loan size,
// realized ratio, net profit USD, within documented tolerances). ANY pairing
// or comparison failure is FAIL-CLOSED: the process exits non-zero and NO
// manifest file is written, so a manifest on disk always means the pairing
// AND the magnitude comparison were both verified.
//
// Shadow-comparison handoff: this repo has no existing cross-language
// scanner-output comparison tool (only single-language economics reports such
// as scripts/execution-policy-report.mjs and scripts/profitability-gates.mjs),
// so the comparison performed here IS the shadow comparison for this replay
// boundary. Magnitude comparison is fail-closed on incompatibility: if either
// envelope's schema doesn't expose comparable numeric fields, the pairing
// fails rather than silently passing on synthetic evidence.
//
// Run: node scripts/replay/manifest.mjs

import { readFileSync, writeFileSync, existsSync, rmSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

import { sha256Hex } from './lib/hash.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, '..', '..');
const OUT_DIR = path.join(REPO_ROOT, 'replay', 'out');
export const TS_ENVELOPE_PATH = path.join(OUT_DIR, 'ts-envelope.json');
export const RS_ENVELOPE_PATH = path.join(OUT_DIR, 'rs-envelope.json');
export const MANIFEST_PATH = path.join(OUT_DIR, 'manifest.json');

const ENVELOPE_VERSION = 'scanner-evidence-v1';

// Magnitude-level economic tolerances between the TS (float, replay-only
// estimate) and Rust (U256 integer, source-of-truth) simulators. Both
// implement the IDENTICAL constant-product-with-fee formula
// (`out = in*(1-fee)*reserveOut / (reserveIn + in*(1-fee))`, see
// `scanner-rust/src/sim.rs` "Swap math fidelity" and `ts-adapter.ts`'s
// `simulateV2Hop`) over the SAME fixture, so a genuine divergence beyond
// float/integer rounding means the TS estimate no longer tracks sim.rs.
// - REALIZED_RATIO: a single quotient close to 1.0 with no compounding
//   subtraction, so its rounding error stays near f64 precision; 1e-6
//   relative leaves ~6 orders of magnitude of margin over observed
//   float-vs-U256 drift (bit-identical on this fixture).
// - NET_PROFIT_USD: subtracts two more terms (Aave premium, gas) from the
//   same ratio, so its relative error can compound slightly more for small
//   loan sizes; 1e-4 relative (1 bps) still easily catches a genuine
//   multi-percent magnitude disagreement.
// - ABS_EPSILON_USD: absolute floor so the relative tolerance doesn't
//   collapse to ~0 right at the profitability breakeven boundary.
export const REALIZED_RATIO_REL_TOLERANCE = 1e-6;
export const NET_PROFIT_REL_TOLERANCE = 1e-4;
export const NET_PROFIT_ABS_EPSILON_USD = 0.01;

function isFiniteNumber(v) {
  return typeof v === 'number' && Number.isFinite(v);
}

/** Relative-with-absolute-floor tolerance check, used for economic magnitude
 * comparisons where the two languages compute the same quantity via
 * different numeric representations (f64 vs U256-then-f64). */
function withinTolerance(a, b, relTol, absEpsilon = 0) {
  const diff = Math.abs(a - b);
  if (diff <= absEpsilon) return true;
  const scale = Math.max(Math.abs(a), Math.abs(b), 1e-12);
  return diff / scale <= relTol;
}

const REQUIRED_FIELDS = [
  'envelopeVersion',
  'runId',
  'inputHash',
  'sourceHash',
  'adapter',
  'fixturePath',
  'fixtureVersion',
  'generatedAt',
  'readOnly',
  'disclaimer',
  'result',
];

function loadEnvelope(filePath, checks) {
  if (!existsSync(filePath)) {
    checks.push({ ok: false, check: `file exists: ${path.relative(REPO_ROOT, filePath)}` });
    return null;
  }
  const raw = readFileSync(filePath);
  let envelope;
  try {
    envelope = JSON.parse(raw.toString('utf8'));
  } catch (e) {
    checks.push({ ok: false, check: `file parses as JSON: ${path.relative(REPO_ROOT, filePath)} (${e.message})` });
    return null;
  }
  for (const field of REQUIRED_FIELDS) {
    checks.push({
      ok: Object.prototype.hasOwnProperty.call(envelope, field),
      check: `${path.basename(filePath)}: has field "${field}"`,
    });
  }
  return { envelope, bytesHash: sha256Hex(raw) };
}

/** Fail-closed pairing validation + shadow comparison. Pure function (no I/O
 * side effects) so it can be unit-tested directly against in-memory envelopes.
 */
export function validatePairing(tsEnvelope, rsEnvelope) {
  const checks = [];
  const check = (ok, description) => checks.push({ ok, check: description });

  check(!!tsEnvelope, 'TS envelope loaded');
  check(!!rsEnvelope, 'Rust envelope loaded');
  if (!tsEnvelope || !rsEnvelope) {
    return { pass: false, checks, comparison: null };
  }

  check(tsEnvelope.envelopeVersion === ENVELOPE_VERSION, `TS envelopeVersion === ${ENVELOPE_VERSION}`);
  check(rsEnvelope.envelopeVersion === ENVELOPE_VERSION, `Rust envelopeVersion === ${ENVELOPE_VERSION}`);

  const adapters = [tsEnvelope.adapter, rsEnvelope.adapter].sort();
  check(
    adapters.length === 2 && adapters[0] === 'rust' && adapters[1] === 'ts',
    'exactly one "ts" adapter and one "rust" adapter (no duplicate/missing pairing)',
  );

  check(tsEnvelope.runId === rsEnvelope.runId, `runId matches (${tsEnvelope.runId} === ${rsEnvelope.runId})`);
  check(
    tsEnvelope.inputHash === rsEnvelope.inputHash,
    `inputHash matches (${tsEnvelope.inputHash} === ${rsEnvelope.inputHash})`,
  );
  check(
    tsEnvelope.sourceHash === rsEnvelope.sourceHash,
    `sourceHash matches (${tsEnvelope.sourceHash} === ${rsEnvelope.sourceHash})`,
  );
  check(
    tsEnvelope.fixtureVersion === rsEnvelope.fixtureVersion,
    `fixtureVersion matches (${tsEnvelope.fixtureVersion} === ${rsEnvelope.fixtureVersion})`,
  );
  check(tsEnvelope.readOnly === true, 'TS envelope declares readOnly: true');
  check(rsEnvelope.readOnly === true, 'Rust envelope declares readOnly: true');

  const pass = checks.every((c) => c.ok);

  // Shadow comparison: the two adapters model different domains (a TS
  // "canonical opportunity" vs a Rust execution-payload survivor report), but
  // both walk the SAME 3-hop fixture loop through the SAME swap formula, so
  // hop count, profitability sign, AND economic magnitude (loan size,
  // realized ratio, net profit) must agree within tolerance. This is the
  // cross-adapter check this replay boundary owns.
  const tsHops = tsEnvelope.result?.trace?.hops;
  const rsHops = rsEnvelope.result?.survivors?.[0]?.hops;
  const tsProfitable = (tsEnvelope.result?.trace?.netProfitUsd ?? 0) > 0;
  const rsProfitable = (rsEnvelope.result?.survivors?.[0]?.realizedNetUsd ?? 0) > 0;

  const tsLoanUsd = tsEnvelope.result?.trace?.loanUsd;
  const rsLoanUsd = rsEnvelope.result?.survivors?.[0]?.bestLoanUsd;
  const tsRealizedRatio = tsEnvelope.result?.trace?.realizedRatio;
  const rsRealizedRatio = rsEnvelope.result?.survivors?.[0]?.realizedRatio;
  const tsNetProfitUsd = tsEnvelope.result?.trace?.netProfitUsd;
  const rsNetProfitUsd = rsEnvelope.result?.survivors?.[0]?.realizedNetUsd;

  // Fail-closed on incompatibility: if either envelope doesn't expose all
  // three comparable magnitude fields as finite numbers, the economics are
  // NOT comparable — this must never be silently treated as a pass.
  const economicsComparable =
    isFiniteNumber(tsLoanUsd) &&
    isFiniteNumber(rsLoanUsd) &&
    isFiniteNumber(tsRealizedRatio) &&
    isFiniteNumber(rsRealizedRatio) &&
    isFiniteNumber(tsNetProfitUsd) &&
    isFiniteNumber(rsNetProfitUsd);

  // Both adapters sweep the SAME finite loan-size candidate list from the
  // shared fixture (`pipelineParams.loanSizesUsd`), so the winning loan size
  // must match EXACTLY — a divergence here means the two simulators disagree
  // about which trade size is optimal, which is a real economic divergence,
  // not numeric noise.
  const loanUsdMatches = economicsComparable && tsLoanUsd === rsLoanUsd;
  const realizedRatioMatches =
    economicsComparable && withinTolerance(tsRealizedRatio, rsRealizedRatio, REALIZED_RATIO_REL_TOLERANCE);
  const netProfitMatches =
    economicsComparable &&
    withinTolerance(tsNetProfitUsd, rsNetProfitUsd, NET_PROFIT_REL_TOLERANCE, NET_PROFIT_ABS_EPSILON_USD);
  const economicsMatch = economicsComparable && loanUsdMatches && realizedRatioMatches && netProfitMatches;

  check(economicsComparable, 'both envelopes expose comparable economic magnitude fields (loanUsd, realizedRatio, netProfitUsd)');
  if (economicsComparable) {
    check(loanUsdMatches, `best loan size matches exactly (ts=${tsLoanUsd} rust=${rsLoanUsd})`);
    check(
      realizedRatioMatches,
      `realized ratio within ${REALIZED_RATIO_REL_TOLERANCE} relative tolerance (ts=${tsRealizedRatio} rust=${rsRealizedRatio})`,
    );
    check(
      netProfitMatches,
      `net profit USD within ${NET_PROFIT_REL_TOLERANCE} relative / ${NET_PROFIT_ABS_EPSILON_USD} abs tolerance (ts=${tsNetProfitUsd} rust=${rsNetProfitUsd})`,
    );
  }

  const comparison = {
    tsHops,
    rsHops,
    hopsMatch: tsHops === rsHops,
    tsProfitable,
    rsProfitable,
    profitabilitySignMatches: tsProfitable === rsProfitable,
    economicsComparable,
    tsLoanUsd,
    rsLoanUsd,
    loanUsdMatches,
    tsRealizedRatio,
    rsRealizedRatio,
    realizedRatioMatches,
    realizedRatioRelTolerance: REALIZED_RATIO_REL_TOLERANCE,
    tsNetProfitUsd,
    rsNetProfitUsd,
    netProfitMatches,
    netProfitRelTolerance: NET_PROFIT_REL_TOLERANCE,
    netProfitAbsEpsilonUsd: NET_PROFIT_ABS_EPSILON_USD,
    economicsMatch,
    note:
      'Shadow comparison covers hop count, profitability sign, AND economic magnitude (best loan size exact match; realized ratio and net profit USD within documented relative tolerances). Magnitude comparison is fail-closed: missing/non-numeric fields or an out-of-tolerance value both cause the pairing to fail.',
  };

  const magnitudeOk = comparison.hopsMatch && comparison.profitabilitySignMatches && economicsMatch;
  return { pass: pass && magnitudeOk, checks, comparison };
}

export function generateManifest() {
  return generateManifestForPaths(TS_ENVELOPE_PATH, RS_ENVELOPE_PATH);
}

/** Generalized manifest generator: pairs an arbitrary TS/Rust envelope path
 * pair (used by the default single-fixture CLI via `generateManifest()`, and
 * by the fixture-matrix runner `scripts/replay/matrix.mjs` for each fixture
 * in the route-length/fee-tier matrix). Pure w.r.t. its inputs beyond reading
 * the two files; same fail-closed pairing + shadow-comparison logic either way.
 */
export function generateManifestForPaths(tsEnvelopePath, rsEnvelopePath) {
  const checks = [];
  const ts = loadEnvelope(tsEnvelopePath, checks);
  const rs = loadEnvelope(rsEnvelopePath, checks);

  const validation = validatePairing(ts?.envelope, rs?.envelope);
  const allChecks = [...checks, ...validation.checks];
  const pass = allChecks.every((c) => c.ok) && validation.pass;

  const manifest = {
    manifestVersion: 'replay-manifest-v1',
    generatedAt: new Date().toISOString(),
    pass,
    checks: allChecks,
    comparison: validation.comparison,
    pairing: pass
      ? {
          runId: ts.envelope.runId,
          inputHash: ts.envelope.inputHash,
          sourceHash: ts.envelope.sourceHash,
          fixtureVersion: ts.envelope.fixtureVersion,
        }
      : null,
    artifacts: {
      tsEnvelope: {
        path: path.relative(REPO_ROOT, tsEnvelopePath).replace(/\\/g, '/'),
        sha256: ts?.bytesHash ?? null,
      },
      rsEnvelope: {
        path: path.relative(REPO_ROOT, rsEnvelopePath).replace(/\\/g, '/'),
        sha256: rs?.bytesHash ?? null,
      },
    },
    disclaimer:
      'SYNTHETIC REPLAY MANIFEST — deterministic fixture replay pairing only. NOT live data, NOT production evidence, NOTHING signed or broadcast.',
  };

  return manifest;
}

function main() {
  const manifest = generateManifest();
  if (!manifest.pass) {
    // FAIL-CLOSED: do not write a manifest for a failed/unpaired/tampered
    // replay, and remove any stale PASSING manifest left over from a prior
    // run so a failure can never be masked by an old manifest.json on disk.
    if (existsSync(MANIFEST_PATH)) {
      rmSync(MANIFEST_PATH);
    }
    console.error('[replay:manifest] FAIL-CLOSED: pairing validation failed. No manifest written.');
    for (const c of manifest.checks.filter((x) => !x.ok)) {
      console.error(`  ✗ ${c.check}`);
    }
    if (
      manifest.comparison &&
      (!manifest.comparison.hopsMatch || !manifest.comparison.profitabilitySignMatches || !manifest.comparison.economicsMatch)
    ) {
      console.error(`  ✗ shadow comparison mismatch: ${JSON.stringify(manifest.comparison)}`);
    }
    process.exitCode = 1;
    return;
  }

  writeFileSync(MANIFEST_PATH, `${JSON.stringify(manifest, null, 2)}\n`, 'utf8');
  console.log(`[replay:manifest] PASS — wrote ${path.relative(REPO_ROOT, MANIFEST_PATH)}`);
  console.log(`[replay:manifest] runId=${manifest.pairing.runId}`);
  console.log(`[replay:manifest] inputHash=${manifest.pairing.inputHash}`);
  console.log(`[replay:manifest] sourceHash=${manifest.pairing.sourceHash}`);
  for (const c of manifest.checks) {
    console.log(`  ✓ ${c.check}`);
  }
}

const isMain = process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1]);
if (isMain) {
  main();
}
