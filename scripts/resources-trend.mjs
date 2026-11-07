// Reads the agent's «resources» lines (P8-7) and says whether anything keeps growing: first and last value, range and
// the least-squares slope per hour after a warm-up. Also counts the events that create and free resources.
// Usage: node scripts/resources-trend.mjs out\phase-08\RUN\rcs-agent.jsonl [WARMUP_MIN]
import { readFileSync } from 'node:fs';

const [file, warmupArg] = process.argv.slice(2);
if (!file) {
  console.error('usage: node scripts/resources-trend.mjs LOG [WARMUP_MIN]');
  process.exit(2);
}
const warmupMin = Number(warmupArg ?? 10);
const fields = ['private_mb', 'working_set_mb', 'handles', 'gdi', 'user', 'gpu_local_mb', 'gpu_non_local_mb'];
const samples = [];
const events = {};
const count = (name) => (events[name] = (events[name] ?? 0) + 1);
for (const line of readFileSync(file, 'utf8').split('\n')) {
  if (!line.trim()) continue;
  let j;
  try {
    j = JSON.parse(line);
  } catch {
    continue;
  }
  const f = j.fields ?? {};
  const at = Date.parse(j.timestamp);
  switch (f.message) {
    case 'resources':
      samples.push({ at, ...f });
      break;
    case 'session opened':
    case 'session ended':
    case 'pipeline opened':
    case 'nobody connected, pipeline and encoder closed':
    case 'stream mode, new encoder':
    case 'pipeline':
      count(f.message);
      break;
  }
}
if (samples.length < 2) {
  console.log(`only ${samples.length} samples`);
  process.exit(1);
}
const t0 = samples[0].at;
const hours = (s) => (s.at - t0) / 3_600_000;
const used = samples.filter((s) => hours(s) * 60 >= warmupMin);
console.log(
  `samples ${samples.length}, ${(hours(samples.at(-1)) * 60).toFixed(1)} min; trend after ${warmupMin} min ` +
    `on ${used.length} samples`,
);
console.log('field\tfirst\tafter warm-up\tlast\tmin\tmax\tslope/h');
for (const name of fields) {
  const all = samples.map((s) => s[name]).filter((v) => typeof v === 'number');
  const pts = used.filter((s) => typeof s[name] === 'number').map((s) => [hours(s), s[name]]);
  if (all.length === 0) continue;
  let slope = NaN;
  if (pts.length >= 2) {
    const n = pts.length;
    const mx = pts.reduce((a, [x]) => a + x, 0) / n;
    const my = pts.reduce((a, [, y]) => a + y, 0) / n;
    const sxy = pts.reduce((a, [x, y]) => a + (x - mx) * (y - my), 0);
    const sxx = pts.reduce((a, [x]) => a + (x - mx) ** 2, 0);
    slope = sxx > 0 ? sxy / sxx : 0;
  }
  const r = (v) => (Number.isFinite(v) ? Math.round(v * 10) / 10 : '—');
  console.log(
    [name, all[0], pts[0]?.[1] ?? '—', all.at(-1), Math.min(...all), Math.max(...all), r(slope)].join('\t'),
  );
}
console.log('events', JSON.stringify(events));
