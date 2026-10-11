---
name: gasless-relayer
description: Operator and onboarding guide for maintainer's gasless Solana relayer (Cloudflare Worker + Durable Objects + D1, devnet staging). Lists everything already deployed on devnet (Worker URL, D1, registered projects, fee payer wallets, test program ids, slots), the HTTP API, how to register a new app or program, how to test it (local e2e, live devnet), and how to operate it safely. Use when a team wants to use the relayer, when registering or changing a project/policy, or when you need an id/address/URL of the relayer's devnet deployment.
---

# Gasless Relayer — คู่มือผู้ใช้และผู้ดูแล

> ข้อมูลในไฟล์นี้ **ตรวจกับของจริงแบบ read-only เมื่อ 2026-10-11** (D1 remote, `wrangler deployments list`, devnet RPC)
> ค่าที่เปลี่ยนได้ (ยอดเงิน, version ของ Worker, policy) ให้ตรวจซ้ำก่อนใช้ ตามคำสั่งในหัวข้อ 2 แต่ละส่วน
> กฎการออกแบบ program และ FE (payer slot, crank, sweep, error ของ FE): **`relayer/skills/gasless-relayer-integration/SKILL.md`** · ไฟล์นี้ไม่ซ้ำเนื้อหา แค่ชี้ไป
> spec: `docs/plans/2026-10-08_program-actions-v3-spec.md` (v3) · v2: `docs/plans/2026-10-06_multi-project-relayer-v2-spec.md`
> ผู้ดูแล: maintainer · ผู้อนุมัติและรันงานบน remote: **maintainer**

## 1. relayer คืออะไร
- ผู้ใช้ที่ login ด้วย Web3Auth (social) มี wallet แต่ไม่มี SOL · FE สร้าง tx ที่ให้ **fee payer = กระเป๋าของ relayer** แล้วให้ผู้ใช้ sign → `POST /relay` พร้อม idToken
- relayer ตรวจ identity, policy ของโปรเจค, quota/budget แล้ว sign เป็น fee payer → (ถ้าต้อง) simulate แล้วตรวจว่าเงินของ relayer ไปไหน → ส่งเข้า chain
- **1 โปรเจค = 1 Web3Auth clientId (`aud`) = 1 กระเป๋า fee payer = 1 cluster** · โปรเจคบอกว่าให้ sponsor อะไรผ่าน `policy`:
  - **memo**: Memo v2 ที่มี prefix
  - **program**: ix ของ program ที่ลงทะเบียน ตาม discriminator, จำนวน account, payer slot / refund slot, sweeps
- **blockchain-games-hub ใช้โปรเจคเดียว (H-3, 2026-10-10):** ทุก program ของ hub ลงทะเบียนในโปรเจคของ hub-web · platform เติมเงินกระเป๋าเอง (H-5)

## 2. ของที่ deploy แล้ว (devnet เท่านั้น · mainnet ปิดอยู่)

### 2.1 Cloudflare (staging)
| อะไร | ค่า |
|---|---|
| Worker | `gasless-relayer` · **https://gasless-relayer.meaw-pipat.workers.dev** |
| Version ที่รันอยู่ | `53182635-e546-4a14-bef6-3c80f4a2f8b2` (upload 2026-10-09T02:21Z = relayer v3 Phases 1–5) · ⚠️ **ยังไม่ได้ deploy:** การแก้ audit `cost_lamports` (relayer `dadb272`) และเอกสารหลังจากนั้น · ตรวจ: `npx wrangler deployments list` |
| Cron | `*/5 * * * *` (sweep สำหรับโปรเจคที่ใส่ `sweeps` ใน policy) |
| Durable Objects | `UserController` (ต่อ `aud:wallet`) · `ProjectThrottle` (ต่อ `aud`) · `ClusterThrottle` (ต่อ cluster) · `SweepController` (ต่อ `aud`) · migration tag `v2` + `v3` |
| D1 | `gasless-relayer-audit` · id `e3bf7ce9-6c44-43a6-b6cb-398c04068d9f` · ตาราง `projects`, `fee_payer_history`, `settings`, `relay_log` · migration ล่าสุด `0003_v3_program_actions.sql` |
| Secrets (ชื่อเท่านั้น) | `FEE_PAYER_STG_POC` · `RPC_URL_DEVNET` (Alchemy devnet) · **ไม่มี `RPC_URL_MAINNET`** |
| `settings` | `mainnet_enabled = false` |
| Vars | ใน `relayer/wrangler.toml` [vars] (ดูครบที่ไฟล์นั้น): `WALLET_BINDING = wallets` (wallet ใน tx ต้องเป็น wallet ของ idToken นั้น) · cooldown 10 s, reject backoff 3 s → 60 s, cluster RPM 300, cluster fee budget 10 / 0.2 SOL ต่อวัน (devnet/mainnet), cluster rent budget 10 SOL ต่อวันทั้งสอง cluster, `RETRY_COOLDOWN_MS` 1000, registry cache 60 s (ใช้ snapshot เก่าได้ไม่เกิน `REGISTRY_MAX_STALE_MS` 600 s ถ้า D1 ล่ม) |
| Frontend (Pages) | **https://gasless-poc-frontend.pages.dev** · `?v3` = แผงทดสอบ program actions (testkit) |

