// Live devnet checks of relayer v3 (spec v3 §8 "Live devnet", Phase 5). Run via tests/run-live.sh.
// Reads chain state from the public devnet RPC (allowed from a local machine; only Workers are
// blocked); the relayer itself uses RPC_URL_DEVNET. Prints a JSON summary (CU, deltas) at the end.
import crypto from 'node:crypto';
import fs from 'node:fs';
import { execFileSync } from 'node:child_process';

const { RELAYER_URL, MOCK_URL, PK_LIVE, TESTKIT, PERSIST_DIR, RELAYER_LOG } = process.env;
const PUBLIC_RPC = 'https://api.devnet.solana.com';
const ORIGIN = 'http://localhost:5190';
const SYSTEM = '11111111111111111111111111111111';
const D = {
  create: '181ec828051c0777', close: 'a75149fabad23e79', noop: '09b20d738123ed66', fail: '2d8f8610d2bf0cd4',
  open: 'e4fbda56c3dd8a9f', drain: 'ef131e786ffcf54a',
};
const results = [];
const summary = { cu: {}, lamports: {}, sweep: {} };
const check = (name, ok, detail = '') => {
  results.push({ name, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${ok ? '' : ` — ${detail}`}`);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const u64 = (n) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b.toString('hex'); };
const i64 = (n) => { const b = Buffer.alloc(8); b.writeBigInt64LE(BigInt(n)); return b.toString('hex'); };

async function rpc(method, params) {
  for (let i = 0; i < 5; i++) {
    const r = await fetch(PUBLIC_RPC, { method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) });
    if (r.status === 429) { await sleep(1000 * (i + 1)); continue; }
    const j = await r.json();
    if (j.error) throw new Error(`${method}: ${JSON.stringify(j.error)}`);
    return j.result;
  }
  throw new Error(`${method}: rate limited`);
}
const blockhash = async () => (await rpc('getLatestBlockhash', [{ commitment: 'confirmed' }])).value.blockhash;
const account = async (pk) => (await rpc('getAccountInfo', [pk, { encoding: 'base64', commitment: 'confirmed' }])).value;
const rentOf = (size) => rpc('getMinimumBalanceForRentExemption', [size, { commitment: 'confirmed' }]);
async function confirmedTx(sig) {
  for (let i = 0; i < 40; i++) {
    const tx = await rpc('getTransaction', [sig, { encoding: 'json', commitment: 'confirmed', maxSupportedTransactionVersion: 0 }]);
    if (tx) return tx;
    await sleep(1500);
  }
  return null;
}
/** Fee payer lamport change and CU of a landed tx (from its own meta: exact, no RPC lag). */
const effect = (tx) => ({ delta: tx.meta.postBalances[0] - tx.meta.preBalances[0], cu: tx.meta.computeUnitsConsumed, err: tx.meta.err });

const seed = () => crypto.randomBytes(32).toString('hex');
async function prog(args, s = seed(), v0 = false) {
  const bh = await blockhash();
  return JSON.parse(execFileSync('./target/debug/examples/progtx', [PK_LIVE, bh, s, ...args],
    { env: { ...process.env, PROGTX_V0: v0 ? '1' : '' } }).toString());
}
async function relay(t) {
  const { token } = await (await fetch(`${MOCK_URL}/__token?` + new URLSearchParams({ user_hex: t.user_hex, aud: 'live-v3' }))).json();
  const r = await fetch(`${RELAYER_URL}/relay`, { method: 'POST',
    headers: { 'content-type': 'application/json', origin: ORIGIN, authorization: `Bearer ${token}` },
    body: JSON.stringify({ transaction: t.tx }) });
  const text = await r.text();
  let json = null;
  try { json = JSON.parse(text); } catch {}
  return { status: r.status, json, text };
}
function d1(sql) {
  const out = execFileSync('npx', ['wrangler', 'd1', 'execute', 'gasless-relayer-audit', '--local', '--persist-to', PERSIST_DIR,
    '--json', '--command', sql]).toString();
  return JSON.parse(out)[0].results;
}
const pda = (s) => `pda/${s}/${TESTKIT}`;
const fpBefore = (await rpc('getBalance', [PK_LIVE, { commitment: 'confirmed' }])).value;
summary.lamports.fee_payer_before = fpBefore;

// ---- crank: user rides as a remaining readonly signer (W12, spike 0.9 (a)) ----
let t = await prog([`p:${TESTKIT}:${D.noop}:u:s`]);
let r = await relay(t);
let tx = r.json?.signature ? await confirmedTx(r.json.signature) : null;
check('L1 crank noop relayed and confirmed (fee only)', r.status === 200 && tx && !tx.meta.err && effect(tx).delta === -10_000,
  `${r.status} ${r.text.slice(0, 200)} ${tx && JSON.stringify(effect(tx))}`);
if (tx) summary.cu.noop = effect(tx).cu;
// The frontend (@solana/kit) sends v0 messages without lookup tables.
t = await prog([`p:${TESTKIT}:${D.noop}:u:s`], seed(), true);
r = await relay(t);
tx = r.json?.signature ? await confirmedTx(r.json.signature) : null;
check('L1b crank as a v0 message (FE shape) relayed and confirmed', r.status === 200 && tx && !tx.meta.err && tx.version === 0,
  `${r.status} ${r.text.slice(0, 200)} version=${tx?.version}`);

// ---- payer slot: create (W4) then refund slot: close_thing (W4b) ----
const thingRent = await rentOf(8 + 32 + 32 + 1);
const sUser = seed();
t = await prog([`p:${TESTKIT}:${D.create}:u:s,fp:w,${pda('thing')}:w,${SYSTEM}:r`], sUser);
r = await relay(t);
tx = r.json?.signature ? await confirmedTx(r.json.signature) : null;
check('L2 create via payer slot: fee payer spends exactly rent + fee', r.status === 200 && tx && !tx.meta.err && effect(tx).delta === -(thingRent + 10_000),
  `${r.status} ${r.text.slice(0, 200)} rent=${thingRent} ${tx && JSON.stringify(effect(tx))}`);
if (tx) { summary.cu.create = effect(tx).cu; summary.lamports.create = effect(tx).delta; }
// The audit insert is best-effort (after the response): poll briefly.
let createRow;
for (let i = 0; i < 10 && !createRow; i++) {
  await sleep(500);
  createRow = d1(`SELECT cost_lamports AS c, rent_lamports AS r FROM relay_log WHERE tx_signature = '${r.json?.signature}'`)[0];
}
check('L2 audit: charged fee + declared rent (1_110_000), simulated rent = real rent', createRow?.c === 1_110_000 && createRow?.r === thingRent,
  JSON.stringify(createRow));

t = await prog([`p:${TESTKIT}:${D.close}:u:s,${pda('thing')}:w,fp:w`], sUser);
r = await relay(t);
tx = r.json?.signature ? await confirmedTx(r.json.signature) : null;
check('L3 close_thing via refund slot: rent comes back to the fee payer', r.status === 200 && tx && !tx.meta.err && effect(tx).delta === thingRent - 10_000,
  `${r.status} ${r.text.slice(0, 200)} ${tx && JSON.stringify(effect(tx))}`);
if (tx) summary.cu.close_thing = effect(tx).cu;

// ---- negative: drain is registered (payer slot) but its simulation moves lamports elsewhere ----
const balBeforeDrain = (await rpc('getBalance', [PK_LIVE, { commitment: 'confirmed' }])).value;
t = await prog([`p:${TESTKIT}:${D.drain}${u64(5_000_000)}:u:s,fp:w,new:w,${SYSTEM}:r`]);
r = await relay(t);
check('L4 drain → 400 inner_ix_rejected (simulated on devnet, never sent)', r.status === 400 && r.json?.code === 'inner_ix_rejected', r.text.slice(0, 300));
await sleep(3000);
const balAfterDrain = (await rpc('getBalance', [PK_LIVE, { commitment: 'confirmed' }])).value;
check('L4 fee payer balance unchanged by the drain attempt', balAfterDrain === balBeforeDrain, `${balBeforeDrain} → ${balAfterDrain}`);

// ---- retryable custom error from the real program (W11) ----
t = await prog([`p:${TESTKIT}:${D.fail}:u:s`]);
r = await relay(t);
check('L5 fail_retryable (Custom 6000 on devnet) → 409 retryable', r.status === 409 && r.json?.code === 'retryable' && /6000/.test(r.json?.reason ?? ''),
  r.text.slice(0, 300));

// ---- cron sweep (W13): one closable Room, one with a 1-hour retention ----
const roomRent = await rentOf(8 + 32 + 32 + 8 + 1);
const sNow = seed(), sLater = seed();
const rooms = {};
for (const [name, s, secs] of [['now', sNow, 0], ['later', sLater, 3600]]) {
  t = await prog([`p:${TESTKIT}:${D.open}${i64(secs)}:u:s,fp:w,${pda('room')}:w,${SYSTEM}:r`], s);
  r = await relay(t);
  tx = r.json?.signature ? await confirmedTx(r.json.signature) : null;
  check(`L6 open_room(${secs}) via payer slot: rent + fee`, r.status === 200 && tx && !tx.meta.err && effect(tx).delta === -(roomRent + 10_000),
    `${r.status} ${r.text.slice(0, 200)} ${tx && JSON.stringify(effect(tx))}`);
  if (tx) summary.cu.open_room = effect(tx).cu;
  // Keys: [fee payer (w,s), user (r,s), room (w), system, testkit] → the only writable non-signer.
  rooms[name] = tx?.transaction.message.accountKeys[2];
}
// Wait until the Rooms are visible to getProgramAccounts at `confirmed` on the relayer's RPC.
await sleep(5000);
const t0 = Date.now();
const sched = await fetch(`${RELAYER_URL}/__scheduled?cron=*%2F5+*+*+*+*`);
summary.sweep.scheduled_wall_ms = Date.now() - t0;
check('L7 /__scheduled ran', sched.status === 200, `${sched.status}`);
await sleep(1000);
const sweptRows = d1(`SELECT wallet_address AS w, status, tx_signature AS sig, rent_lamports AS r, cost_lamports AS c FROM relay_log WHERE status = 'sweep'`);
const nowRow = sweptRows.find((x) => x.w === rooms.now);
tx = nowRow ? await confirmedTx(nowRow.sig) : null;
check('L7 sweep closed the closable Room on devnet: rent back to the fee payer, one signature',
  !!nowRow && tx && !tx.meta.err && effect(tx).delta === roomRent - 5_000 && tx.transaction.signatures.length === 1 && nowRow.r === roomRent,
  `${JSON.stringify(sweptRows)} ${tx && JSON.stringify(effect(tx))}`);
if (tx) summary.cu.sweep_close = effect(tx).cu;
check('L7 the Room account is gone', (await account(rooms.now)) === null, rooms.now);
check('L7 the Room inside its retention was skipped (simulated, not sent) and still exists',
  !sweptRows.some((x) => x.w === rooms.later) && (await account(rooms.later)) !== null, rooms.later);
const report = (fs.readFileSync(RELAYER_LOG, 'utf8').match(/sweep live-v3: (\{.*\})/g) || []).pop();
summary.sweep.report = report;
// Rooms from earlier runs may be listed too: at least our two simulated, ≥ 1 sent, ≥ 1 skipped.
const rep = report ? JSON.parse(report.slice('sweep live-v3: '.length)).sweeps?.[0] : null;
check('L7 sweep report: the 1-hour Room was simulated and skipped (not just missing from the list)',
  rep && rep.found >= 2 && rep.simulated >= 2 && rep.sent >= 1 && rep.skipped >= 1, report ?? 'no report line');

summary.lamports.fee_payer_after = (await rpc('getBalance', [PK_LIVE, { commitment: 'confirmed' }])).value;
summary.lamports.spent_this_run = summary.lamports.fee_payer_before - summary.lamports.fee_payer_after;
summary.rooms = rooms;
console.log(JSON.stringify(summary, null, 2));
const failed = results.filter((x) => !x.ok);
console.log(`\nlive: ${results.length - failed.length}/${results.length} passed`);
process.exit(failed.length ? 1 : 0);
