#!/usr/bin/env node
// Linux/macOS end-to-end resource profile. Node >=22; no npm dependencies.
// Usage: node scripts/resource-profile.mjs BINARY OUTPUT_DIR [claude-code|mock]
// DISPLAY must name a working X server; unset WAYLAND_DISPLAY for Xvfb.
// Uses isolated local data and a real harness (Haiku by default). This costs
// one API turn. RSS/CPU for the CLI child are reported separately from Zeron.
import { spawn, execFileSync } from 'node:child_process';
import { mkdirSync, openSync, readFileSync, writeFileSync, existsSync, readdirSync, copyFileSync, createReadStream } from 'node:fs';
import { resolve } from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { setTimeout as sleep } from 'node:timers/promises';
import { createServer } from 'node:net';
import { fileURLToPath } from 'node:url';

const macOS = process.platform === 'darwin';
const nativeWindowSource = fileURLToPath(new URL('./macos-profile-window.swift', import.meta.url));
if (macOS) execFileSync('swift', [nativeWindowSource]);

const [binaryArg, outputArg, harness = 'claude-code'] = process.argv.slice(2);
if (!binaryArg || !outputArg) throw Error('Usage: resource-profile.mjs BINARY OUTPUT_DIR [claude-code|mock]');
const binary = resolve(binaryArg), output = resolve(outputArg);
if (existsSync(output)) throw Error('Use a new output directory for an isolated run');
mkdirSync(output, { recursive: true });
const nativeStat = `${output}/macos-resource-stat`;
const nativeWindow = `${output}/macos-profile-window`;
if (macOS) execFileSync('xcrun', ['clang', '-O2', '-Wall', '-Wextra',
  fileURLToPath(new URL('./macos-resource-stat.c', import.meta.url)), '-o', nativeStat]);
if (macOS) execFileSync('swiftc', ['-O', nativeWindowSource, '-o', nativeWindow]);
// Shared Cargo targets can be replaced by another worktree mid-profile.
// Both processes must execute the exact same immutable build throughout.
const profiledBinary = `${output}/zeron-profiled`;
copyFileSync(binary, profiledBinary);
const binaryHash = createHash('sha256');
for await (const chunk of createReadStream(profiledBinary)) binaryHash.update(chunk);
const binarySha256 = binaryHash.digest('hex');
const cwd = `${output}/project`;
mkdirSync(cwd);
execFileSync('git', ['init', '-q', cwd]);
mkdirSync(`${output}/ui`);
writeFileSync(`${output}/ui/composer-defaults.json`, JSON.stringify({
  harness, modelByHarness: { [harness]: {
    id: harness === 'mock' ? 'fable-5' : 'claude-haiku-4-5', label: harness === 'mock' ? 'Fable 5' : 'Haiku',
  } },
  modelLabels: { 'claude-haiku-4-5': 'Haiku' }, reasoning: null,
}));
const server = createServer();
await new Promise(r => server.listen(0, '127.0.0.1', r));
const port = server.address().port;
await new Promise(r => server.close(r));
const env = { ...process.env, ZERON_IPC_PORT: String(port), ZERON_DATA_DIR: `${output}/engine`,
  ZERON_HARNESS: harness, ZERON_FRAME_STATS: process.env.ZERON_FRAME_STATS ?? '1', RUST_LOG: 'warn',
  ZERON_MOCK_CHARS: '24', ZERON_MOCK_DELAY_MS: '20' };