### 2.2 โปรเจคที่ลงทะเบียนแล้ว (D1 remote)
| slug | `aud` (Web3Auth clientId · Sapphire Devnet) | origin | fee payer | ค่า |
|---|---|---|---|---|
| `STG_POC` | `BCBRRKiatPMMJBRPEdNaqI-fRZ0w3Z1C0J0Q51ypmz2vEYAkQLbCpRFA7L_cgK2ylavoMkdFIOWcKJCLg6JKns4` | `https://gasless-poc-frontend.pages.dev` | `hp1nnTcTLqrBczEGujPFhvtXzL2HJRxnBumeU216xyr` | active · quota 100/วัน · 60 RPM · fee budget 1 SOL/วัน · rent budget 0.05 SOL/วัน · min balance 0.05 SOL · cooldown = ค่าของ Worker |

policy ของ `STG_POC`:
- memo prefix `gasless-poc:` (128 bytes)
- program testkit (pin slot `509032443`): `create`, `close_thing`, `noop`, `fail_retryable`, `open_room`
- sweep `close_room`
- limits: `max_instructions` 4, `max_cu` 400,000, ไม่มี priority fee
- ตรวจ: `npx wrangler d1 execute gasless-relayer-audit --remote --command "SELECT slug, status, policy FROM projects"`

### 2.3 กระเป๋า (pubkey เท่านั้น · ห้ามใส่ private key ในเอกสาร, git หรือ command line)
| บทบาท | pubkey | key อยู่ที่ | ยอด 2026-10-11 |
|---|---|---|---|
| fee payer ของ `STG_POC` (staging) | `hp1nnTcTLqrBczEGujPFhvtXzL2HJRxnBumeU216xyr` | Worker secret `FEE_PAYER_STG_POC` + สำรองใน `relayer/.dev.vars` | 1.999865 SOL |
| fee payer ของ live test ในเครื่อง | `FSrvuAXPpjuSDjZxc24j2dsnFa1K4Au9s5vavqHcN989` | `relayer/.dev.vars` (`FEE_PAYER_LIVE_V3`) | 0.19663 SOL · มี `Room` ของ testkit ค้างอยู่ **3 ห้อง** (ห้องที่ตั้ง retention 1 ชม. จาก live test รอบก่อน · rent ประมาณ 1,061,720 lamports ต่อห้อง) ซึ่ง sweep ของการรัน `run-live.sh` รอบถัดไปจะปิดคืน (cron ของ staging ไม่แตะ เพราะค้นเฉพาะ account ที่ `rent_payer` = กระเป๋าของโปรเจคตัวเอง) |
| fee payer ของ local dev (`wrangler dev` + FE ในเครื่อง) | `7uA1QxYTnzXzNBoVaE4gbLvXzxtPLjzvWAQqvmcMU5Cc` | `relayer/.dev.vars` (`FEE_PAYER_POC`) | 1.99993 SOL |
| deployer / upgrade authority ของ testkit | `Ej77KMCVLEcDTrSjXHTLHTjMZUAr5F46PFrDw6MVVe7F` | `~/.config/solana/testkit-deployer-devnet.json` (นอก repo) | 1.00756 SOL |

- ทุกกระเป๋าเป็น **devnet เท่านั้น** · กระเป๋าใบหนึ่งผูกกับโปรเจคเดียวและ cluster เดียวตลอดอายุ (`fee_payer_history`)
- `relayer/.dev.vars` (gitignored, 0600) ยังมี key ของ v1 (`RELAYER_PRIVATE_KEY`, `RPC_URL`) ซึ่งไม่ได้ใช้แล้ว

