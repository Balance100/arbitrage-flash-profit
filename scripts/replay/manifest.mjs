// Fail-closed pairing validator + manifest generator for the replay boundary.
//
// Reads BOTH adapter envelopes (replay/out/ts-envelope.json,
// replay/out/rs-envelope.json), verifies they are a genuine, unmodified pair
// (same envelopeVersion, same runId/inputHash/sourceHash/fixtureVersion, one
// "ts" adapter and one "rust" adapter — never two of the same), and performs a
// shadow comparison of the overlapping economics the two adapters can agree on
// (hop count and profitability sign). ANY pairing failure is FAIL-CLOSED: the
// process exits non-zero and NO manifest file is written, so a manifest on
// disk always means the pairing was verified.
//
// Shadow-comparison handoff: this repo has no existing cross-language
// scanner-output comparison tool (only single-language economics reports such
// as scripts/execution-policy-report.mjs and scripts/profitability-gates.mjs),
// so the comparison performed here IS the shadow comparison for this replay
// boundary. A later phase could extend it to compare realized-ratio magnitude
// once the TS and Rust simulators share identical fixed-point math.
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
  // both walk the SAME 3-hop fixture loop, so hop count and profitability sign
  // must agree. This is the cross-adapter check this replay boundary owns.
  const tsHops = tsEnvelope.result?.trace?.hops;
  const rsHops = rsEnvelope.result?.survivors?.[0]?.hops;
  const tsProfitable = (tsEnvelope.result?.trace?.netProfitUsd ?? 0) > 0;
  const rsProfitable = (rsEnvelope.result?.survivors?.[0]?.realizedNetUsd ?? 0) > 0;

  const comparison = {
    tsHops,
    rsHops,
    hopsMatch: tsHops === rsHops,
    tsProfitable,
    rsProfitable,
    profitabilitySignMatches: tsProfitable === rsProfitable,
    note:
      'No pre-existing cross-language scanner comparison tool was found in this repo; this manifest performs the shadow comparison. A future phase could add magnitude-level comparison once both simulators share fixed-point math.',
  };

  return { pass: pass && comparison.hopsMatch && comparison.profitabilitySignMatches, checks, comparison };
}

export function generateManifest() {
  const checks = [];
  const ts = loadEnvelope(TS_ENVELOPE_PATH, checks);
  const rs = loadEnvelope(RS_ENVELOPE_PATH, checks);

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
        path: path.relative(REPO_ROOT, TS_ENVELOPE_PATH).replace(/\\/g, '/'),
        sha256: ts?.bytesHash ?? null,
      },
      rsEnvelope: {
        path: path.relative(REPO_ROOT, RS_ENVELOPE_PATH).replace(/\\/g, '/'),
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
    if (manifest.comparison && (!manifest.comparison.hopsMatch || !manifest.comparison.profitabilitySignMatches)) {
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
