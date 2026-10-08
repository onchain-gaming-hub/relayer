// E2E against `wrangler dev` + mock-upstream. Usage: node tests/e2e.mjs <A|B|C>
// Test ids: T# = v1 spec §7, T-v2-# = v2 spec §8. Policy-shape and registry-parsing cases are
// covered by Rust unit tests (src/policy.rs, src/registry.rs). Projects are seeded by run-e2e.sh.
import http from 'node:http';
import { execFileSync } from 'node:child_process';

const SUITE = process.argv[2];
const RELAYER = process.env.RELAYER_URL || 'http://127.0.0.1:8788';
const MOCK = process.env.MOCK_URL || 'http://127.0.0.1:8899';
const ORIGIN = 'http://localhost:5173';
const PERSIST = process.env.PERSIST_DIR;
const WRANGLER_LOG = process.env.WRANGLER_LOG;
const { PK_A, PK_B, PK_M, PK_X, PK_Y, PK_C, PK_D, PK_G, TESTKIT } = process.env;
const ORIGIN_G = 'http://localhost:5181';
const SYSTEM = '11111111111111111111111111111111';
const ORIGIN_B = 'http://localhost:5174';
const ORIGIN_M = 'http://localhost:5175';
const ISS_MAINNET = 'https://issuer-mainnet.test';
import fs from 'node:fs';
const BLOCKHASH = '11111111111111111111111111111111';

