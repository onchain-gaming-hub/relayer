// E2E against `wrangler dev` + mock-upstream. Usage: node tests/e2e.mjs <A|B|C>
// Test ids (T#) match spec §7. Policy-shape cases are covered by Rust unit tests (src/policy.rs).
import http from 'node:http';
import { execFileSync } from 'node:child_process';

const SUITE = process.argv[2];
const RELAYER = process.env.RELAYER_URL || 'http://127.0.0.1:8788';
const MOCK = process.env.MOCK_URL || 'http://127.0.0.1:8899';
const ORIGIN = 'http://localhost:5173';
const PERSIST = process.env.PERSIST_DIR;
const BLOCKHASH = '11111111111111111111111111111111';

const results = [];
const check = (id, ok, detail = '') => { results.push({ id, ok, detail }); console.log(`${ok ? 'PASS' : 'FAIL'} ${id} ${detail}`); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let seedN = 0;
const freshSeed = () => (++seedN + SUITE.charCodeAt(0) * 16).toString(16).padStart(2, '0').repeat(32);

function request(path, { method = 'POST', headers = {}, body, chunked = false } = {}) {
  return new Promise((resolve, reject) => {
    const u = new URL(RELAYER + path);
    // agent:false -> fresh socket per request, so an early response that leaves an unread body
    // on a keep-alive socket cannot poison the next test.
    const req = http.request({ hostname: u.hostname, port: u.port, path: u.pathname, method, headers, agent: false }, (res) => {
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
function mk(seed, mode = '') {
  const out = execFileSync('./target/debug/examples/mktx', [relayerPubkey, BLOCKHASH, seed ?? '-', mode]);
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
  const t = mk(seed, opts.mode);
  return relay(t, { token: opts.token ?? (await tokenFor(t.user_hex)), ...opts });
}

function d1Counts() {
  const out = execFileSync('npx', ['wrangler', 'd1', 'execute', 'gasless-relayer-audit', '--local', '--persist-to', PERSIST,
    '--command', 'SELECT status, count(*) AS c FROM relay_log GROUP BY status', '--json'], { stdio: ['ignore', 'pipe', 'ignore'] });
  const rows = JSON.parse(out.toString())[0].results;
  return Object.fromEntries(rows.map((r) => [r.status, r.c]));
}

async function suiteA() {
  const info = await request('/relayer', { method: 'GET' });
  check('relayer-info', info.status === 200 && info.json?.pubkey === relayerPubkey && info.json?.allowlist?.[0] === 'MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr');
  check('relayer-info-bad-origin', (await request('/relayer', { method: 'GET', headers: { origin: 'https://evil.test' } })).status === 403);
  const pre = await request('/relay', { method: 'OPTIONS', headers: { origin: ORIGIN } });
  check('cors-preflight', pre.status === 204 && pre.headers['access-control-allow-origin'] === ORIGIN && /authorization/i.test(pre.headers['access-control-allow-headers'] || ''));
  check('cors-preflight-bad-origin', (await request('/relay', { method: 'OPTIONS', headers: { origin: 'https://evil.test' } })).status === 403);

  const s0 = await stats();

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
}

async function suiteB() {
  let r;
  // GLOBAL_RPM=3, COOLDOWN_MS=5000
  // T26 (R8): rejected requests return their global slot, so 3 rejects + 3 successes fit in rpm=3
  await setState({ send: 'reject' });
  const rej = [];
  for (let i = 0; i < 3; i++) rej.push((await relayAs(freshSeed())).status);
  await setState({ send: 'ok' });
  const ok = [];
  for (let i = 0; i < 3; i++) ok.push((await relayAs(freshSeed())).status);
  check('T26 rejected requests refund global quota', rej.every((s) => s === 502) && ok.every((s) => s === 200), `rej=${rej} ok=${ok}`);
  r = await relayAs(freshSeed());
  check('T26 quota still enforced for real sends', r.status === 429 && r.json?.code === 'global_throttle', `${r.status} ${r.json?.code}`);
  console.log('… waiting 61 s for the global window to roll over');
  await sleep(61_000);
  const a = freshSeed();
  r = await relayAs(a);
  check('T16 setup user A', r.status === 200);
  const spam = [];
  for (let i = 0; i < 5; i++) spam.push((await relayAs(a)).json?.code);
  check('T16 repeats hit wallet cooldown, not global', spam.every((c) => c === 'cooldown'), spam.join(','));
  r = await relayAs(freshSeed());
  check('T16 other user still served', r.status === 200, `${r.status} ${r.json?.code}`);
  r = await relayAs(freshSeed());
  check('T12 3rd admit ok', r.status === 200, `${r.status}`);
  r = await relayAs(freshSeed());
  check('T12 global throttle', r.status === 429 && r.json?.code === 'global_throttle' && Number(r.headers['retry-after']) >= 1, `${r.status} ${r.json?.code}`);
}

async function suiteC() {
  // Fresh state, no D1 schema applied (T14).
  await setState({ genesis: 'NotDevnetGenesisHash1111111111111111111111111' });
  let r = await relayAs(freshSeed());
  check('T20 non-devnet RPC', r.status === 503 && r.json?.code === 'upstream', r.json?.reason);
  await setState({ genesis: 'EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG', balance: 0 });
  r = await relayAs(freshSeed());
  check('T13 low balance', r.status === 503 && r.json?.code === 'low_balance', `${r.status} ${r.json?.code}`);
  console.log('… waiting 31 s for balance cache to expire');
  await sleep(31_000);
  await setState({ balanceMode: 'http500' });
  const u = freshSeed();
  r = await relayAs(u);
  check('T18 getBalance failure fail-closed', r.status === 503 && r.json?.code === 'upstream', `${r.status} ${r.json?.reason}`);
  await setState({ balanceMode: 'ok', balance: 10_000_000_000 });
  r = await relayAs(u);
  check('T18 lock released after upstream failure', r.status === 200, `${r.status} ${r.json?.code}`);
  check('T14 D1 unavailable still relays', r.status === 200);
  // Unknown kid while the JWKS endpoint is down: still 401, never 503.
  await setState({ jwksMode: 'http500' });
  const tu = mk(freshSeed());
  r = await relay(tu, { token: await tokenFor(tu.user_hex, { kid: 'rotated-kid' }) });
  check('T10 unknown kid + JWKS refetch failure -> 401', r.status === 401 && r.json?.code === 'unauthorized', `${r.status} ${r.json?.reason}`);
  await setState({ jwksMode: 'ok' });
}

const info = await request('/relayer', { method: 'GET' });
relayerPubkey = info.json?.pubkey;
if (!relayerPubkey) { console.error('relayer not reachable', info); process.exit(2); }
await { A: suiteA, B: suiteB, C: suiteC }[SUITE]();
const failed = results.filter((r) => !r.ok);
console.log(`\nsuite ${SUITE}: ${results.length - failed.length}/${results.length} passed`);
process.exit(failed.length ? 1 : 0);