### 2.4 Program บน devnet
| program | id | ProgramData / deploy slot | สถานะกับ relayer |
|---|---|---|---|
| **testkit** (ของ relayer · Anchor 1.2.1 · repo `solana-poc/testkit`) | `Bh4GY79ytnEZ4t3ZGf7qUVooaSKYyC8sM1uJpiM2xdCW` | `Bx4QouRTySJqMCPDhBStWY5EPzvkk5iFQrkvkU9Nq6YU` · slot **509032443** · authority `Ej77KM…` | ลงทะเบียนใน `STG_POC` (pin) |
| platform-core (ทีม platform-core) | `AJnPSYsn7J2muHfKztRtpZz5kBP94dWkNZLr9aMhu18G` | `4HDMqVpFQQLwrSXrzR4yEPpBTPk5ZZ864gGGsumjdadk` · slot 509128032 · authority `HRstpEJNgPzXUPMQmDFef4HzAfydbqK6KorqFuzMsTvy` | **ไม่ลงทะเบียน** (ไม่ถูก sponsor · ห้ามเป็น top-level ix ใน tx ที่ผ่าน relayer) |
| rewards-pool (ทีม rewards-pool) | `HgApfwjAg1L2FJL3fX7A4WX5yfhrVV8k9Ahii5Pn8HVW` | `CnmWkV6B1uXQ8fSiPtDBodeBWu6CfoqXhvAqgTZmL9Dk` · slot 509454201 · authority `HRstpE…` (เดียวกัน) | **รอลงทะเบียน** ในโปรเจค hub หลัง hub-web deploy · policy ผ่านการทดสอบบน devnet แล้ว (`tests/live/rewards-pool-policy.json`) |
| lobby (ทีม lobby) | `5zdBSvep3yV6v1SVD67uw9N5Wb3eT9Hp9D3eg4YSRqRu` | `5uWN3vjanUhyhQzpa4oVhjhp1otjaWq85FeNV9SporKu` · slot **509795184** · authority `HRstpE…` (เดียวกัน) | **รอลงทะเบียน**ในโปรเจค hub · มีคำขอให้ relayer เพิ่มเพดาน rent ต่อผู้ใช้ (`maintainer/<internal-note>`) ที่ยังไม่ได้ตอบ · ข้อมูลลงทะเบียนฉบับล่าสุด: `<internal-note>` · L14: lobby ปิด account เองด้วย Lobby Worker ไม่ใช้ sweep ของ relayer |

testkit ix (discriminator · accounts):
- `create` `181ec828051c0777` · [user s, payer w s (payer slot 1), thing PDA `["thing", user]` w, system]
- `close_thing` `a75149fabad23e79` · [user s, thing w, rent_payer w (refund slot 2)]
- `noop` `09b20d738123ed66` · [] (crank: แนบ user เป็น readonly signer)
- `fail_retryable` `2d8f8610d2bf0cd4` · Custom 6000
- `open_room(i64)` `e4fbda56c3dd8a9f` · [user s, payer w s (payer slot 1), room PDA `["room", user]` w, system]
- `sweep_close` `f1ed4afddfe936dc` · [room w, rent_payer w s] · ก่อนถึงเวลาได้ Custom 6001
- `drain` `ef131e786ffcf54a` (negative test ห้ามลงทะเบียน)
- account `Room` = `9cc7431bde17b95e` (`rent_payer` ที่ offset 8) · account `Thing` = `12f42ecb0fc17b7b`

genesis hash: devnet `EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG` · mainnet `5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d` (Worker ตรวจ RPC ของแต่ละ cluster กับค่านี้)

## 3. HTTP API (สำหรับ FE)
| method / path | ต้องมี | ได้ |
|---|---|---|
| `GET /relayer?project=<clientId>` | ไม่ส่ง Origin (เช่น curl) หรือ Origin ของโปรเจคที่ active ตัวใดก็ได้ (ไม่ใช่ Origin อื่น) | `{pubkey, cluster, policy.actions, cooldown_ms, user_daily_quota, wallet_binding}` · โปรเจคไม่ active / ไม่มี / policy เสีย → 404 |
| `POST /relay` | **Origin บังคับ และต้องอยู่ใน `allowed_origins` ของโปรเจคที่ token ระบุ** · `Authorization: Bearer <Web3Auth idToken>` · `Content-Type: application/json` · body `{"transaction": "<base64 wire tx>"}` | `200 {signature, quota}` หรือ error ตามตารางใน integration skill §4 |
| `GET /quota?wallet=<pubkey>` | Bearer idToken ของ wallet นั้น · กฎ Origin เดียวกับ `GET /relayer` | `{used, limit, resets_at}` (reset 00:00 UTC) |
| `OPTIONS` | Origin ที่อนุญาต | 204 + CORS (`Authorization, Content-Type` · expose `Retry-After`) |