const results = [];
const check = (id, ok, detail = '') => { results.push({ id, ok, detail }); console.log(`${ok ? 'PASS' : 'FAIL'} ${id} ${detail}`); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let seedN = 0;
// Distinct wallets per suite (D2 reuses D's state, so it needs its own range).
const SEED_BASE = SUITE === 'D2' ? 3000 : SUITE.charCodeAt(0) * 16;
const freshSeed = () => (++seedN + SEED_BASE).toString(16).padStart(2, '0').repeat(32);

function request(path, { method = 'POST', headers = {}, body, chunked = false } = {}) {
  return new Promise((resolve, reject) => {
    const u = new URL(RELAYER + path);
    // agent:false -> fresh socket per request, so an early response that leaves an unread body
    // on a keep-alive socket cannot poison the next test.
    const req = http.request({ hostname: u.hostname, port: u.port, path: u.pathname + u.search, method, headers, agent: false }, (res) => {
      let data = '';
      res.on('data', (c) => (data += c));
      res.on('end', () => {
        let json = null;
        try { json = JSON.parse(data); } catch {}
        resolve({ status: res.statusCode, headers: res.headers, json, text: data });
      });
    });
    req.on('error', reject);
    if (body !== undefined) {
      if (chunked) { for (let i = 0; i < body.length; i += 1000) req.write(body.slice(i, i + 1000)); }
      else req.write(body);
    }
    req.end();
  });
}

const mock = async (path, body) => (await fetch(MOCK + path, body ? { method: 'POST', body: JSON.stringify(body) } : {})).json();
const setState = (s) => mock('/__state', s);
const stats = () => mock('/__stats');
const tokenFor = async (user_hex, extra = {}) => (await mock('/__token?' + new URLSearchParams({ user_hex, ...extra }))).token;

let relayerPubkey;
function mk(seed, mode = '', feePayer = relayerPubkey) {
  const out = execFileSync('./target/debug/examples/mktx', [feePayer, BLOCKHASH, seed ?? '-', mode]);
  return JSON.parse(out.toString());
}

async function relay(t, { token, origin = ORIGIN, contentType = 'application/json', body } = {}) {
  const headers = { 'content-type': contentType };
  if (origin) headers.origin = origin;
  if (token) headers.authorization = `Bearer ${token}`;
  return request('/relay', { headers, body: body ?? JSON.stringify({ transaction: t.tx }) });
}

/** Builds a fresh tx for `seed` and relays it with a valid token for that user. */
async function relayAs(seed, opts = {}) {
  const t = mk(seed, opts.mode, opts.feePayer);
  return relay(t, { token: opts.token ?? (await tokenFor(t.user_hex, opts.claims ?? {})), ...opts });
}
/** Same for project B (its fee payer, clientId and origin). */
const relayAsB = (seed, opts = {}) =>
  relayAs(seed, { feePayer: PK_B, claims: { aud: 'client-b' }, origin: ORIGIN_B, ...opts });

/** GET /quota for the user behind `seed` in project `aud` (default A). */
async function quotaOf(seed, { aud = 'test-client', origin = ORIGIN, token } = {}) {
  const t = mk(seed);
  const tok = token ?? (await tokenFor(t.user_hex, { aud }));
  const headers = { origin };
  if (tok) headers.authorization = `Bearer ${tok}`;
  return request(`/quota?wallet=${t.user}`, { method: 'GET', headers });
}
const nextUtcMidnight = () => (Math.floor(Date.now() / 86_400_000) + 1) * 86_400_000;

function d1Exec(sql) {
  execFileSync('npx', ['wrangler', 'd1', 'execute', 'gasless-relayer-audit', '--local', '--persist-to', PERSIST,
    '--command', sql], { stdio: 'ignore' });
}
/** Number of registry snapshot loads (D1 queries) so far, from the wrangler log. */
const registryLoads = () => (fs.readFileSync(WRANGLER_LOG, 'utf8').match(/registry: loaded/g) || []).length;

function d1Query(sql) {
  const out = execFileSync('npx', ['wrangler', 'd1', 'execute', 'gasless-relayer-audit', '--local', '--persist-to', PERSIST,
    '--command', sql, '--json'], { stdio: ['ignore', 'pipe', 'ignore'] });
  return JSON.parse(out.toString())[0].results;
}
function d1Counts() {
  const rows = d1Query('SELECT status, count(*) AS c FROM relay_log GROUP BY status');
  return Object.fromEntries(rows.map((r) => [r.status, r.c]));
}

async function suiteA() {
  // Baseline for T15 (audit rows == RPC sends over the whole suite).
  const s0 = await stats();
  const info = await request('/relayer?project=test-client', { method: 'GET' });
  check('relayer-info', info.status === 200 && info.json?.pubkey === PK_A && info.json?.cluster === 'devnet'
    && info.json?.policy?.actions?.[0]?.type === 'memo' && info.json?.policy?.actions?.[0]?.prefix === 'gasless-poc:', info.text);
  check('relayer-info cooldown_ms is an integer in the raw JSON', /"cooldown_ms":2000[,}]/.test(info.text) && info.json?.cooldown_ms === 2000, info.text);
  check('relayer-info-bad-origin', (await request('/relayer?project=test-client', { method: 'GET', headers: { origin: 'https://evil.test' } })).status === 403);
  const pre = await request('/relay', { method: 'OPTIONS', headers: { origin: ORIGIN } });
  check('cors-preflight', pre.status === 204 && pre.headers['access-control-allow-origin'] === ORIGIN && /authorization/i.test(pre.headers['access-control-allow-headers'] || ''));
  const badPre = await request('/relay', { method: 'OPTIONS', headers: { origin: 'https://evil.test' } });
  check('cors-preflight-bad-origin', badPre.status === 403 && !badPre.headers['access-control-allow-origin']);

  // ---- v2: multi-project (spec v2 §8) ----
  // T-v2-2: random clientIds never trigger D1 queries (whole-table snapshot, V4)
  {
    const loads0 = registryLoads();
    const t = mk(freshSeed());
    const rand = [];
    for (let i = 0; i < 50; i++) rand.push((await relay(t, { token: await tokenFor(t.user_hex, { aud: `random-${i}` }) })).status);
    for (let i = 0; i < 20; i++) rand.push((await request(`/relayer?project=random-${i}`, { method: 'GET' })).status);
    check('T-v2-2 unknown aud -> 401, unknown project -> 404, no extra D1 loads',
      rand.slice(0, 50).every((x) => x === 401) && rand.slice(50).every((x) => x === 404) && registryLoads() === loads0,
      `loads ${loads0}->${registryLoads()} statuses=${[...new Set(rand)]}`);
    const arr = await relay(t, { token: await tokenFor(t.user_hex, { aud_array: '1' }) });
    check('T-v2-2 aud array -> 401', arr.status === 401 && arr.json?.code === 'unauthorized', `${arr.status}`);
  }
  // T-v2-12: GET /relayer for unknown / paused / mainnet-while-disabled / missing param -> 404
  {
    const codes = [];
    for (const q of ['?project=nope', '?project=client-p', '?project=client-m', '']) codes.push((await request(`/relayer${q}`, { method: 'GET' })).status);
    const b = await request('/relayer?project=client-b', { method: 'GET', headers: { origin: ORIGIN_B } });
    check('T-v2-12 relayer info 404s', codes.every((c) => c === 404), `${codes}`);
    check('T-v2-12 project B info + CORS echo', b.status === 200 && b.json?.pubkey === PK_B && b.headers['access-control-allow-origin'] === ORIGIN_B);
  }
  // T-v2-1: each project pays with its own fee payer
  {
    let x = await relayAsB(freshSeed());
    check('T-v2-1 project B relays with its own fee payer', x.status === 200 && x.headers['access-control-allow-origin'] === ORIGIN_B, `${x.status} ${x.json?.code}`);
    x = await relayAsB(freshSeed(), { feePayer: PK_A });
    check('T-v2-1 B token cannot spend A fee payer', x.status === 400 && x.json?.reason === 'fee payer is not the relayer', `${x.status} ${x.json?.reason}`);
  }
  // T-v2-3: Origin must belong to the token's project
  {
    let x = await relayAs(freshSeed(), { origin: ORIGIN_B });
    check('T-v2-3 A token from B origin -> 403', x.status === 403 && x.json?.code === 'origin', `${x.status}`);
    const preB = await request('/relay', { method: 'OPTIONS', headers: { origin: ORIGIN_B } });
    const preP = await request('/relay', { method: 'OPTIONS', headers: { origin: 'http://localhost:5176' } });
    check('T-v2-3 preflight: union of active origins only', preB.status === 204 && preB.headers['access-control-allow-origin'] === ORIGIN_B
      && preP.status === 403 && !preP.headers['access-control-allow-origin'], `${preB.status} ${preP.status}`);
  }
  // T-v2-2: paused project and mainnet behind the kill switch
  {
    let x = await relayAs(freshSeed(), { claims: { aud: 'client-p' } });
    check('T-v2-2 paused project -> 503 project_paused', x.status === 503 && x.json?.code === 'project_paused', `${x.status} ${x.json?.code}`);
    x = await relayAs(freshSeed(), { feePayer: PK_M, claims: { aud: 'client-m', iss: ISS_MAINNET } });
    check('T-v2-2 mainnet while disabled -> 503 cluster_disabled', x.status === 503 && x.json?.code === 'cluster_disabled', `${x.status} ${x.json?.code}`);
  }
  // T-v2-11: a broken project answers 500; the others keep working
  {
    const xs = await relayAs(freshSeed(), { feePayer: PK_X, claims: { aud: 'client-x' }, origin: 'http://localhost:5177' });
    const ys = await relayAs(freshSeed(), { feePayer: PK_Y, claims: { aud: 'client-y' }, origin: 'http://localhost:5178' });
    const ok = await relayAs(freshSeed());
    check('T-v2-11 missing secret / key mismatch -> 500 only for that project',
      xs.status === 500 && ys.status === 500 && xs.json?.code === 'internal' && ys.json?.reason === undefined && ok.status === 200,
      `x=${xs.status} y=${ys.status} a=${ok.status}`);
  }

  // T1 / T2 / T3
  const u1 = freshSeed();
  let r = await relayAs(u1);
  check('T1 happy path', r.status === 200 && typeof r.json?.signature === 'string' && r.headers['access-control-allow-origin'] === ORIGIN, `${r.status}`);
  r = await relayAs(u1);
  check('T2 cooldown', r.status === 429 && r.json?.code === 'cooldown' && Number(r.headers['retry-after']) >= 1 && r.json?.retry_after_ms > 0, `${r.status} ${r.json?.code}`);
  await sleep(2100);
  r = await relayAs(u1);
  check('T3 after cooldown', r.status === 200, `${r.status}`);

  // T4 / T5: concurrent requests while the first is waiting on a slow RPC
  await setState({ delayMs: 800 });
  const u2 = freshSeed();
  const before = (await stats()).sendTransaction;
  const txs = await Promise.all([...Array(5)].map(async () => { const t = mk(u2); return { t, token: await tokenFor(t.user_hex) }; }));
  const rs = await Promise.all(txs.map(({ t, token }) => relay(t, { token })));
  const codes = rs.map((x) => x.status).sort();
  const t4Sends = (await stats()).sendTransaction - before;
  check('T4 concurrent same wallet', codes.filter((c) => c === 200).length === 1 && codes.every((c) => [200, 409, 429].includes(c)) && t4Sends === 1, `${codes} sends=${t4Sends}`);
  check('T5 in-flight -> 409', rs.some((x) => x.status === 409 && x.json?.code === 'in_progress'));
  await setState({ delayMs: 0 });

  // T6: RPC reject -> short cooldown
  const u3 = freshSeed();
  await setState({ send: 'reject' });
  r = await relayAs(u3);
  check('T6 rpc_rejected', r.status === 502 && r.json?.code === 'rpc_rejected', `${r.status}`);
  await setState({ send: 'ok' });
  r = await relayAs(u3);
  check('T6 short cooldown applies', r.status === 429 && r.json?.code === 'cooldown');
  await sleep(1100);
  r = await relayAs(u3);
  check('T6 retry after short cooldown', r.status === 200, `${r.status}`);

  // T25: one wallet looping failing txs is rate-bounded by REJECT_COOLDOWN_MS (1 s here)
  const u4 = freshSeed();
  await setState({ send: 'reject' });
  const b25 = (await stats()).sendTransaction;
  const end = Date.now() + 3000;
  while (Date.now() < end) { await relayAs(u4); }
  const loopSends = (await stats()).sendTransaction - b25;
  check('T25 reject loop bounded', loopSends <= 4, `sends=${loopSends} in 3s`);
  await setState({ send: 'ok' });

  // T7 / T8 / T9 (policy + signature) — shape cases are unit-tested; spot-check via HTTP
  r = await relayAs(freshSeed(), { mode: 'foreign_payer' });
  check('T8 foreign fee payer', r.status === 400 && r.json?.code === 'validation', `${r.json?.reason}`);
  r = await relayAs(freshSeed(), { mode: 'tamper' });
  check('T9 bad user signature', r.status === 400, `${r.json?.reason}`);
  r = await relayAs(freshSeed(), { mode: 'v0' });
  check('T7 v0 without ALT accepted', r.status === 200, `${r.status}`);
  r = await relay({ tx: 'not-base64!!' }, { token: 'x' });
  check('T7 garbage tx', r.status === 400);

  // T10: identity
  const t10 = mk(freshSeed());
  r = await relay(t10, {});
  check('T10 missing token', r.status === 401 && r.json?.code === 'unauthorized');
  {
    // Auth scheme is case-insensitive.
    const tl = mk(freshSeed());
    const lower = await request('/relay', { headers: { 'content-type': 'application/json', origin: ORIGIN,
      authorization: `bearer ${await tokenFor(tl.user_hex)}` }, body: JSON.stringify({ transaction: tl.tx }) });
    check('T10 lowercase bearer scheme accepted', lower.status === 200, `${lower.status} ${lower.json?.code}`);
  }
  r = await relay(t10, { token: await tokenFor(t10.user_hex, { exp: String(Math.floor(Date.now() / 1000) - 120) }) });
  check('T10 expired token', r.status === 401, r.json?.reason);
  r = await relay(t10, { token: await tokenFor(t10.user_hex, { aud: 'other-client' }) });
  check('T10 wrong aud', r.status === 401, r.json?.reason);
  r = await relay(t10, { token: await tokenFor(t10.user_hex, { iss: 'https://evil.test' }) });
  check('T10 wrong iss', r.status === 401, r.json?.reason);
  r = await relay(t10, { token: await tokenFor(mk(freshSeed()).user_hex) });
  check('T10 token for another wallet', r.status === 401, r.json?.reason);
  r = await relay(t10, { token: await tokenFor(t10.user_hex, { curve: 'secp256k1' }) });
  check('T10 non-ed25519 wallet claim', r.status === 401);
  const tok = await tokenFor(t10.user_hex);
  r = await relay(t10, { token: tok.slice(0, -4) + (tok.endsWith('AAAA') ? 'BBBB' : 'AAAA') });
  check('T10 forged signature', r.status === 401);

  // T23: random kid flood -> 401 and at most one forced JWKS refetch
  const j0 = (await stats()).jwks;
  const flood = await Promise.all([...Array(10)].map(async (_, i) => relay(t10, { token: await tokenFor(t10.user_hex, { kid: `rand-${i}` }) })));
  const jwksFetches = (await stats()).jwks - j0;
  check('T23 unknown kid flood', flood.every((x) => x.status === 401) && jwksFetches <= 1, `jwks fetches=${jwksFetches}`);

  // T11: HTTP hardening
  const big = JSON.stringify({ transaction: 'A'.repeat(5000) });
  r = await relay(t10, { body: big });
  check('T10 missing token fails fast (before body read)', r.status === 401, `${r.status}`);
  r = await new Promise((resolve) => {
    const u = new URL(RELAYER + '/relay');
    const req = http.request({ hostname: u.hostname, port: u.port, path: '/relay', method: 'POST', agent: false,
      headers: { 'content-type': 'application/json', origin: ORIGIN, 'transfer-encoding': 'chunked' } }, (res) => {
      let d = ''; res.on('data', (c) => (d += c)); res.on('end', () => resolve({ status: res.statusCode }));
    });
    for (let i = 0; i < 6; i++) req.write('A'.repeat(1000));
    req.end();
  });
  check('T10 missing token, multi-chunk upload -> 401', r.status === 401, `${r.status}`);
  r = await request('/relay', { headers: { 'content-type': 'application/json', origin: ORIGIN,
    authorization: `Bearer ${tok}`, 'content-length': String(Buffer.byteLength(big)) }, body: big });
  check('T11 body too large (Content-Length)', r.status === 413, `${r.status}`);
  r = await new Promise((resolve) => {
    const u = new URL(RELAYER + '/relay');
    const req = http.request({ hostname: u.hostname, port: u.port, path: '/relay', method: 'POST',
      agent: false,
      headers: { 'content-type': 'application/json', origin: ORIGIN, authorization: `Bearer ${tok}`, 'transfer-encoding': 'chunked' } }, (res) => {
      let d = ''; res.on('data', (c) => (d += c)); res.on('end', () => resolve({ status: res.statusCode }));
    });
    for (let i = 0; i < 6; i++) req.write('A'.repeat(1000));
    req.end();
  });
  check('T11 body too large (chunked)', r.status === 413, `${r.status}`);
  check('T11 wrong method', (await request('/relay', { method: 'GET', headers: { origin: ORIGIN } })).status === 405);
  check('T11 wrong content-type', (await relay(t10, { token: tok, contentType: 'text/plain' })).status === 415);
  check('T11 wrong origin', (await relay(t10, { token: tok, origin: 'https://evil.test' })).status === 403);
  check('T11 missing origin', (await relay(t10, { token: tok, origin: null })).status === 403);

  // T27: consecutive rejects back off exponentially (base 1 s here); success resets the streak
  const u9 = freshSeed();
  await setState({ send: 'reject' });
  r = await relayAs(u9);
  const b1 = r.json?.retry_after_ms;
  await sleep(b1 + 100);
  r = await relayAs(u9);
  const b2 = r.json?.retry_after_ms;
  check('T27 reject backoff doubles', r.status === 502 && b1 >= 900 && b1 <= 1000 && b2 >= 1900 && b2 <= 2000, `b1=${b1} b2=${b2}`);
  r = await relayAs(u9);
  check('T27 backoff enforced', r.status === 429 && r.json?.retry_after_ms > 1500, `${r.status} ${r.json?.retry_after_ms}`);
  await sleep(2100);
  await setState({ send: 'ok' });
  r = await relayAs(u9);
  check('T27 success after backoff', r.status === 200, `${r.status}`);
  await sleep(2100);
  await setState({ send: 'reject' });
  r = await relayAs(u9);
  check('T27 streak reset after success', r.status === 502 && r.json?.retry_after_ms <= 1000, `${r.json?.retry_after_ms}`);
  await setState({ send: 'ok' });

  // T17: unknown outcome counts the full cooldown and returns the signature
  const u6 = freshSeed();
  await setState({ send: 'http503' });
  r = await relayAs(u6);
  check('T17 rpc_unknown (5xx)', r.status === 504 && r.json?.code === 'rpc_unknown' && typeof r.json?.signature === 'string', `${r.status}`);
  await setState({ send: 'ok' });
  r = await relayAs(u6);
  check('T17 unknown counts cooldown', r.status === 429 && r.json?.retry_after_ms > 1100, `${r.json?.retry_after_ms}`);

  // T22: RPC hang beyond RPC_TIMEOUT_MS -> 504, flag released
  const u7 = freshSeed();
  await setState({ send: 'hang' });
  const t0 = Date.now();
  r = await relayAs(u7);
  check('T22 rpc timeout', r.status === 504 && Date.now() - t0 < 6000, `${r.status} in ${Date.now() - t0}ms`);
  await setState({ send: 'ok' });
  r = await relayAs(u7);
  check('T22 lock released (429 not 409)', r.status === 429 && r.json?.code === 'cooldown', `${r.status} ${r.json?.code}`);

  // T24: RPC HTTP 429 -> rejected
  await setState({ send: 'http429' });
  const u8 = freshSeed();
  r = await relayAs(u8);
  const c1 = r.json?.retry_after_ms;
  check('T24 rpc http 429', r.status === 502 && r.json?.code === 'rpc_rejected');
  await sleep(c1 + 100);
  r = await relayAs(u8);
  check('T24 RPC overload does not grow user backoff', r.status === 502 && r.json?.retry_after_ms <= 1000, `c1=${c1} c2=${r.json?.retry_after_ms}`);
  // T28: node-side JSON-RPC error (-32005) is not the tx's fault -> base cooldown, no backoff
  const u10 = freshSeed();
  await setState({ send: 'unhealthy' });
  r = await relayAs(u10);
  const n1 = r.json?.retry_after_ms;
  await sleep(n1 + 100);
  r = await relayAs(u10);
  check('T28 node unhealthy does not grow user backoff', r.status === 502 && n1 <= 1000 && r.json?.retry_after_ms <= 1000, `n1=${n1} n2=${r.json?.retry_after_ms}`);
  await setState({ send: 'already' });
  r = await relayAs(freshSeed());
  check('already-processed -> 200 with signature', r.status === 200 && typeof r.json?.signature === 'string', `${r.status}`);
  await setState({ send: 'ok' });
  const ct = await relay(mk(freshSeed()), { token: tok, contentType: 'application/jsonx' });
  check('T11 exact media type', ct.status === 415, `${ct.status}`);

  // T15 + audit: D1 rows == RPC sends during this suite (4xx never written)
  await sleep(1500);
  const sends = (await stats()).sendTransaction - s0.sendTransaction;
  const counts = d1Counts();
  const rows = (counts.success || 0) + (counts.rpc_error || 0);
  check('T15 audit rows == RPC sends (no 4xx rows)', rows === sends, `rows=${JSON.stringify(counts)} sends=${sends}`);
  const replayRows = d1Query("SELECT count(*) AS c FROM relay_log WHERE status = 'success' AND reason = 'already processed'")[0].c;
  check('audit marks already-processed replays', replayRows === 1, `rows=${replayRows}`);
  const bRows = d1Query("SELECT aud, cluster, cost_lamports FROM relay_log WHERE aud = 'client-b'");
  check('V14 audit carries aud / cluster / cost', bRows.length === 1 && bRows[0].cluster === 'devnet' && bRows[0].cost_lamports === 10000, JSON.stringify(bRows));
}

