#!/usr/bin/env node
// Warm query latency + server RSS for `graft mcp` over stdio.
// Usage: node mcp_bench.mjs <repo-name> <repo-dir> [--warmup N]
// Sends initialize, then graft_trace_calls for each line of symbols_<repo>.txt and
// graft_file_api for each line of apifiles_<repo>.txt, timing each round trip.
// Prints one JSON object to stdout.
import { spawn, execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { performance } from 'node:perf_hooks';

const here = dirname(fileURLToPath(import.meta.url));
const [name, dir] = process.argv.slice(2);
const warmup = Number(process.argv[process.argv.indexOf('--warmup') + 1]) || 3;
if (!name || !dir) throw new Error('usage: mcp_bench.mjs <repo-name> <repo-dir>');
const lines = (f) => readFileSync(join(here, f), 'utf8').split('\n').filter(Boolean);
const symbols = lines(`symbols_${name}.txt`);
const files = lines(`apifiles_${name}.txt`);

const env = { ...process.env, DO_NOT_TRACK: '1', GRAFT_NO_GITIGNORE: '1', GRAFT_NO_IGNORE: '1', GRAFT_NO_BROWSER: '1' };
// /usr/bin/time -l reports the server's max RSS on exit (stderr).
const t0 = performance.now();
const proc = spawn('/usr/bin/time', ['-l', 'graft', 'mcp', dir], { cwd: dir, env, stdio: ['pipe', 'pipe', 'pipe'] });
let stderr = '';
proc.stderr.on('data', (d) => (stderr += d));

let buf = '';
const pending = new Map();
proc.stdout.on('data', (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (!line) continue;
    const msg = JSON.parse(line);
    if (msg.id !== undefined && pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    }
  }
});

let nextId = 1;
function rpc(method, params) {
  const id = nextId++;
  return new Promise((resolve) => {
    pending.set(id, resolve);
    proc.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
}

// Sample RSS of the whole process tree under /usr/bin/time every 50 ms (async, so the
// driver's event loop is never blocked while a request is in flight).
const execFileP = promisify(execFile);
async function treeRssKB() {
  try {
    const { stdout: out } = await execFileP('ps', ['-A', '-o', 'pid=,ppid=,rss=']);
    const rows = out.trim().split('\n').map((l) => l.trim().split(/\s+/).map(Number));
    const kids = new Map();
    for (const [pid, ppid] of rows) (kids.get(ppid) ?? kids.set(ppid, []).get(ppid)).push(pid);
    const rss = new Map(rows.map(([pid, , r]) => [pid, r]));
    let total = 0;
    const stack = [...(kids.get(proc.pid) ?? [])]; // exclude /usr/bin/time itself
    while (stack.length) {
      const p = stack.pop();
      total += rss.get(p) ?? 0;
      stack.push(...(kids.get(p) ?? []));
    }
    return total;
  } catch { return 0; }
}
let peakTreeKB = 0;
let sampling = false;
const sampler = setInterval(async () => {
  if (sampling) return;
  sampling = true;
  peakTreeKB = Math.max(peakTreeKB, await treeRssKB());
  sampling = false;
}, 50);

const pct = (xs, p) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.ceil((p / 100) * s.length) - 1)];
};
const stats = (xs) => ({ n: xs.length, p50_ms: +pct(xs, 50).toFixed(2), p99_ms: +pct(xs, 99).toFixed(2), max_ms: +Math.max(...xs).toFixed(2), mean_ms: +(xs.reduce((a, b) => a + b, 0) / xs.length).toFixed(2) });

async function timed(tool, args) {
  const s = performance.now();
  const r = await rpc('tools/call', { name: tool, arguments: args });
  const ms = performance.now() - s;
  const text = (r.result?.content ?? []).map((c) => c.text ?? '').join('');
  return { ms, isError: !!(r.error || r.result?.isError), bytes: Buffer.byteLength(text) };
}

const init = await rpc('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'bench', version: '0' } });
const initMs = performance.now() - t0;
proc.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n');
const tools = (await rpc('tools/list', {})).result.tools.map((t) => t.name);

const firstTrace = await timed('graft_trace_calls', { symbol: symbols[0] });
const firstApi = await timed('graft_file_api', { file: files[0] });
for (let i = 0; i < warmup; i++) {
  await timed('graft_trace_calls', { symbol: symbols[(i + 1) % symbols.length] });
  await timed('graft_file_api', { file: files[(i + 1) % files.length] });
}

const result = { repo: name, server: init.result?.serverInfo, tools, init_ms: +initMs.toFixed(1), first_call_ms: { trace: +firstTrace.ms.toFixed(1), file_api: +firstApi.ms.toFixed(1) } };
for (const [tool, list, key] of [['graft_trace_calls', symbols, 'symbol'], ['graft_file_api', files, 'file']]) {
  const ms = [];
  let errors = 0, bytes = 0;
  for (const v of list) {
    const r = await timed(tool, { [key]: v });
    ms.push(r.ms);
    errors += r.isError;
    bytes += r.bytes;
  }
  result[tool] = { ...stats(ms), errors, mean_resp_bytes: Math.round(bytes / list.length) };
}

clearInterval(sampler);
proc.stdin.end();
await new Promise((r) => proc.on('exit', r));
const m = stderr.match(/(\d+)\s+maximum resident set size/);
result.server_maxrss_bytes = m ? Number(m[1]) : null;
result.server_tree_peak_rss_bytes_sampled = peakTreeKB * 1024;
console.log(JSON.stringify(result));
