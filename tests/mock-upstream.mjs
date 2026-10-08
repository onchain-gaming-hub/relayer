// Mock Solana RPC + Web3Auth JWKS for local e2e (no deps). Port: MOCK_PORT (default 8899).
//   POST / | /mainnet JSON-RPC (devnet | mainnet): getGenesisHash, getBalance, sendTransaction
//   GET  /jwks        JWKS for a generated ES256 key (kid "test-kid")
//   GET  /__token     ?user_hex=&exp=&aud=&aud_array=&iss=&kid=&curve= -> signed JWT
//   state.balances    { <pubkey>: lamports } overrides state.balance per fee payer
//   state.simulate    `result.value` for simulateTransaction (null = generic success; `accounts` filled from
//                     the request with owner state.simOwner when absent; the string "$REQ0" becomes the
//                     first requested address) · stats.lastSimulate = its params
//   stats.lastSendSkipPreflight   `skipPreflight` of the last sendTransaction
//   state.programs    { <programId>: { slot, authority? } } → getAccountInfo returns BPF Upgradeable layouts
//   POST /__state     merge JSON into state · GET /__stats -> call counters
import http from 'node:http';
import crypto from 'node:crypto';

const DEVNET = 'EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG';
const MAINNET = '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d';
const { privateKey, publicKey } = crypto.generateKeyPairSync('ec', { namedCurve: 'P-256' });
const jwk = { ...publicKey.export({ format: 'jwk' }), kid: 'test-kid', use: 'sig', alg: 'ES256' };
const rsaJwk = { ...crypto.generateKeyPairSync('rsa', { modulusLength: 2048 }).publicKey.export({ format: 'jwk' }), kid: 'rsa-kid', use: 'sig', alg: 'RS256' };

const state = { genesis: DEVNET, genesisMainnet: MAINNET, balances: {}, balance: 10_000_000_000, balanceMode: 'ok', balanceDelayMs: 0, jwksMode: 'ok', send: 'ok', delayMs: 0 };
const stats = { sendTransaction: 0, getBalance: 0, getGenesisHash: 0, jwks: 0, sendMainnet: 0, simulateTransaction: 0, getAccountInfo: 0, lastSimulate: null, lastSendSkipPreflight: null };
Object.assign(state, { simulate: null, simOwner: '11111111111111111111111111111111', programs: {} });

// Minimal base58 (pubkeys only) for the fake ProgramData addresses.
const B58 = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';
function b58encode(buf) {
  let n = BigInt('0x' + Buffer.from(buf).toString('hex')), out = '';
  while (n > 0n) { out = B58[Number(n % 58n)] + out; n /= 58n; }
  for (const b of buf) { if (b !== 0) break; out = '1' + out; }
  return out;
}
function b58decode(str) {
  let n = 0n;
  for (const c of str) n = n * 58n + BigInt(B58.indexOf(c));
  let hex = n.toString(16); if (hex.length % 2) hex = '0' + hex;
  const bytes = Buffer.from(hex, 'hex');
  const zeros = str.match(/^1*/)[0].length;
  return Buffer.concat([Buffer.alloc(zeros), bytes, Buffer.alloc(Math.max(0, 32 - zeros - bytes.length))]).subarray(0, 32);
}
const LOADER = 'BPFLoaderUpgradeab1e11111111111111111111111';
const programDataOf = (id) => b58encode(crypto.createHash('sha256').update('pd:' + id).digest());

const b64url = (b) => Buffer.from(b).toString('base64url');
function token({ user_hex, exp, aud = 'test-client', aud_array, iss = 'https://issuer.test', kid = 'test-kid', curve = 'ed25519' }) {
  const header = b64url(JSON.stringify({ alg: 'ES256', kid, typ: 'JWT' }));
  const claims = {
    aud: aud_array ? [aud] : aud, iss, sub: 'test-user', iat: Math.floor(Date.now() / 1000),
    exp: exp ? Number(exp) : Math.floor(Date.now() / 1000) + 3600,
    wallets: [{ public_key: user_hex, type: 'web3auth_app_key', curve }],
  };
  const payload = b64url(JSON.stringify(claims));
  const sig = crypto.sign('sha256', Buffer.from(`${header}.${payload}`), { key: privateKey, dsaEncoding: 'ieee-p1363' });
  return `${header}.${payload}.${b64url(sig)}`;
}

const readBody = (req) => new Promise((ok) => { let d = ''; req.on('data', (c) => (d += c)); req.on('end', () => ok(d)); });
const json = (res, status, obj) => { res.writeHead(status, { 'content-type': 'application/json' }); res.end(JSON.stringify(obj)); };