async function suiteB() {
  let r;
  // project A/B rpm=3, CLUSTER_RPM_DEVNET=5, COOLDOWN_MS=5000
  // T4 (global part): 5 concurrent same-wallet requests consume ONE global slot,
  // so two other users still fit before the window is full.
  await setState({ delayMs: 800 });
  const w4 = freshSeed();
  const burst = await Promise.all([...Array(5)].map(async () => { const t = mk(w4); return relay(t, { token: await tokenFor(t.user_hex) }); }));
  await setState({ delayMs: 0 });
  const others = [];
  for (let i = 0; i < 2; i++) others.push((await relayAs(freshSeed())).status);
  r = await relayAs(freshSeed());
  check('T4 concurrent same wallet uses one project slot',
    burst.filter((x) => x.status === 200).length === 1 && burst.filter((x) => x.json?.code === 'in_progress').length === 4
      && others.every((s) => s === 200) && r.json?.code === 'project_throttle',
    `burst=${burst.map((x) => x.status)} others=${others} next=${r.status} ${r.json?.code}`);
  console.log('… waiting 61 s for the global window to roll over');
  await sleep(61_000);

  // T26 (R8): tx-fault rejects (JSON-RPC error, HTTP 400) return their global slot,
  // so 3 rejects + 3 successes fit in rpm=3
  const rej = [];
  for (const mode of ['reject', 'http400', 'reject']) {
    await setState({ send: mode });
    rej.push((await relayAs(freshSeed())).status);
  }
  await setState({ send: 'ok' });
  const ok = [];
  for (let i = 0; i < 3; i++) ok.push((await relayAs(freshSeed())).status);
  check('T26 rejected requests refund project quota', rej.every((s) => s === 502) && ok.every((s) => s === 200), `rej=${rej} ok=${ok}`);
  r = await relayAs(freshSeed());
  check('T26 quota still enforced for real sends', r.status === 429 && r.json?.code === 'project_throttle', `${r.status} ${r.json?.code}`);
  // Refunds reached the cluster too: it holds 3 (A's real sends), so B fits 2 more of 5.
  const bs = [];
  for (let i = 0; i < 3; i++) bs.push(await relayAsB(freshSeed()));
  check('T-v2-7 refund chain reaches the cluster; cluster cap across projects',
    bs[0].status === 200 && bs[1].status === 200 && bs[2].status === 429 && bs[2].json?.code === 'cluster_throttle' && Number(bs[2].headers['retry-after']) >= 1,
    `B=${bs.map((x) => `${x.status}:${x.json?.code ?? ''}`)}`);
  console.log('… waiting 61 s for the global window to roll over');
  await sleep(61_000);
  const a = freshSeed();
  r = await relayAs(a);
  check('T16 setup user A', r.status === 200);
  const spam = [];
  for (let i = 0; i < 5; i++) spam.push((await relayAs(a)).json?.code);
  check('T16 repeats hit wallet cooldown, not project quota', spam.every((c) => c === 'cooldown'), spam.join(','));
  r = await relayAs(freshSeed());
  check('T16 other user still served', r.status === 200, `${r.status} ${r.json?.code}`);
  r = await relayAs(freshSeed());
  check('T12 3rd admit ok', r.status === 200, `${r.status}`);
  r = await relayAs(freshSeed());
  check('T12 project throttle', r.status === 429 && r.json?.code === 'project_throttle' && Number(r.headers['retry-after']) >= 1 && r.json?.retry_after_ms > 0, `${r.status} ${r.json?.code}`);
  r = await relayAsB(freshSeed());
  check('T-v2 project A at capacity does not block project B', r.status === 200, `${r.status} ${r.json?.code}`);

  // T24/T28: RPC overload (HTTP 429, node-side -32005) does NOT refund: 3 such failures use up rpm=3
  console.log('… waiting 61 s for the global window to roll over');
  await sleep(61_000);
  const over = [];
  for (const mode of ['http429', 'unhealthy', 'http429']) {
    await setState({ send: mode });
    over.push((await relayAs(freshSeed())).status);
  }
  await setState({ send: 'ok' });
  r = await relayAs(freshSeed());
  check('T24/T28 overload rejects keep their project slot', over.every((s) => s === 502) && r.status === 429 && r.json?.code === 'project_throttle', `over=${over} next=${r.status} ${r.json?.code}`);
}

