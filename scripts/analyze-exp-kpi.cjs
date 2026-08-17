#!/usr/bin/env node
const fs = require('node:fs');
const path = require('node:path');

function usage(exitCode = 1) {
  const script = path.basename(__filename);
  console.error(`Usage: node scripts/${script} <logfile>`);
  process.exit(exitCode);
}

function toNum(value) {
  const n = Number(value);
  return Number.isFinite(n) ? n : null;
}

function parseNumberField(line, key) {
  const escaped = key.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const match = line.match(new RegExp(`${escaped}=(-?\\d+(?:\\.\\d+)?)`));
  return match ? toNum(match[1]) : null;
}

function parseIntField(line, key) {
  const value = parseNumberField(line, key);
  return value === null ? null : Math.trunc(value);
}

function percentile(sorted, p) {
  if (sorted.length === 0) return null;
  const idx = Math.max(0, Math.min(Math.ceil((p / 100) * sorted.length) - 1, sorted.length - 1));
  return sorted[idx];
}

function summarize(values) {
  if (values.length === 0) return null;
  let min = Number.POSITIVE_INFINITY;
  let max = Number.NEGATIVE_INFINITY;
  let sum = 0;
  for (const value of values) {
    if (value < min) min = value;
    if (value > max) max = value;
    sum += value;
  }
  return { avg: sum / values.length, min, max, count: values.length };
}

function fmt(value, digits = 2, suffix = '') {
  if (value === null || value === undefined || Number.isNaN(value)) return 'n/a';
  return `${value.toFixed(digits)}${suffix}`;
}

function main() {
  const logFile = process.argv[2];
  if (!logFile || logFile === '-h' || logFile === '--help') usage(logFile ? 0 : 1);

  const resolved = path.resolve(process.cwd(), logFile);
  if (!fs.existsSync(resolved)) {
    console.error(`Log file not found: ${resolved}`);
    process.exit(1);
  }

  const raw = fs.readFileSync(resolved, 'utf8');
  const lines = raw.split(/\r?\n/);

  const scansPerMin = [];
  const rlPerMin = [];
  const cooldownDuty = [];

  const lat = {
    cycle: { p50: [], p95: [] },
    quote: { p50: [], p95: [] },
    policy: { p50: [], p95: [] },
    submit: { p50: [], p95: [] },
  };

  const policyCounts = {
    blocked: 0,
    diag: 0,
    executable: 0,
  };

  const bestEdgePoints = [];
  let kpiWCount = 0;
  let kpiCCount = 0;

  for (const line of lines) {
    if (line.includes('[kpi-W]')) {
      kpiWCount += 1;

      const spm = parseNumberField(line, 'scans/min');
      if (spm !== null) scansPerMin.push(spm);

      const rpm = parseNumberField(line, 'rl/min');
      if (rpm !== null) rlPerMin.push(rpm);

      const duty = parseNumberField(line, 'cooldownDuty');
      if (duty !== null) cooldownDuty.push(duty);

      const exec = parseIntField(line, 'exec') ?? parseIntField(line, 'executable');
      if (exec !== null) policyCounts.executable += exec;

      const diag = parseIntField(line, 'diag');
      if (diag !== null) policyCounts.diag += diag;

      const blocked = parseIntField(line, 'blocked') ?? parseIntField(line, 'policyBlocked');
      if (blocked !== null) policyCounts.blocked += blocked;
    }

    if (line.includes('[kpi-C]')) {
      kpiCCount += 1;

      const trendMatch = line.match(/bestEdge=([^|]+)/);
      if (trendMatch) {
        const nums = trendMatch[1].match(/\$(-?\d+(?:\.\d+)?)/g) || [];
        for (const token of nums) {
          const n = toNum(token.replace('$', ''));
          if (n !== null) bestEdgePoints.push(n);
        }
      }

      const latMatch = line.match(/lat\(P50\/P95\):\s*([^|]+)/);
      if (latMatch) {
        const re = /(cycle|quote|policy|submit)=(-?\d+(?:\.\d+)?)\/(-?\d+(?:\.\d+)?)ms/g;
        let m = re.exec(latMatch[1]);
        while (m) {
          const field = m[1];
          const p50 = toNum(m[2]);
          const p95 = toNum(m[3]);
          if (p50 !== null) lat[field].p50.push(p50);
          if (p95 !== null) lat[field].p95.push(p95);
          m = re.exec(latMatch[1]);
        }
      }
    }
  }

  const scansSummary = summarize(scansPerMin);
  const rlSummary = summarize(rlPerMin);
  const dutySummary = summarize(cooldownDuty);

  console.log(`KPI log: ${path.basename(resolved)} (${kpiWCount} windowed, ${kpiCCount} cumulative)`);
  console.log(
    `scans/min avg|min|max = ${fmt(scansSummary?.avg)} | ${fmt(scansSummary?.min)} | ${fmt(scansSummary?.max)}`,
  );
  console.log(`rl/min avg = ${fmt(rlSummary?.avg)} ; cooldown duty avg = ${fmt(dutySummary?.avg, 1, '%')}`);

  const latFields = ['cycle', 'quote', 'policy', 'submit'];
  let printedLatency = false;
  for (const field of latFields) {
    const p50s = lat[field].p50.slice().sort((a, b) => a - b);
    const p95s = lat[field].p95.slice().sort((a, b) => a - b);
    const p50v = percentile(p50s, 50);
    const p95v = percentile(p95s, 50);
    if (p50v !== null || p95v !== null) {
      if (!printedLatency) {
        console.log('latency snapshot medians (ms):');
        printedLatency = true;
      }
      console.log(`  ${field} p50=${fmt(p50v, 0)} p95=${fmt(p95v, 0)}`);
    }
  }

  if (bestEdgePoints.length > 0) {
    const start = bestEdgePoints[0];
    const end = bestEdgePoints[bestEdgePoints.length - 1];
    const delta = end - start;
    const direction = delta > 0 ? 'up' : (delta < 0 ? 'down' : 'flat');
    const edgeSummary = summarize(bestEdgePoints);
    console.log(
      `bestEdge trend: ${direction} ${fmt(start)} -> ${fmt(end)} (Δ ${delta >= 0 ? '+' : ''}${fmt(delta)}; peak ${fmt(edgeSummary?.max)})`,
    );
  } else {
    console.log('bestEdge trend: n/a');
  }

  const anyPolicyCounts = Object.values(policyCounts).some((v) => v > 0);
  if (anyPolicyCounts) {
    console.log(
      `policy counts: blocked=${policyCounts.blocked} diag=${policyCounts.diag} executable=${policyCounts.executable}`,
    );
  } else {
    console.log('policy counts: n/a');
  }
}

main();
