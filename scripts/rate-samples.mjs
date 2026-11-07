// Prints the agent's once-a-second "rate sample" lines as a table, with the session clock and summary per step of
// --simulate-link (P8-1). Usage: node scripts/rate-samples.mjs out\phase-08\RUN\rcs-agent.jsonl [STEPS]
// STEPS is the --simulate-link value, for example 20:40,20:6,20:2.5,40:40.
import { readFileSync } from 'node:fs';

const [file, steps] = process.argv.slice(2);
if (!file) {
  console.error('usage: node scripts/rate-samples.mjs LOG [STEPS]');
  process.exit(2);
}
const plan = (steps ?? '')
  .split(',')
  .filter(Boolean)
  .map((s) => s.split(':').map(Number));
const cycle = plan.reduce((a, [secs]) => a + secs, 0);
const stepAt = (t) => {
  if (!cycle) return '';
  let x = t % cycle;
  for (const [secs, mbps] of plan) {
    if (x < secs) return `${mbps}`;
    x -= secs;
  }
  return '';
};

let connected = null;
const rows = [];
for (const line of readFileSync(file, 'utf8').split('\n')) {
  if (!line.trim()) continue;
  let j;
  try {
    j = JSON.parse(line);
  } catch {
    continue;
  }
  const at = Date.parse(j.timestamp);
  const f = j.fields ?? {};
  if (f.message === 'dtls connected') connected = at;
  if (f.message !== 'rate sample' || connected === null) continue;
  const t = (at - connected) / 1000;
  rows.push({ t, link: stepAt(t), ...f });
}
console.log('t,s\tlink\ttarget\testimate\tsent\tout\tdropped\tloss\trtt,ms');
for (const r of rows) {
  const k = (v) => (v === undefined || v === null ? '' : Math.round(v / 100) / 10);
  console.log(
    [r.t.toFixed(0), r.link, k(r.target_kbps), k(r.estimate_kbps), k(r.sent_kbps), k(r.out_kbps), r.dropped_test, r.loss ?? '', r.rtt_ms ?? ''].join('\t'),
  );
}