async function suiteC() {
  // REGISTRY_CACHE_MS=1000, REGISTRY_MAX_STALE_MS=4000.
  await setState({ genesis: 'NotDevnetGenesisHash1111111111111111111111111' });
  // Cold isolate, concurrent requests: each may fetch JWKS (no cross-request I/O sharing in
  // Workers), but none may fail with 500.
  const jc0 = (await stats()).jwks;
  const cold = await Promise.all([...Array(5)].map(() => relayAs(freshSeed())));
  const jwksFetches = (await stats()).jwks - jc0;
  check('JWKS cold start under concurrency', cold.every((x) => x.status === 503 && x.json?.code === 'upstream'), `statuses=${cold.map((x) => x.status)} jwks fetches=${jwksFetches}`);
  let r = await relayAs(freshSeed());
  check('T20 non-devnet RPC', r.status === 503 && r.json?.code === 'upstream' && r.json?.reason === undefined, JSON.stringify(r.json));
  await setState({ genesis: 'EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG', balances: { [PK_A]: 0 } });
  // Cold balance cache + concurrent wallets, with getBalance held open for 1.5 s so all five
  // admits overlap inside GlobalThrottle: single-flight must share ONE call across requests
  // in the Durable Object (no 500s).
  await setState({ balanceDelayMs: 1500 });
  const gbCold0 = (await stats()).getBalance;
  const coldTxs = await Promise.all([...Array(5)].map(async () => { const t = mk(freshSeed()); return { t, token: await tokenFor(t.user_hex) }; }));
  const lows = await Promise.all(coldTxs.map(({ t, token }) => relay(t, { token })));
  const gbCold = (await stats()).getBalance - gbCold0;
  await setState({ balanceDelayMs: 0 });
  check('T13 low balance (concurrent, single getBalance)', lows.every((x) => x.status === 503 && x.json?.code === 'low_balance') && gbCold === 1,
    `statuses=${lows.map((x) => `${x.status}:${x.json?.code}`)} getBalance=${gbCold}`);
  r = await relayAs(freshSeed());
  check('T13 low balance', r.status === 503 && r.json?.code === 'low_balance', `${r.status} ${r.json?.code}`);
  r = await relayAsB(freshSeed());
  check('T-v2-8 low balance of A does not affect B', r.status === 200, `${r.status} ${r.json?.code}`);
  console.log('… waiting 31 s for balance cache to expire');
  await sleep(31_000);
  await setState({ balanceMode: 'http500' });
  const u = freshSeed();
  r = await relayAs(u);
  check('T18 getBalance failure fail-closed', r.status === 503 && r.json?.code === 'upstream', `${r.status} ${r.json?.reason}`);
  // Failure is cached ~5 s: an immediate retry must not hit getBalance again (R6).
  const gb0 = (await stats()).getBalance;
  r = await relayAs(u);
  const gbCalls = (await stats()).getBalance - gb0;
  check('T18 getBalance failure cached (no RPC hammering)', r.status === 503 && r.json?.code === 'upstream' && gbCalls === 0, `${r.status} calls=${gbCalls}`);
  await setState({ balanceMode: 'ok', balances: {} });
  await sleep(5_500);
  r = await relayAs(u);
  check('T18 lock released after upstream failure', r.status === 200, `${r.status} ${r.json?.code}`);
  // T14: the audit table is gone -> the relay still succeeds (best-effort audit)
  d1Exec('ALTER TABLE relay_log RENAME TO relay_log_off');
  r = await relayAs(freshSeed());
  check('T14 audit failure still relays', r.status === 200, `${r.status}`);
  d1Exec('ALTER TABLE relay_log_off RENAME TO relay_log');
  // Unknown kid while the JWKS endpoint is down: still 401, never 503.
  await setState({ jwksMode: 'http500' });
  const tu = mk(freshSeed());
  r = await relay(tu, { token: await tokenFor(tu.user_hex, { kid: 'rotated-kid' }) });
  check('T10 unknown kid + JWKS refetch failure -> 401', r.status === 401 && r.json?.code === 'unauthorized', `${r.status} ${r.json?.reason}`);
  await setState({ jwksMode: 'ok' });

  // ---- v2: registry changes at runtime (snapshot ≤ REGISTRY_CACHE_MS old) ----
  const mainnetRelay = () => relayAs(freshSeed(), { feePayer: PK_M, claims: { aud: 'client-m', iss: ISS_MAINNET }, origin: ORIGIN_M });
  r = await relayAs(freshSeed(), { feePayer: PK_M, claims: { aud: 'client-m', iss: ISS_MAINNET } });
  check('T-v2-2 mainnet disabled', r.status === 503 && r.json?.code === 'cluster_disabled', `${r.status} ${r.json?.code}`);
  d1Exec("UPDATE settings SET value = 'true' WHERE key = 'mainnet_enabled'");
  await sleep(1500);
  await relayAs(freshSeed()); // first request after expiry refreshes the snapshot
  const m0 = (await stats()).sendMainnet;
  r = await mainnetRelay();
  check('V5 kill switch on (no deploy) -> mainnet project relays to the mainnet RPC',
    r.status === 200 && (await stats()).sendMainnet === m0 + 1, `${r.status} ${r.json?.code}`);
  // Retire project B: takes effect within the snapshot age
  d1Exec("UPDATE projects SET status = 'retired' WHERE aud = 'client-b'");
  await sleep(1500);
  await relayAs(freshSeed());
  r = await relayAsB(freshSeed(), { origin: ORIGIN });
  check('V13 retired project -> 503 project_paused', r.status === 503 && r.json?.code === 'project_paused', `${r.status} ${r.json?.code}`);
  // D1 outage: stale snapshot is served, but mainnet fails closed after REGISTRY_MAX_STALE_MS
  d1Exec('ALTER TABLE projects RENAME TO projects_off');
  await sleep(1500);
  r = await mainnetRelay();
  check('V4 D1 down: stale snapshot still serves (within max stale)', r.status === 200, `${r.status} ${r.json?.code}`);
  await sleep(4500);
  r = await mainnetRelay();
  const dev = await relayAs(freshSeed());
  check('V4 D1 down past max stale: mainnet fails closed, devnet continues',
    r.status === 503 && r.json?.code === 'upstream' && dev.status === 200, `mainnet=${r.status} ${r.json?.code} devnet=${dev.status}`);
  d1Exec('ALTER TABLE projects_off RENAME TO projects');
}