- body ไม่เกิน 4 KB · tx เป็น Legacy หรือ v0 ที่ไม่มี ALT · signer 2 ตัว (fee payer ที่ slot 0 + ผู้ใช้) · ตัวอย่าง FE: `frontend/src/lib/solana.ts` (`buildTx`, `signWithWallet` + placeholder D18) และ `frontend/src/lib/testkit.ts`

## 4. ใช้ relayer กับแอปหรือ program ใหม่
1. **อ่าน integration skill** แล้วออกแบบ program/FE ตามกฎ (โดยเฉพาะ §2 payer slot, §3 รูปแบบ tx, §5 ข้อมูลที่ต้องส่ง)
2. **เลือกโปรเจค:**
   - ผู้เล่นของ hub → เพิ่ม program เข้าโปรเจคของ hub (H-3)
   - แอปอื่นที่มี Web3Auth clientId ของตัวเอง → สร้างโปรเจคใหม่ ทำตาม `docs/plans/2026-10-07_phase4-staging-runbook.md` ขั้น 2, 4, 6–8: สร้างกระเป๋า (`cargo run -q --example keygen -- <SLUG>`), `secret put`, insert แถวแบบ paused แล้วเปิด
3. **เขียน policy JSON** (schema: spec v3 §3 · ตัวอย่างจริง: `docs/plans/2026-10-09_v3-phase5-staging-runbook.md` ขั้น 3 และ `tests/live/rewards-pool-policy.json`)
4. **ตรวจในเครื่องก่อนเสมอ:** policy ที่ parse ไม่ผ่านทำให้**ทั้งโปรเจคใช้ไม่ได้** (สำหรับ hub = ทุกเกม)
   - ทดสอบบน devnet ผ่าน relayer จริง: `LIVE_POLICY=<file> LIVE_SCRIPT=<script> ./tests/run-live.sh` (ดูหัวข้อ 5)
   - อย่างน้อยให้โหลดเข้า `wrangler dev` ที่มี D1 local แล้ว `GET /relayer` ต้องได้ 200 พร้อม action ครบ
5. **maintainer ลงทะเบียนบน staging** (Oracle ไม่เขียน D1 remote เอง):
   1. เก็บ policy เดิมไว้
   2. `UPDATE projects SET policy = …` ผ่าน `--file`
   3. รอ 65 s แล้ว `GET /relayer`
   4. เตรียม SQL สำหรับ rollback (ตัวอย่างอยู่ใน runbook Phase 5)
6. **ทดสอบผ่าน FE จริง** (Web3Auth) แล้วตรวจ `relay_log`

## 5. เครื่องมือทดสอบ (`relayer/`)
| คำสั่ง | ทำอะไร | network |
|---|---|---|
| `cargo test --lib` | unit test (75) | ไม่มี |
| `cargo clippy --lib --tests --examples --target wasm32-unknown-unknown` | lint | ไม่มี |
| `./tests/run-e2e.sh [A B C D E F S M]` | e2e บน `wrangler dev` + mock RPC/JWKS · A–E = v1/v2 · F = program actions · S = cron sweep · M = ย้ายตัวนับรายวันจาก Worker รุ่นเก่า (`OLD_WORKER_DIR=<git worktree ของ version ที่ deploy อยู่>` · worktree ไม่มี `node_modules` ให้ symlink จาก `relayer/` เพื่อใช้ wrangler ตัวเดียวกัน แทนการดึงจาก npm) | ไม่มี (mock ทั้งหมด) |
| `./tests/run-live.sh` | relayer จริงในเครื่อง + **devnet จริง** (Alchemy จาก `.dev.vars`) + mock JWKS แทน Web3Auth · default = testkit 15 checks (ใช้ ~0.003 SOL + rent ของ Room 1 ชม. ค้างชั่วคราว) | devnet |
| `LIVE_POLICY=tests/live/rewards-pool-policy.json LIVE_SCRIPT=tests/live/rewards-pool.mjs ./tests/run-live.sh` | ตัวอย่างการทดสอบ policy ของโปรเจคอื่น (ไม่ส่ง tx จริง ไม่เสีย SOL) | devnet |
| `./target/debug/examples/progtx <fee_payer> <blockhash> <seed> <ix>...` | สร้าง tx ทดสอบ (`p:<program>:<hex>:<accts>` · `u:s`, `fp:w`, `new:w`, `pda/<seed>/<program>:w` · `cbl:` / `cbp:` · `PROGTX_V0=1`) | ไม่มี |
| `$FE/?v3` | แผงทดสอบ testkit ผ่าน Web3Auth จริง | staging |

