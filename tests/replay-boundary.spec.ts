import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { expect, test } from '@playwright/test';

import { sha256Hex, deriveRunId } from '../scripts/replay/lib/hash.mjs';
import { validatePairing } from '../scripts/replay/manifest.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, '..');

function baseEnvelope(adapter) {
  return {
    envelopeVersion: 'scanner-evidence-v1',
    runId: 'replay-aaaaaaaaaaaaaaaa',
    inputHash: 'h'.repeat(64),
    sourceHash: 'h'.repeat(64),
    adapter,
    fixturePath: 'replay/fixtures/replay-input-v1.json',
    fixtureVersion: 'replay-input-v1',
    generatedAt: new Date().toISOString(),
    readOnly: true,
    disclaimer: 'SYNTHETIC REPLAY EVIDENCE',
    result:
      adapter === 'ts'
        ? { trace: { hops: 3, netProfitUsd: 10 } }
        : { survivors: [{ hops: 3, realizedNetUsd: 10 }] },
  };
}

test.describe('replay boundary: hash helpers', () => {
  test('sha256Hex is deterministic and language-agnostic in shape', () => {
    const digest = sha256Hex('hello');
    expect(digest).toMatch(/^[0-9a-f]{64}$/);
    expect(sha256Hex('hello')).toBe(digest);
    expect(sha256Hex('hello!')).not.toBe(digest);
  });

  test('deriveRunId is a pure function of its sourceHash input', () => {
    const hash = sha256Hex('fixture-bytes');
    const runId = deriveRunId(hash);
    expect(runId).toMatch(/^replay-[0-9a-f]{16}$/);
    expect(deriveRunId(hash)).toBe(runId);
    expect(deriveRunId(sha256Hex('other-bytes'))).not.toBe(runId);
  });
});

test.describe('replay boundary: fail-closed pairing validator', () => {
  test('accepts a genuinely paired ts + rust envelope pair', () => {
    const result = validatePairing(baseEnvelope('ts'), baseEnvelope('rust'));
    expect(result.pass).toBe(true);
    expect(result.checks.every((c) => c.ok)).toBe(true);
    expect(result.comparison.hopsMatch).toBe(true);
    expect(result.comparison.profitabilitySignMatches).toBe(true);
  });

  test('fails closed when runId diverges between adapters', () => {
    const rs = baseEnvelope('rust');
    rs.runId = 'replay-bbbbbbbbbbbbbbbb';
    const result = validatePairing(baseEnvelope('ts'), rs);
    expect(result.pass).toBe(false);
    expect(result.checks.some((c) => !c.ok && c.check.includes('runId matches'))).toBe(true);
  });

  test('fails closed when sourceHash diverges (tampered fixture/output)', () => {
    const rs = baseEnvelope('rust');
    rs.sourceHash = 'f'.repeat(64);
    const result = validatePairing(baseEnvelope('ts'), rs);
    expect(result.pass).toBe(false);
    expect(result.checks.some((c) => !c.ok && c.check.includes('sourceHash matches'))).toBe(true);
  });

  test('fails closed when both envelopes claim the same adapter (no real pairing)', () => {
    const result = validatePairing(baseEnvelope('ts'), baseEnvelope('ts'));
    expect(result.pass).toBe(false);
    expect(result.checks.some((c) => !c.ok && c.check.includes('exactly one "ts" adapter'))).toBe(true);
  });

  test('fails closed when a required envelope is missing', () => {
    const result = validatePairing(null, baseEnvelope('rust'));
    expect(result.pass).toBe(false);
  });

  test('shadow comparison flags a hop-count or profitability-sign divergence', () => {
    const rs = baseEnvelope('rust');
    rs.result.survivors[0].hops = 2;
    const result = validatePairing(baseEnvelope('ts'), rs);
    expect(result.comparison.hopsMatch).toBe(false);
    expect(result.pass).toBe(false);
  });
});

test.describe('replay boundary: manifest CLI removes stale PASS manifests on failure', () => {
  test('a prior PASS manifest.json is deleted, not left stale, when a subsequent run fails closed', () => {
    const outDir = path.join(REPO_ROOT, 'replay', 'out');
    const rsPath = path.join(outDir, 'rs-envelope.json');
    const manifestPath = path.join(outDir, 'manifest.json');
    const manifestScript = path.join(REPO_ROOT, 'scripts', 'replay', 'manifest.mjs');

    const originalRs = readFileSync(rsPath, 'utf8');
    try {
      // Establish a known-good PASS manifest first.
      execFileSync('node', [manifestScript], { cwd: REPO_ROOT });
      expect(readFileSync(manifestPath, 'utf8')).toContain('"pass": true');

      // Tamper with one envelope's runId, then re-run: must fail closed AND
      // remove the stale PASS manifest rather than leaving it on disk.
      const tampered = originalRs.replace(/"runId": "[^"]+"/, '"runId": "replay-0000000000000000"');
      writeFileSync(rsPath, tampered, 'utf8');

      let threw = false;
      try {
        execFileSync('node', [manifestScript], { cwd: REPO_ROOT, stdio: 'pipe' });
      } catch (e) {
        threw = true;
        expect(e.status).toBe(1);
      }
      expect(threw).toBe(true);

      let manifestStillExists = true;
      try {
        readFileSync(manifestPath, 'utf8');
      } catch {
        manifestStillExists = false;
      }
      expect(manifestStillExists).toBe(false);
    } finally {
      writeFileSync(rsPath, originalRs, 'utf8');
      execFileSync('node', [manifestScript], { cwd: REPO_ROOT });
    }
  });
});

test.describe('replay boundary: end-to-end TS adapter run', () => {
  test('runTsAdapter is deterministic across two invocations', () => {
    const adapterUrl = pathToFileURL(path.join(REPO_ROOT, 'scripts/replay/ts-adapter.ts')).href;
    const script = `
      const mod = await import('${adapterUrl}');
      const a = mod.runTsAdapter();
      const b = mod.runTsAdapter();
      const out = {
        runIdsMatch: a.runId === b.runId,
        inputHashEqualsSourceHash: a.inputHash === a.sourceHash,
        runId: a.runId,
        sourceHash: a.sourceHash,
        status: a.result.opportunity.status,
      };
      console.log(JSON.stringify(out));
    `;
    const tmpDir = mkdtempSync(path.join(tmpdir(), 'replay-ts-adapter-'));
    const scriptPath = path.join(tmpDir, 'run.mjs');
    writeFileSync(scriptPath, script, 'utf8');
    try {
      const stdout = execFileSync('node', ['--experimental-strip-types', scriptPath], {
        cwd: REPO_ROOT,
        encoding: 'utf8',
      });
      const lastLine = stdout.trim().split('\n').pop();
      const result = JSON.parse(lastLine);
      expect(result.runIdsMatch).toBe(true);
      expect(result.inputHashEqualsSourceHash).toBe(true);
      expect(result.runId).toMatch(/^replay-[0-9a-f]{16}$/);
      expect(result.sourceHash).toMatch(/^[0-9a-f]{64}$/);
      expect(result.status).toBe('active');
    } finally {
      rmSync(tmpDir, { recursive: true, force: true });
    }
  });
});