async function suiteD() {
  // user_daily_quota(A)=2 · project budget(A)=30000 (3 tx) · cluster budget=60000 (6 tx) · cooldown 300 ms
  let r;
  const pause = () => sleep(350);
  // T-v2-7: a tx-fault reject gives back the user's count, the project and the cluster budget
  const u0 = freshSeed();
  await setState({ send: 'reject' });
  r = await relayAs(u0);
  await setState({ send: 'ok' });
  const q0 = await quotaOf(u0);
  check('T-v2-7 reject refunds the user count', r.status === 502 && q0.status === 200 && q0.json?.used === 0, `${r.status} used=${q0.json?.used}`);

  // T-v2-4: user quota
  const u1 = freshSeed();
  const a1 = await relayAs(u1); await pause();
  const a2 = await relayAs(u1); await pause();
  const a3 = await relayAs(u1);
  check('T-v2-4 quota counted in /relay response', a1.status === 200 && a1.json?.quota?.used === 1 && a1.json?.quota?.limit === 2
    && a2.status === 200 && a2.json?.quota?.used === 2, `${a1.status}:${JSON.stringify(a1.json?.quota)} ${a2.status}:${JSON.stringify(a2.json?.quota)}`);
  const left = nextUtcMidnight() - Date.now();
  check('T-v2-4 3rd tx -> 429 user_quota until UTC midnight', a3.status === 429 && a3.json?.code === 'user_quota'
    && Math.abs(a3.json?.retry_after_ms - left) < 5000 && Number(a3.headers['retry-after']) >= 1, `${a3.status} ${a3.json?.code} ${a3.json?.retry_after_ms} vs ${left}`);
  const q1 = await quotaOf(u1);
  check('T-v2-4 GET /quota', q1.status === 200 && q1.json?.used === 2 && q1.json?.limit === 2 && q1.json?.resets_at === nextUtcMidnight()
    && q1.headers['access-control-allow-origin'] === ORIGIN, JSON.stringify(q1.json));
  r = await relayAsB(u1);
  check('T-v2-4 same wallet in another project has its own quota', r.status === 200 && r.json?.quota?.used === 1, `${r.status} ${JSON.stringify(r.json?.quota)}`);
  const qb = await quotaOf(u1, { aud: 'client-b', origin: ORIGIN_B });
  check('T-v2-4 GET /quota is per project', qb.json?.used === 1 && qb.json?.limit === 100, JSON.stringify(qb.json));

  // GET /quota auth
  const noTok = await request(`/quota?wallet=${mk(u1).user}`, { method: 'GET', headers: { origin: ORIGIN } });
  const other = await quotaOf(u1, { token: await tokenFor(mk(freshSeed()).user_hex) });
  const xorigin = await quotaOf(u1, { origin: ORIGIN_B });
  check('GET /quota requires a token bound to the wallet and the project origin',
    noTok.status === 401 && other.status === 401 && xorigin.status === 403, `${noTok.status} ${other.status} ${xorigin.status}`);

  // T-v2-5: project budget (A has used 2 of 3 — the reject was refunded)
  r = await relayAs(freshSeed());
  check('T-v2-5 3rd tx within project budget', r.status === 200, `${r.status} ${r.json?.code}`);
  const u3 = freshSeed();
  r = await relayAs(u3);
  check('T-v2-5 project budget exhausted -> 503 until UTC midnight', r.status === 503 && r.json?.code === 'project_budget'
    && Math.abs(r.json?.retry_after_ms - (nextUtcMidnight() - Date.now())) < 5000, `${r.status} ${r.json?.code}`);
  const q3 = await quotaOf(u3);
  check('T-v2-5 budget refusal gives back the user count', q3.json?.used === 0, JSON.stringify(q3.json));

  // T-v2-6: cluster budget = 6 tx: A 3 + B 1 so far
  const bs = [];
  for (let i = 0; i < 3; i++) bs.push(await relayAsB(freshSeed()));
  check('T-v2-6 cluster budget across projects', bs[0].status === 200 && bs[1].status === 200
    && bs[2].status === 503 && bs[2].json?.code === 'cluster_budget', bs.map((x) => `${x.status}:${x.json?.code ?? ''}`).join(','));
  // B's budget is 4 tx and B has 3 counted: if the cluster refusal above had leaked B's slot,
  // this request would stop at project_budget instead of reaching the cluster again.
  r = await relayAsB(freshSeed());
  check('T-v2-6 cluster refusal does not leak project B budget', r.status === 503 && r.json?.code === 'cluster_budget', `${r.status} ${r.json?.code}`);
  // Persisted for D2
  fs.writeFileSync(`${PERSIST}/d-seeds.json`, JSON.stringify({ u1 }));
}