## 6. งานดูแล (operator)
- **ลำดับการ deploy:** D1 migration ก่อน Worker เสมอ (Worker v3 อ่านคอลัมน์ใหม่) · DO migration ใหม่ (เพิ่ม class) ทำให้ `wrangler rollback` ข้ามไม่ได้ ต้องทำ revert แบบ deploy ไปข้างหน้า · ก่อน migration ที่ drop/rebuild ตาราง ให้จด `wrangler d1 time-travel info` + `d1 export`
- **runbook ที่รันแล้ว:** `docs/plans/2026-10-07_phase4-staging-runbook.md` (ขึ้น v2) · `2026-10-08_v3-staging-runbook.md` (อัปเกรด v3) · `2026-10-09_v3-phase5-staging-runbook.md` (program actions บน `STG_POC`)
- **ปิดฉุกเฉิน (เร็วสุด):**
  - `UPDATE projects SET status='paused'` มีผล ≤ 60 s
  - ถอด program ออกจาก policy (ห้ามถอดจน policy ว่าง)
  - ถอนเงินจากกระเป๋าให้ต่ำกว่า `min_balance` → 503 `low_balance` ≤ 30 s
- **เติมเงิน:** โอน SOL เข้า pubkey ของ fee payer (ไม่ต้องใช้ key) · ยอดต้องสูงกว่า `min_balance_lamports` ของโปรเจคเสมอ · **relayer ยังไม่มีระบบแจ้งเตือนยอดต่ำ** ต้องตรวจเอง (`solana balance <pubkey> --url devnet`)
- **program ที่ลงทะเบียนถูก upgrade** → ix ของ program นั้นตอบ 503 `program_changed` จนกว่าจะอัปเดต `deploy_slot` ใน policy (ทีมเจ้าของ program ต้องแจ้งก่อน upgrade)
- **audit:** `npx wrangler d1 execute gasless-relayer-audit --remote --command "SELECT id, status, reason, cost_lamports, rent_lamports, aud FROM relay_log ORDER BY id DESC LIMIT 20"` · status `success` (reason ว่าง หรือ `already processed`) / `rpc_error` (reason ขึ้นต้นด้วย `simulation_failed:`, `inner_ix_rejected:`, `retryable:`, `rejected:`, `unknown:` จาก user · `sweep_rejected:`, `sweep rejected:`, `sweep unknown:`, `sweep already processed` จาก cron) / `sweep` (reason = ชื่อ sweep)
- **ข้อจำกัดที่รู้แล้ว:**
  - ไม่มี budget หรือ audit แยกต่อ program (`relay_log` ไม่มีคอลัมน์ program id)
  - ยังไม่ได้วัด CPU time ของ cron บน Cloudflare
  - relayer ไม่ตรวจขนาด tx เอง (RPC เป็นคนปฏิเสธ tx ที่เกิน 1232 bytes)

## 7. กฎที่ห้ามละเมิด
- **devnet เท่านั้น** จนกว่า maintainer อนุมัติ mainnet แยกต่างหาก (ห้ามใส่ `RPC_URL_MAINNET`, ห้ามตั้ง `mainnet_enabled = true`)
- **ห้าม** ใส่ private key / RPC URL ที่มี API key ลงใน git, `wrangler.toml`, command line หรือเอกสาร · ส่ง secret ผ่าน stdin เท่านั้น
- `wrangler deploy`, D1 remote ที่เขียนข้อมูล, `secret put`, upgrade testkit = **maintainer อนุมัติ/รันเอง**
- ห้าม DELETE แถวใน `projects` / `fee_payer_history` (ใช้ `status='retired'`)