delete env.ZERON_EDGE_TOKEN;
delete env.ZERON_ORG_ID;
const processes = [];
function start(args, name, extra = {}) {
  const log = openSync(`${output}/${name}.log`, 'w');
  const child = spawn(profiledBinary, args, { env: { ...env, ...extra }, detached: true, stdio: ['ignore', log, log] });
  processes.push(child);
  return child;
}
const engine = start(['headless'], 'engine');
let ws, sampler;
const samples = [], frames = [], pending = new Map();
let phase = 'startup', id = 0, transcript = [], streamError;
const hz = Number(execFileSync('getconf', ['CLK_TCK'], { encoding: 'utf8' }).trim());
function stat(pid) {
  try {
    if (macOS) return JSON.parse(execFileSync(nativeStat, [String(pid)], { encoding: 'utf8' }))[0] ?? null;
    const stat = readFileSync(`/proc/${pid}/stat`, 'utf8').split(') ')[1].split(' ');
    const main = readFileSync(`/proc/${pid}/task/${pid}/stat`, 'utf8').split(') ')[1].split(' ');
    const status = readFileSync(`/proc/${pid}/status`, 'utf8');
    // Optional proportional memory avoids counting shared mappings twice.
    const pss = process.env.ZERON_PROFILE_PSS === '1'
      ? { pssMiB: Number(readFileSync(`/proc/${pid}/smaps_rollup`, 'utf8').match(/^Pss:\s+(\d+)/m)?.[1] ?? 0) / 1024 }
      : {};
    return { pid, ...pss, cpuSeconds: (Number(stat[11]) + Number(stat[12])) / hz,
      mainCpuSeconds: (Number(main[11]) + Number(main[12])) / hz,
      rssMiB: Number(status.match(/VmRSS:\s+(\d+)/)?.[1] ?? 0) / 1024,
      threads: Number(status.match(/Threads:\s+(\d+)/)?.[1] ?? 0) };
  } catch { return null; }
}
function descendants(pid) {
  try {
    if (macOS) {
      const rows = execFileSync('ps', ['-axo', 'pid=,ppid='], { encoding: 'utf8' })
        .trim().split('\n').map(line => line.trim().split(/\s+/).map(Number));
      const walk = parent => rows.filter(row => row[1] === parent).flatMap(([child]) => [child, ...walk(child)]);
      return walk(pid);
    }
    // A Tokio worker can own the subprocess: inspect all task children.
    const tasks = readdirSync(`/proc/${pid}/task`);
    const children = new Set(tasks.flatMap(t => {
      try { return readFileSync(`/proc/${pid}/task/${t}/children`, 'utf8').trim().split(/\s+/).filter(Boolean).map(Number); }
      catch { return []; }
    }));
    return [...children].flatMap(p => [p, ...descendants(p)]);
  } catch { return []; }
}
function apply(frame) {
  if (frame.reset) { transcript = frame.reset; return; }
  for (const remove of frame.remove ?? []) transcript = transcript.filter(e => e.id !== remove);
  for (const { entry, after } of frame.upsert ?? []) {
    transcript = transcript.filter(e => e.id !== entry.id);
    transcript.splice(after == null ? 0 : transcript.findIndex(e => e.id === after) + 1, 0, entry);
  }
  for (const append of frame.append ?? []) {
    const entry = transcript.find(e => e.id === append.entry);
    if (!entry) throw Error('Append without entry');
    const part = entry.parts.find(p => p.id === append.part);
    if (!part || typeof part.text !== 'string') throw Error('Append without text part');
    part.text += append.text;
    if (Buffer.byteLength(part.text) !== append.len) throw Error('Append length mismatch');
  }
  if (transcript.length !== frame.count) throw Error('Transcript count mismatch');
}
try {
  for (let attempt = 0; attempt < 120; attempt++) {
    if (engine.exitCode != null) throw Error('Engine exited; inspect engine.log');
    try {
      ws = new WebSocket(`ws://127.0.0.1:${port}`);
      await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
      break;
    } catch { await sleep(250); }
  }
  if (ws?.readyState !== WebSocket.OPEN) throw Error('IPC startup timed out');
  ws.onmessage = event => {
    const frame = JSON.parse(event.data);
    const request = pending.get(frame.id);
    if ('err' in frame) { request?.reject(Error(JSON.stringify(frame.err))); return; }
    if ('ok' in frame) { pending.delete(frame.id); request?.resolve(frame.ok); }
    if ('item' in frame) {
      try { request?.item?.(frame.item); } catch (error) { streamError = error; }
    }
  };
  const call = (method, params) => new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(Error(`${method} timed out`)), 30000);
    timer.unref();
    pending.set(++id, {
      resolve: value => { clearTimeout(timer); resolve(value); },
      reject: error => { clearTimeout(timer); reject(error); },
    });
    ws.send(JSON.stringify({ id, method, params }));
  });
  await call('EngineReady', {});
  const { deviceId } = await call('LocalDevice', {});
  const spaceId = randomUUID(), chatId = randomUUID();
  await call('Mutate', { op: 'createSpace', spaceId, deviceId, path: cwd });
  await call('Mutate', { op: 'createChat', chatId, spaceId,
    config: { harness, model: harness === 'mock' ? 'fable-5' : 'claude-haiku-4-5', reasoning: null, sandbox: 'workspace-write' } });
  await call('Mutate', { op: 'renameChat', chatId, title: 'Resource profile' });
  const backgroundChats = Number(process.env.ZERON_PROFILE_BACKGROUND_CHATS ?? 0);
  if (!Number.isSafeInteger(backgroundChats) || backgroundChats < 0) throw Error('Invalid background chat count');
  for (let index = 0; index < backgroundChats; index++) {
    const backgroundId = randomUUID();
    await call('Mutate', { op: 'createChat', chatId: backgroundId, spaceId });
    await call('Mutate', { op: 'renameChat', chatId: backgroundId, title: `Background conversation ${index + 1}` });
  }
  pending.set(++id, { reject: e => { streamError = e; }, item: frame => {
    // apply() reuses reset/upsert objects as the mutable transcript. Capture
    // first so later append frames cannot rewrite earlier replay records.
    frames.push({ at: Date.now(), frame: structuredClone(frame) }); apply(frame);
  } });
  ws.send(JSON.stringify({ id, method: 'WatchDocMessages', params: { chatId } }));
  const locator = createHash('sha256').update(`Local\0device:${deviceId}`).digest('hex').slice(0, 16);
  const ui = start([`zeron://open/chat/${chatId}?workspace=${locator}`], 'ui', { ZERON_DATA_DIR: `${output}/ui` });
  writeFileSync(`${output}/pids.json`, JSON.stringify({engine: engine.pid, ui: ui.pid}));
  // Native windows activate themselves. Let the initial layout/splash settle.
  if (macOS) {
    await sleep(2000);
    execFileSync(nativeWindow, [String(ui.pid)]);
  }
  // Xvfb has no window manager to send the initial configure/focus events.
  // Force first paint before measuring (otherwise a mapped, transparent
  // window can consume no rendering CPU for the entire workload).
  if (!macOS && process.env.DISPLAY && !process.env.WAYLAND_DISPLAY) {
    await sleep(2000);
    const windows = execFileSync('xdotool', ['search', '--class', '^zeron$'], { encoding: 'utf8' }).trim().split('\n');
    if (windows.length !== 1) throw Error('Use a dedicated X display with exactly one Zeron window');
    execFileSync('xdotool', ['windowraise', windows[0], 'windowsize', windows[0], '1280', '800', 'windowfocus', windows[0]]);
  }
  sampler = setInterval(() => {
    if (macOS) {
      try { execFileSync(nativeWindow, [String(ui.pid), '--check'], { stdio: 'pipe' }); }
      catch (error) {
        const reason = error.stderr?.toString().trim() || error.message;
        streamError = Error(`Native window validation failed: ${reason}`);
      }
    }
    samples.push({ at: Date.now(), phase, engine: stat(engine.pid), ui: stat(ui.pid),
      harness: descendants(engine.pid).map(stat).filter(Boolean) });
  }, 500);
  phase = 'idle';
  await sleep(Number(process.env.ZERON_PROFILE_PRE_IDLE_MS ?? 10000));
  if (ui.exitCode != null) throw Error('UI exited; inspect ui.log');
  phase = 'stream';
  const prompt = process.env.ZERON_PROFILE_PROMPT ??
    'Do not use tools. Write a detailed tutorial of Rust ownership in exactly 80 numbered sections. Each section should have a heading, a paragraph of at least 60 words and a short Rust code example. Continue through all 80 sections without asking questions.';
  const turns = Number(process.env.ZERON_PROFILE_TURNS ?? 1);
  if (!Number.isSafeInteger(turns) || turns < 1) throw Error('ZERON_PROFILE_TURNS must be a positive integer');
  for (let turn = 0; turn < turns; turn++) {
    const priorAssistants = new Set(transcript.filter(e => e.role === 'assistant').map(e => e.id));
    if (process.env.ZERON_PROFILE_SUBMIT_UI === '1') {
      // Functional mode exercises the composer's own-turn scroll anchoring.
      // Use a dedicated display; the driver focused the app's composer above.
      if (macOS) {
        const promptFile = `${output}/prompt.txt`;
        writeFileSync(promptFile, prompt);
        execFileSync(nativeWindow, [String(ui.pid), '--submit', promptFile]);
      } else {
        execFileSync('xdotool', ['type', '--clearmodifiers', '--delay', '0', '--', prompt]);
        execFileSync('xdotool', ['key', 'Return']);
      }
    } else {
      await call('QueueCommand', { chatId, command: { kind: 'run', messageId: randomUUID(), request: {
        prompt, model: harness === 'mock' ? null : 'haiku', reasoning: null, modelOptions: {}, cwd,
        sandbox: 'workspace-write', autoApprove: true, resume: null,
      } } });
    }
    const timeoutMs = Number(process.env.ZERON_PROFILE_TIMEOUT_MS ?? 240000);
    const deadline = Date.now() + timeoutMs;
    let complete = false;
    while (Date.now() < deadline) {
      await sleep(500);
      if (streamError) throw streamError;
      const assistants = transcript.filter(e => e.role === 'assistant' && !priorAssistants.has(e.id));
      if (assistants.length && assistants.at(-1).status !== 'streaming') {
        if (assistants.at(-1).status !== 'complete') throw Error('Harness turn did not complete successfully');
        if (assistants.some(e => e.parts.some(p => p.kind === 'error'))) {
          throw Error('Harness turn returned an error part');
        }
        complete = true; break;
      }
      if (ui.exitCode != null || engine.exitCode != null) throw Error('Profiled process exited');
    }
    if (!complete) throw Error(`Turn did not complete within ${timeoutMs / 1000} seconds`);
    if (process.env.ZERON_PROFILE_SUBMIT_UI === '1') {
      const sent = transcript.filter(e => e.role === 'user').at(-1)?.parts
        .filter(p => p.kind === 'text').map(p => p.text).join('');
      if (sent !== prompt) throw Error('Composer input did not submit the exact requested prompt');
    }
    if (turn + 1 < turns) await sleep(500);
  }
  phase = 'settled';
  await sleep(Number(process.env.ZERON_PROFILE_IDLE_MS ?? 15000));
  if (streamError) throw streamError;
  if (macOS) execFileSync(nativeWindow);
  const reply = transcript.filter(e => e.role === 'assistant').flatMap(e => e.parts)
    .filter(p => p.kind === 'text' || p.kind === 'reasoning').map(p => p.text).join('\n\n');
  const summary = { binary, binarySha256, harness, mode: process.env.ZERON_REPLAY_JOURNAL ? 'replay' : 'live',
    platform: process.platform, arch: process.arch,
    renderThreads: process.env.LP_NUM_THREADS ?? 'default', frameStats: env.ZERON_FRAME_STATS,
    submission: process.env.ZERON_PROFILE_SUBMIT_UI === '1' ? 'composer' : 'rpc', turns, backgroundChats, prompt,
    replySha256: createHash('sha256').update(reply).digest('hex'), replyBytes: Buffer.byteLength(reply),
    transcriptBytes: Buffer.byteLength(JSON.stringify(transcript)), frames: frames.length, phases: {} };
  for (const p of ['idle', 'stream', 'settled']) {
    const rows = samples.filter(s => s.phase === p);
    summary.phases[p] = { durationSeconds: (rows.at(-1).at - rows[0].at) / 1000 };
    for (const name of ['engine', 'ui']) {
      const valid = rows.filter(r => r[name]);
      const first = valid[0], last = valid.at(-1);
      summary.phases[p][name] = {
        ...(macOS ? { peakFootprintMiB: Math.max(...valid.map(r => r[name].footprintMiB)),
          endFootprintMiB: last[name].footprintMiB,
          lifetimePeakFootprintMiB: last[name].lifetimePeakFootprintMiB } : {}),
        ...(first[name].pssMiB == null ? {} : {
          peakPssMiB: Math.max(...valid.map(r => r[name].pssMiB)), endPssMiB: last[name].pssMiB,
        }), peakRssMiB: Math.max(...valid.map(r => r[name].rssMiB)),
        endRssMiB: last[name].rssMiB,
        cpuPercent: 100000 * (last[name].cpuSeconds - first[name].cpuSeconds) / (last.at - first.at),
        ...(macOS ? {} : { mainCpuPercent: 100000 * (last[name].mainCpuSeconds - first[name].mainCpuSeconds) / (last.at - first.at) }) };
    }
  }
  // Report the last ten seconds separately from the completion transition.
  const settled = samples.filter(s => s.phase === 'settled');
  const quiet = settled.filter(s => s.at >= settled.at(-1).at - 10000);
  summary.postCompletionTail = { durationSeconds: (quiet.at(-1).at - quiet[0].at) / 1000 };
  for (const name of ['engine', 'ui']) {
    const first = quiet[0], last = quiet.at(-1);
    summary.postCompletionTail[name] = {
      ...(macOS ? { endFootprintMiB: last[name].footprintMiB } : {}),
      endRssMiB: last[name].rssMiB,
      cpuPercent: 100000 * (last[name].cpuSeconds - first[name].cpuSeconds) / (last.at - first.at),
      ...(macOS ? {} : { mainCpuPercent: 100000 * (last[name].mainCpuSeconds - first[name].mainCpuSeconds) / (last.at - first.at) }),
    };
  }
  writeFileSync(`${output}/summary.json`, JSON.stringify(summary, null, 2));
  console.log(JSON.stringify(summary, null, 2));
  // Optional machine-specific regression budgets. Do not bake software-GPU
  // numbers into a universal desktop threshold.
  for (const name of ['engine', 'ui']) {
    const budget = process.env[`ZERON_MAX_${name.toUpperCase()}_RSS_MIB`];
    if (budget == null) continue;
    const limit = Number(budget);
    if (!Number.isFinite(limit) || limit <= 0) throw Error(`Invalid ${name} RSS budget`);
    const peak = Math.max(...Object.values(summary.phases).map(p => p[name].peakRssMiB));
    if (peak > limit) throw Error(`${name} peak RSS ${peak.toFixed(1)} MiB exceeds ${limit} MiB`);
  }
  // Keep a verified conversation open for manual interaction checks without
  // counting those interactions in the measured phases.
  clearInterval(sampler);
  if (process.env.ZERON_PROFILE_HOLD_MS) await sleep(Number(process.env.ZERON_PROFILE_HOLD_MS));
} finally {
  clearInterval(sampler);
  writeFileSync(`${output}/samples.json`, JSON.stringify(samples));
  writeFileSync(`${output}/frames.json`, JSON.stringify(frames));
  writeFileSync(`${output}/transcript.json`, JSON.stringify(transcript));
  ws?.close();
  for (const child of processes.reverse()) {
    try { process.kill(-child.pid, 'SIGTERM'); } catch { /* already exited */ }
  }
}