async function suiteD2() {
  // Same state as D, after a full wrangler dev restart (T-v2-9).
  const { u1 } = JSON.parse(fs.readFileSync(`${PERSIST}/d-seeds.json`, 'utf8'));
  let r = await relayAs(u1);
  check('T-v2-9 user quota survives restart', r.status === 429 && r.json?.code === 'user_quota', `${r.status} ${r.json?.code}`);
  const q = await quotaOf(u1);
  check('T-v2-9 GET /quota after restart', q.json?.used === 2, JSON.stringify(q.json));
  r = await relayAs(freshSeed());
  check('T-v2-9 project budget survives restart', r.status === 503 && r.json?.code === 'project_budget', `${r.status} ${r.json?.code}`);
  r = await relayAsB(freshSeed());
  check('T-v2-9 cluster budget survives restart', r.status === 503 && r.json?.code === 'cluster_budget', `${r.status} ${r.json?.code}`);
}

async function suiteE() {
  // T-v2-10: projects C and D (budget 3 tx each), 5 distinct wallets each, all at once, with
  // getBalance held open so admits overlap inside each ProjectThrottle.
  await setState({ balanceDelayMs: 1500 });
  const fire = (fp, aud, origin) => Promise.all([...Array(5)].map(async () => {
    const t = mk(freshSeed(), '', fp);
    return relay(t, { token: await tokenFor(t.user_hex, { aud }), origin });
  }));
  const [c, d] = await Promise.all([fire(PK_C, 'client-c', 'http://localhost:5179'), fire(PK_D, 'client-d', 'http://localhost:5180')]);
  await setState({ balanceDelayMs: 0 });
  const tally = (rs) => ({ ok: rs.filter((x) => x.status === 200).length, budget: rs.filter((x) => x.json?.code === 'project_budget').length });
  const tc = tally(c), td = tally(d);
  check('T-v2-10 concurrent admits never exceed the project budget', tc.ok === 3 && tc.budget === 2 && td.ok === 3 && td.budget === 2,
    `C=${JSON.stringify(tc)} D=${JSON.stringify(td)} statuses=${[...c, ...d].map((x) => `${x.status}:${x.json?.code ?? ''}`)}`);
}