http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://x');
  if (req.method === 'GET' && url.pathname === '/jwks') {
    stats.jwks++;
    if (state.jwksMode === 'http500') return json(res, 500, {});
    // A foreign (RSA) key in the same set must be skipped, not break parsing.
    return json(res, 200, { keys: [rsaJwk, jwk] });
  }
  if (req.method === 'GET' && url.pathname === '/__token') return json(res, 200, { token: token(Object.fromEntries(url.searchParams)) });
  if (req.method === 'GET' && url.pathname === '/__stats') return json(res, 200, { ...stats, state });
  if (req.method === 'POST' && url.pathname === '/__state') { Object.assign(state, JSON.parse(await readBody(req) || '{}')); return json(res, 200, state); }
  if (req.method !== 'POST' || !['/', '/mainnet'].includes(url.pathname)) return json(res, 404, {});
  const mainnet = url.pathname === '/mainnet';

  const rpc = JSON.parse(await readBody(req));
  stats[rpc.method] = (stats[rpc.method] || 0) + 1;
  const ok = (result) => json(res, 200, { jsonrpc: '2.0', id: rpc.id, result });
  switch (rpc.method) {
    case 'getGenesisHash': return ok(mainnet ? state.genesisMainnet : state.genesis);
    case 'simulateTransaction': {
      const addresses = rpc.params?.[1]?.accounts?.addresses ?? [];
      stats.lastSimulate = rpc.params?.[1];
      const v = state.simulate ? JSON.parse(JSON.stringify(state.simulate).replaceAll('$REQ0', addresses[0] ?? '')) : {
        err: null, fee: 10000, preBalances: [10_000_000_000], postBalances: [9_999_990_000], innerInstructions: [] };
      if (!('accounts' in v)) v.accounts = addresses.map(() => ({ owner: state.simOwner, lamports: 1, data: ['', 'base64'] }));
      return ok({ context: { slot: 1 }, value: v });
    }
    case 'getAccountInfo': {
      const [addr, cfg] = rpc.params;
      const len = cfg?.dataSlice?.length;
      const slice = (b) => (len === undefined ? b : b.subarray(0, len)).toString('base64');
      for (const [id, p] of Object.entries(state.programs)) {
        if (addr === id) {
          return ok({ context: { slot: 1 }, value: { owner: LOADER, lamports: 1, executable: true,
            data: [slice(Buffer.concat([Buffer.from([2, 0, 0, 0]), b58decode(programDataOf(id))])), 'base64'] } });
        }
        if (addr === programDataOf(id)) {
          const slot = Buffer.alloc(8); slot.writeBigUInt64LE(BigInt(p.slot));
          const auth = p.authority ? Buffer.concat([Buffer.from([1]), b58decode(p.authority)]) : Buffer.from([0]);
          return ok({ context: { slot: 1 }, value: { owner: LOADER, lamports: 1, executable: false,
            data: [slice(Buffer.concat([Buffer.from([3, 0, 0, 0]), slot, auth, Buffer.alloc(64)])), 'base64'] } });
        }
      }
      return ok({ context: { slot: 1 }, value: null });
    }
    case 'getBalance':
      if (state.balanceDelayMs) await new Promise((r) => setTimeout(r, state.balanceDelayMs));
      if (state.balanceMode === 'http500') return json(res, 500, {});
      return ok({ context: { slot: 1 }, value: state.balances[rpc.params?.[0]] ?? state.balance });
    case 'sendTransaction': {
      if (mainnet) stats.sendMainnet++;
      stats.lastSendSkipPreflight = rpc.params?.[1]?.skipPreflight ?? null;
      if (state.delayMs) await new Promise((r) => setTimeout(r, state.delayMs));
      switch (state.send) {
        case 'ok': return ok(crypto.randomBytes(32).toString('hex'));
        case 'reject': return json(res, 200, { jsonrpc: '2.0', id: rpc.id, error: { code: -32002, message: 'Transaction simulation failed: Blockhash not found' } });
        case 'already': return json(res, 200, { jsonrpc: '2.0', id: rpc.id, error: { code: -32002, message: 'Transaction simulation failed: This transaction has already been processed' } });
        case 'http429': return json(res, 429, { error: 'Too many requests' });
        case 'http400': return json(res, 400, { error: 'Bad request' });
        case 'unhealthy': return json(res, 200, { jsonrpc: '2.0', id: rpc.id, error: { code: -32005, message: 'Node is unhealthy' } });
        case 'http503': return json(res, 503, {});
        case 'hang': return; // never respond
      }
    }
  }
  return json(res, 200, { jsonrpc: '2.0', id: rpc.id, error: { code: -32601, message: 'method not found' } });
}).listen(Number(process.env.MOCK_PORT || 8899), '127.0.0.1', () => console.log('mock-upstream ready'));
