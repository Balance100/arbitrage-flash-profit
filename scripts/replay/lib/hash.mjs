// Deterministic hashing helpers for the replay boundary.
//
// sha256Hex(bytes) hashes raw bytes (or a utf8 string) and returns a lowercase
// hex digest. deriveRunId(sourceHash) derives a stable runId from a hash by
// hashing its ASCII hex text — this is language-agnostic (both the TypeScript
// and Rust adapters hash the SAME hex string, never a re-serialized JSON
// value), so it produces byte-identical results in both runtimes without
// depending on any language's JSON/float canonicalization.
import { createHash } from 'node:crypto';

export function sha256Hex(data) {
  const buf = typeof data === 'string' ? Buffer.from(data, 'utf8') : Buffer.from(data);
  return createHash('sha256').update(buf).digest('hex');
}

export function deriveRunId(sourceHash) {
  const digest = sha256Hex(sourceHash);
  return `replay-${digest.slice(0, 16)}`;
}
