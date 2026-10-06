// Mock Solana RPC + Web3Auth JWKS for local e2e (no deps). Port: MOCK_PORT (default 8899).
//   POST /            JSON-RPC: getGenesisHash, getBalance, sendTransaction (behavior from state)
//   GET  /jwks        JWKS for a generated ES256 key (kid "test-kid")
//   GET  /__token     ?user_hex=&exp=&aud=&iss=&kid=&curve= -> signed JWT
//   POST /__state     merge JSON into state · GET /__stats -> call counters
import http from 'node:http';
import crypto from 'node:crypto';

const DEVNET = 'EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG';
const { privateKey, publicKey } = crypto.generateKeyPairSync('ec', { namedCurve: 'P-256' });
const jwk = { ...publicKey.export({ format: 'jwk' }), kid: 'test-kid', use: 'sig', alg: 'ES256' };

const state = { genesis: DEVNET, balance: 10_000_000_000, balanceMode: 'ok', jwksMode: 'ok', send: 'ok', delayMs: 0 };
const stats = { sendTransaction: 0, getBalance: 0, getGenesisHash: 0, jwks: 0 };

const b64url = (b) => Buffer.from(b).toString('base64url');
function token({ user_hex, exp, aud = 'test-client', iss = 'https://issuer.test', kid = 'test-kid', curve = 'ed25519' }) {
  const header = b64url(JSON.stringify({ alg: 'ES256', kid, typ: 'JWT' }));
  const claims = {
    aud, iss, sub: 'test-user', iat: Math.floor(Date.now() / 1000),
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
    return json(res, 200, { keys: [jwk] });
  }
  if (req.method === 'GET' && url.pathname === '/__token') return json(res, 200, { token: token(Object.fromEntries(url.searchParams)) });
  if (req.method === 'GET' && url.pathname === '/__stats') return json(res, 200, { ...stats, state });
  if (req.method === 'POST' && url.pathname === '/__state') { Object.assign(state, JSON.parse(await readBody(req) || '{}')); return json(res, 200, state); }
  if (req.method !== 'POST' || url.pathname !== '/') return json(res, 404, {});

  const rpc = JSON.parse(await readBody(req));
  stats[rpc.method] = (stats[rpc.method] || 0) + 1;
  const ok = (result) => json(res, 200, { jsonrpc: '2.0', id: rpc.id, result });
  switch (rpc.method) {
    case 'getGenesisHash': return ok(state.genesis);
    case 'getBalance':
      if (state.balanceMode === 'http500') return json(res, 500, {});
      return ok({ context: { slot: 1 }, value: state.balance });
    case 'sendTransaction': {
      if (state.delayMs) await new Promise((r) => setTimeout(r, state.delayMs));
      switch (state.send) {
        case 'ok': return ok(crypto.randomBytes(32).toString('hex'));
        case 'reject': return json(res, 200, { jsonrpc: '2.0', id: rpc.id, error: { code: -32002, message: 'Transaction simulation failed: Blockhash not found' } });
        case 'already': return json(res, 200, { jsonrpc: '2.0', id: rpc.id, error: { code: -32002, message: 'Transaction simulation failed: This transaction has already been processed' } });
        case 'http429': return json(res, 429, { error: 'Too many requests' });
        case 'http503': return json(res, 503, {});
        case 'hang': return; // never respond
      }
    }
  }
  return json(res, 200, { jsonrpc: '2.0', id: rpc.id, error: { code: -32601, message: 'method not found' } });
}).listen(Number(process.env.MOCK_PORT || 8899), '127.0.0.1', () => console.log('mock-upstream ready'));
