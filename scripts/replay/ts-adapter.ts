// TypeScript replay adapter — deterministic, read-only, SYNTHETIC.
//
// Loads the shared replay fixture (replay/fixtures/replay-input-v1.json),
// simulates the same 3-hop constant-product loop the Rust adapter simulates
// (x*y=k with per-hop fee — the same formula `scanner-rust/src/sim.rs` uses for
// UniswapV2-style pools), and builds a real `CanonicalOpportunity` /
// `CanonicalExecutionPayload` using the EXISTING, already-tested shared
// contract (`supabase/functions/_shared/opportunity-contract.ts`) rather than
// reimplementing its validation/parity logic. The result is fail-closed
// validated with `validateOpportunityParity` before being wrapped in a
// `scanner-evidence-v1` envelope.
//
// SAFETY: read-only. No RPC, no live credentials, no signing/broadcasting.
// Nothing here mutates the fixture; it is only ever read.
//
// Run: node --experimental-strip-types scripts/replay/ts-adapter.ts

import { readFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

import {
  AAVE_PREMIUM_BPS,
  DEFAULT_DRYRUN_SLIPPAGE_BPS,
  MIN_HOPS,
  MAX_HOPS,
  OPPORTUNITY_REASON_CODES,
  buildRouteKey,
  createDeterministicCandidateId,
  deriveAmountBMinFromQuote,
  validateOpportunityParity,
  type CanonicalHop,
  type CanonicalOpportunity,
} from '../../supabase/functions/_shared/opportunity-contract.ts';
import { sha256Hex, deriveRunId } from './lib/hash.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, '..', '..');
export const FIXTURE_PATH = path.join(REPO_ROOT, 'replay', 'fixtures', 'replay-input-v1.json');
export const OUT_DIR = path.join(REPO_ROOT, 'replay', 'out');
export const ENVELOPE_PATH = path.join(OUT_DIR, 'ts-envelope.json');

export const ENVELOPE_VERSION = 'scanner-evidence-v1' as const;

interface FixtureEdge {
  tokenIn: string;
  tokenOut: string;
  router: string;
  dex: string;
  price: number;
  feePpm: number;
  reserveIn: string;
  reserveOut: string;
  decIn: number;
  decOut: number;
  isV3: boolean;
}

interface Fixture {
  fixtureVersion: string;
  description: string;
  network: string;
  edges: FixtureEdge[];
  usdPrices: Record<string, number>;
  pipelineParams: {
    aaveBps: number;
    loanSizesUsd: number[];
    gasUsdPerHop: number;
    slippageBps: number;
  };
}

/** Exact constant-product-with-fee swap, mirroring scanner-rust's V2 sim math:
 * `out = in*(1-fee)*reserveOut / (reserveIn + in*(1-fee))`. Uses `number` (not
 * bigint) because the fixture's USD-denominated loan sizes are small relative
 * to the 1e24-scale reserves and this is a REPLAY-ONLY economic estimate, not
 * an on-chain-exact integer computation (that fidelity lives in the Rust sim).
 */
function simulateV2Hop(amountIn: number, edge: FixtureEdge): number {
  const feeFraction = edge.feePpm / 1_000_000;
  const reserveIn = Number(edge.reserveIn) / 10 ** edge.decIn;
  const reserveOut = Number(edge.reserveOut) / 10 ** edge.decOut;
  const amountInAfterFee = amountIn * (1 - feeFraction);
  return (amountInAfterFee * reserveOut) / (reserveIn + amountInAfterFee);
}

/** Sweep the fixture's candidate loan sizes through the full hop chain and keep
 * the best realized net, mirroring `pipeline::best_trace`'s sweep-and-keep-best
 * strategy on the Rust side. Returns per-hop outputs for the winning loan size.
 */
function simulateLoop(fixture: Fixture) {
  const { edges, pipelineParams } = fixture;
  const startUsd = fixture.usdPrices[edges[0].tokenIn] ?? 1;
  let best: null | {
    loanUsd: number;
    hopOutputs: number[];
    realizedRatio: number;
    grossAfterFeesUsd: number;
    aavePremiumUsd: number;
    gasCostUsd: number;
    netProfitUsd: number;
  } = null;

  for (const loanUsd of pipelineParams.loanSizesUsd) {
    const startAmount = loanUsd / startUsd;
    let amount = startAmount;
    const hopOutputs: number[] = [];
    for (const edge of edges) {
      amount = simulateV2Hop(amount, edge);
      hopOutputs.push(amount);
    }
    const realizedRatio = amount / startAmount;
    const grossAfterFeesUsd = (realizedRatio - 1) * loanUsd;
    const aavePremiumUsd = (loanUsd * pipelineParams.aaveBps) / 10_000;
    const gasCostUsd = pipelineParams.gasUsdPerHop * edges.length;
    const netProfitUsd = grossAfterFeesUsd - aavePremiumUsd - gasCostUsd;
    if (!best || netProfitUsd > best.netProfitUsd) {
      best = { loanUsd, hopOutputs, realizedRatio, grossAfterFeesUsd, aavePremiumUsd, gasCostUsd, netProfitUsd };
    }
  }
  if (!best) throw new Error('replay fixture must declare at least one loan size');
  return best;
}

function applySlippageBps(amount: number, slippageBps: number): number {
  return (amount * (10_000 - slippageBps)) / 10_000;
}

export function runTsAdapter() {
  const raw = readFileSync(FIXTURE_PATH);
  const sourceHash = sha256Hex(raw);
  // The fixture is read and consumed verbatim (no pre-processing before use),
  // so inputHash == sourceHash by construction. Kept as a distinct envelope
  // field for schema clarity and to allow a future adapter stage that
  // transforms the input before consuming it without changing the schema.
  const inputHash = sourceHash;
  const runId = deriveRunId(sourceHash);

  const fixture = JSON.parse(raw.toString('utf8')) as Fixture;
  if (fixture.edges.length < MIN_HOPS || fixture.edges.length > MAX_HOPS) {
    throw new Error(`replay fixture hop count ${fixture.edges.length} outside [${MIN_HOPS},${MAX_HOPS}]`);
  }

  const trace = simulateLoop(fixture);
  const network = fixture.network;
  const asset = fixture.edges[0].tokenIn;
  const buyDex = fixture.edges[0].dex;
  const sellDex = fixture.edges[fixture.edges.length - 1].dex;
  const tokenPair = `${network}:${asset}/${asset}`; // closed loop: borrowed == repaid asset
  const routeKey = buildRouteKey(network, tokenPair, buyDex, sellDex);
  const status: 'active' | 'watchlist' = trace.netProfitUsd > 0 ? 'active' : 'watchlist';
  const reasonCode = trace.netProfitUsd > 0
    ? OPPORTUNITY_REASON_CODES.activeExecutionReady
    : OPPORTUNITY_REASON_CODES.watchlistNetProfitBelowThreshold;
  const candidateId = createDeterministicCandidateId(runId, routeKey, status);
  // Fixed, non-wall-clock quote timestamp so the envelope's economic content
  // (everything validated by validateOpportunityParity) stays fully
  // deterministic run-to-run; `generatedAt` (wall clock) is recorded
  // separately, outside the hashed/validated payload.
  const quoteTimestamp = '2024-01-01T00:00:00.000Z';

  const startUsd = fixture.usdPrices[asset] ?? 1;
  const buyPrice = fixture.edges[0].price;
  const tokenBDecimals = fixture.edges[0].decOut;
  const amountBMin = deriveAmountBMinFromQuote({
    loanAmountUsd: trace.loanUsd,
    quoteTokenUsdPrice: startUsd,
    buyPrice,
    estimatedSlippageBps: fixture.pipelineParams.slippageBps,
    tokenBDecimals,
  }).toString();

  const hops: CanonicalHop[] = fixture.edges.map((edge, i) => ({
    router: edge.router,
    tokenOut: edge.tokenOut,
    isV3: edge.isV3,
    fee: edge.feePpm,
    amountOutMin: Math.floor(applySlippageBps(trace.hopOutputs[i], fixture.pipelineParams.slippageBps)).toString(),
  }));

  const opportunity: CanonicalOpportunity = {
    tokenPair,
    buyDex,
    sellDex,
    network,
    loanAmount: trace.loanUsd,
    executableLoanAmount: trace.loanUsd,
    grossProfit: trace.grossAfterFeesUsd,
    netProfit: trace.netProfitUsd,
    distanceToExecutableUsd: Math.max(0, -trace.netProfitUsd),
    gasCost: trace.gasCostUsd,
    confidenceScore: trace.netProfitUsd > 0 ? 90 : 40,
    confidenceTier: trace.netProfitUsd > 0 ? 'high' : 'low',
    spread: (trace.realizedRatio - 1).toFixed(6),
    liquidity: fixture.edges[0].reserveIn,
    estimatedSlippageBps: fixture.pipelineParams.slippageBps,
    buyImpactBps: 0,
    sellImpactBps: 0,
    routePenaltyBps: 0,
    status,
    quoteSources: ['replay-fixture'],
    scanRunId: runId,
    candidateId,
    quoteTimestamp,
    dataSource: 'multi-source',
    reasonCode,
    executionPayload: {
      asset,
      amount: trace.loanUsd.toString(),
      routerA: fixture.edges[0].router,
      routerB: fixture.edges[fixture.edges.length - 1].router,
      tokenB: fixture.edges[0].tokenOut,
      routerAisV3: fixture.edges[0].isV3,
      routerBisV3: fixture.edges[fixture.edges.length - 1].isV3,
      feeA: fixture.edges[0].feePpm,
      feeB: fixture.edges[fixture.edges.length - 1].feePpm,
      amountBMin,
      tokenPair,
      buyDex,
      sellDex,
      network,
      predictedGrossProfit: trace.grossAfterFeesUsd,
      predictedNetProfit: trace.netProfitUsd,
      estimatedGasCost: trace.gasCostUsd,
      estimatedSlippageBps: fixture.pipelineParams.slippageBps,
      scanTimestamp: quoteTimestamp,
      confidenceScore: trace.netProfitUsd > 0 ? 90 : 40,
      quote: {
        version: 'scanner-opportunity-v1',
        routeKey,
        quoteTimestamp,
        quoteTokenUsdPrice: startUsd,
        buyPrice,
        expectedBuyTokenAmount: trace.hopOutputs[0].toString(),
        amountBMin,
        tokenBDecimals,
        slippageBps: fixture.pipelineParams.slippageBps,
        sourceQualityBps: 9_500,
        persistenceCount: 2,
        minRequiredPersistence: 2,
        sourceFlags: { hasSubgraph: true, fallbackOnly: false, sameFallbackSource: false },
      },
      hops,
    },
  };

  const parity = validateOpportunityParity(opportunity, { maxQuoteAgeMs: 0 });
  if (!parity.ok) {
    // Fail-closed: never emit an envelope built from a payload the shared
    // contract itself considers invalid/inconsistent.
    throw new Error(`replay ts-adapter: opportunity failed validateOpportunityParity: ${parity.errors.join(', ')}`);
  }

  const envelope = {
    envelopeVersion: ENVELOPE_VERSION,
    runId,
    inputHash,
    sourceHash,
    adapter: 'ts' as const,
    fixturePath: path.relative(REPO_ROOT, FIXTURE_PATH).replace(/\\/g, '/'),
    fixtureVersion: fixture.fixtureVersion,
    generatedAt: new Date().toISOString(),
    readOnly: true,
    disclaimer:
      'SYNTHETIC REPLAY EVIDENCE — deterministic fixture replay only. NOT live data, NOT production evidence, NOTHING signed or broadcast.',
    result: {
      opportunity,
      trace: {
        loanUsd: trace.loanUsd,
        realizedRatio: trace.realizedRatio,
        netProfitUsd: trace.netProfitUsd,
        hops: fixture.edges.length,
      },
    },
  };

  return envelope;
}

function main() {
  const envelope = runTsAdapter();
  mkdirSync(OUT_DIR, { recursive: true });
  writeFileSync(ENVELOPE_PATH, `${JSON.stringify(envelope, null, 2)}\n`, 'utf8');
  console.log(`[replay:ts] wrote ${path.relative(REPO_ROOT, ENVELOPE_PATH)}`);
  console.log(`[replay:ts] runId=${envelope.runId} inputHash=${envelope.inputHash} sourceHash=${envelope.sourceHash}`);
  console.log(
    `[replay:ts] status=${envelope.result.opportunity.status} netProfitUsd=${envelope.result.trace.netProfitUsd.toFixed(4)}`,
  );
}

const isMain = process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1]);
if (isMain) {
  main();
}
