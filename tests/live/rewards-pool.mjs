// Live devnet: rewards-pool (HgApfw…) policy + ix shape through the real relayer (2026-10-10).
// No pool is settled on devnet yet, so every claim fails in simulation with a program error:
// that proves the policy accepted the shape and devnet ran the program. Nothing is sent (no SOL).
// Run: LIVE_POLICY=tests/live/rewards-pool-policy.json LIVE_SCRIPT=tests/live/rewards-pool.mjs ./tests/run-live.sh
import crypto from 'node:crypto';
import { execFileSync } from 'node:child_process';

const { RELAYER_URL, MOCK_URL, PK_LIVE } = process.env;
const RP = 'HgApfwjAg1L2FJL3fX7A4WX5yfhrVV8k9Ahii5Pn8HVW';
const TOKEN = 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA';
const ATA = 'ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL';
const SYSTEM = '11111111111111111111111111111111';
const D = { sol: '8b71b3bdbe1e84c3', token: '74ce1bbfa6130049', ata: 'a37264d84003848a' };
const results = [];
const check = (name, ok, detail = '') => { results.push(ok); console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${ok ? '' : ` — ${detail}`}`); };
const rpc = async (method, params) => (await (await fetch('https://api.devnet.solana.com', { method: 'POST',
  headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })).json()).result;
const key = () => execFileSync('./target/debug/examples/genkey').toString().trim().split(/\s+/)[1];
const seed = () => crypto.randomBytes(32).toString('hex');
async function prog(args, s = seed()) {
  const bh = (await rpc('getLatestBlockhash', [{ commitment: 'confirmed' }])).value.blockhash;
  return JSON.parse(execFileSync('./target/debug/examples/progtx', [PK_LIVE, bh, s, ...args],
    { env: { ...process.env, PROGTX_V0: '1' } }).toString());
}
async function relay(t) {
  const { token } = await (await fetch(`${MOCK_URL}/__token?` + new URLSearchParams({ user_hex: t.user_hex, aud: 'live-v3' }))).json();
  const r = await fetch(`${RELAYER_URL}/relay`, { method: 'POST',
    headers: { 'content-type': 'application/json', origin: 'http://localhost:5190', authorization: `Bearer ${token}` },
    body: JSON.stringify({ transaction: t.tx }) });
  const text = await r.text();
  let json = null; try { json = JSON.parse(text); } catch {}
  return { status: r.status, json, text, bytes: Buffer.from(t.tx, 'base64').length };
}
// Shape accepted + simulated on devnet = a program error from simulation (not a policy 400).
const simulated = (r) => r.status === 400 && r.json?.code === 'simulation_failed' && /InstructionError/.test(r.json?.reason ?? '');
const ix = (disc, accts) => `p:${RP}:${disc}00:${accts}`;
const balBefore = (await rpc('getBalance', [PK_LIVE, { commitment: 'confirmed' }])).value;

// Accounts as in IDL v1.0.0; the player is the `winner` (marked signer by the client).
let r = await relay(await prog([ix(D.sol, 'new:r,new:w,u:ws')]));
check('R1 claim_sol (winner = writable signer) passes policy, program runs on devnet', simulated(r), r.text.slice(0, 300));
r = await relay(await prog([ix(D.token, `new:r,new:w,new:r,new:w,u:s,new:w,${TOKEN}:r`)]));
check('R2 claim_token (winner = readonly signer at 4)', simulated(r), r.text.slice(0, 300));
r = await relay(await prog([ix(D.ata, `u:ws,new:r,new:w,new:r,new:w,u:s,new:w,${TOKEN}:r,${ATA}:r,${SYSTEM}:r`)]));
check('R3 claim_token_with_ata (player as payer 0 and winner 5: same key twice)', simulated(r), r.text.slice(0, 300));
console.log(`   R3 reason: ${r.json?.reason}`);

// Claim All: one mint (shared mint / vault / player ATA), a different match per claim.
const [mint, vault, ataW] = [key(), key(), key()];
const claim = () => ix(D.token, `new:r,new:w,${mint}:r,${vault}:w,u:s,${ataW}:w,${TOKEN}:r`);
const s7 = seed();
r = await relay(await prog(['cbl:120000', ...Array.from({ length: 7 }, claim)], s7));
console.log(`   Claim All 7 × claim_token + ComputeBudget: ${r.bytes} bytes`);
check('R4 Claim All (7 claims + ComputeBudget = 8 ix) passes policy', simulated(r) && r.bytes <= 1232, `${r.bytes} bytes · ${r.text.slice(0, 300)}`);
r = await relay(await prog(['cbl:120000', ...Array.from({ length: 8 }, claim)]));
check('R5 9 instructions → 400 too many instructions', r.status === 400 && r.json?.reason === 'too many instructions', r.text.slice(0, 200));
r = await relay(await prog([`p:${RP}:0102030405060708:u:s`]));
check('R6 unregistered ix (e.g. refund_boost_token_with_ata) → 400 instruction not allowed', r.status === 400 && r.json?.reason === 'instruction not allowed', r.text.slice(0, 200));

await new Promise((res) => setTimeout(res, 2000));
const balAfter = (await rpc('getBalance', [PK_LIVE, { commitment: 'confirmed' }])).value;
check('R7 nothing sent: fee payer balance unchanged', balAfter === balBefore, `${balBefore} → ${balAfter}`);
console.log(`\nlive (rewards-pool): ${results.filter(Boolean).length}/${results.length} passed`);
process.exit(results.every(Boolean) ? 0 : 1);