async function suiteF() {
  // Program actions, v3 Phase 1: the policy engine runs, nothing is relayed yet.
  const D = { create: '181ec828051c0777', close: 'a75149fabad23e79', noop: '09b20d738123ed66' };
  const prog = (args, seed = freshSeed()) => JSON.parse(execFileSync('./target/debug/examples/progtx', [PK_G, BLOCKHASH, seed, ...args]).toString());
  const sendG = async (t, opts = {}) => relay(t, { token: await tokenFor(t.user_hex, { aud: opts.aud ?? 'client-g' }), origin: opts.origin ?? ORIGIN_G });
  const createIx = `p:${TESTKIT}:${D.create}:u:s,fp:w,new:w,${SYSTEM}:r`;
  const closeIx = `p:${TESTKIT}:${D.close}:u:s,new:w,fp:w`;
  const noopIx = `p:${TESTKIT}:${D.noop}:u:s`;
  const sends0 = (await stats()).sendTransaction;

  const info = await request('/relayer?project=client-g', { method: 'GET' });
  check('V3 relayer info lists the program action', info.status === 200
    && info.json?.policy?.actions?.[0]?.type === 'program' && info.json?.policy?.actions?.[0]?.program_id === TESTKIT
    && info.json?.policy?.actions?.[0]?.instructions?.join() === 'create,close_thing,noop', info.text);

  const gate = (r) => r.status === 503 && r.json?.code === 'program_actions_disabled';
  let r = await sendG(prog([createIx]));
  check('V3-2 payer slot passes policy, gated until Phase 2 (503)', gate(r), `${r.status} ${r.json?.code} ${r.json?.reason ?? ''}`);
  r = await sendG(prog([closeIx]));
  check('V3-12 refund slot passes policy, gated', gate(r), `${r.status} ${r.json?.code} ${r.json?.reason ?? ''}`);
  r = await sendG(prog([noopIx]));
  check('V3-10 crank (user as remaining readonly signer) passes policy, gated', gate(r), `${r.status} ${r.json?.code} ${r.json?.reason ?? ''}`);
  r = await sendG(prog(['cbl:300000', 'cbp:1000', noopIx]));
  check('W5 compute budget within ceilings passes, gated', gate(r), `${r.status} ${r.json?.code} ${r.json?.reason ?? ''}`);

  const bad = async (id, args, reason) => {
    const x = await sendG(prog(args));
    check(id, x.status === 400 && x.json?.code === 'validation' && x.json?.reason === reason, `${x.status} ${x.json?.reason}`);
  };
  await bad('V3-1 unregistered program', [`p:${PK_A}:${D.noop}:u:s`], 'program not allowed');
  await bad('V3-1 unknown instruction', [`p:${TESTKIT}:0102030405060708:u:s`], 'instruction not allowed');
  // A top-level CreateAccount needs the new account to sign (3 signers) -> W2 rejects it first.
  await bad('V3-1 Kora #720: top-level System CreateAccount attached', [noopIx, `p:${SYSTEM}:00000000:fp:w,new:ws`], 'exactly 2 signatures required');
  // Same attachment without an extra signer: a top-level System Transfer from the relayer to the user.
  await bad('V3-1 top-level System Transfer from the relayer attached', [noopIx, `p:${SYSTEM}:0200000040420f0000000000:fp:w,u:w`], 'program not allowed');
  await bad('W3 relayer outside its slot', [`p:${TESTKIT}:${D.noop}:fp:w`, noopIx], 'relayer not allowed in this instruction');
  await bad('V3-15 payer slot holding the user', [`p:${TESTKIT}:${D.create}:u:s,u:ws,new:w,${SYSTEM}:r`], 'slot must hold the relayer');
  await bad('W4b refund to someone else', [`p:${TESTKIT}:${D.close}:u:s,new:w,new:w`], 'slot must hold the relayer');
  await bad('payer + refund slot in one tx', [createIx, closeIx], 'payer and refund slots in one transaction');
  await bad('W5 CU limit above ceiling', ['cbl:500000', noopIx], 'compute unit limit above ceiling');
  await bad('W5 CU price without limit', ['cbp:10', noopIx], 'compute unit price needs a compute unit limit');
  await bad('W5 CU price above ceiling', ['cbl:1000', 'cbp:2000', noopIx], 'compute unit price above ceiling');
  await bad('max_instructions', [noopIx, noopIx, noopIx, noopIx, noopIx], 'too many instructions');

  // Action routing between projects (W15)
  const memoToG = mk(freshSeed(), '', PK_G);
  r = await relay(memoToG, { token: await tokenFor(memoToG.user_hex, { aud: 'client-g' }), origin: ORIGIN_G });
  check('W15 memo tx to a program-only project', r.status === 400 && r.json?.reason === 'memo action not enabled for this project', `${r.status} ${r.json?.reason}`);
  const toA = JSON.parse(execFileSync('./target/debug/examples/progtx', [PK_A, BLOCKHASH, freshSeed(), noopIx]).toString());
  r = await relay(toA, { token: await tokenFor(toA.user_hex), origin: ORIGIN });
  check('W15 program tx to a memo-only project', r.status === 400 && r.json?.reason === 'program actions not enabled for this project', `${r.status} ${r.json?.reason}`);
  r = await relayAs(freshSeed());
  check('memo path unchanged (v2 regression)', r.status === 200, `${r.status} ${r.json?.code}`);

  const sent = (await stats()).sendTransaction - sends0;
  check('Phase 1: no program tx reached sendTransaction', sent === 1, `sends=${sent} (1 = the memo regression)`);
}

const info = await request('/relayer?project=test-client', { method: 'GET' });
relayerPubkey = info.json?.pubkey;
if (!relayerPubkey) { console.error('relayer not reachable', info); process.exit(2); }
await { A: suiteA, B: suiteB, C: suiteC, D: suiteD, D2: suiteD2, E: suiteE, F: suiteF }[SUITE]();
const failed = results.filter((r) => !r.ok);
console.log(`\nsuite ${SUITE}: ${results.length - failed.length}/${results.length} passed`);
process.exit(failed.length ? 1 : 0);
