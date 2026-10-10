---
name: gasless-relayer-integration
description: Best practices for designing a Solana program and frontend that use maintainer's gasless relayer (relayer pays fees, can front rent through a registered payer slot, closes accounts with a cron sweep). Use when designing or reviewing a Solana program's accounts/instructions, deciding who pays rent, building transactions in a frontend that sends them to the relayer, writing the relayer policy for a project, or preparing a project to register on the relayer.
---

# Gasless Relayer Integration — Best Practices

> ต้นฉบับ: `solana-poc/relayer/skills/gasless-relayer-integration/SKILL.md` (ห้ามแก้สำเนาในโปรเจคอื่น ให้แก้ที่ต้นฉบับแล้ว copy ใหม่)
> อ้างอิง: spec relayer v3 (`solana-poc/docs/plans/2026-10-08_program-actions-v3-spec.md`) · v2 (`2026-10-06_multi-project-relayer-v2-spec.md`) · research rent (`2026-10-08_rent-sponsorship-research.md`) · lobby compat (`2026-10-08_lobby-relayer-compat.md`)
> ตัวอย่างที่รันจริง: program `solana-poc/testkit` (Anchor 1.2.1) · FE `solana-poc/frontend/src/lib/testkit.ts` · policy ใน `docs/plans/2026-10-09_v3-phase5-staging-runbook.md` ขั้น 3
> ฉบับ: 2026-10-09 (v3) · ผู้ดูแล: maintainer

## 0. ภาพรวม relayer
- **1 โปรเจค = 1 Web3Auth clientId (`aud`) = 1 กระเป๋า fee payer = 1 cluster** แต่ละโปรเจคมี quota, budget และกระเป๋าของตัวเอง ความเสียหายของโปรเจคหนึ่งไม่ลามไปโปรเจคอื่น
- ผู้ใช้ login ผ่าน Web3Auth (social) → sign tx → `POST /relay` พร้อม idToken → relayer ตรวจ policy, sign เป็น fee payer (slot 0), (ถ้าต้อง) simulate แล้วตรวจผล → broadcast
- relayer **ไม่ cache state ของ program** chain คือ source of truth เดียว
- ทุก program ที่จะให้ sponsor ต้อง **ลงทะเบียนในโปรเจคที่ผู้เล่น login** (program ที่หลายโปรเจคใช้ร่วมกันก็ลงทะเบียนแยกในแต่ละโปรเจค)
- **blockchain-games-hub (H-3, maintainer 2026-10-10):** hub มี **โปรเจคเดียว** = Web3Auth clientId ของ hub-web · ทุก program ของ hub (lobby, rewards-pool, เกมต่างๆ) ลงทะเบียนเป็น program action ในโปรเจคนี้ ไม่แยกโปรเจคหรือ clientId ต่อเกม · ปิดทีละเกม = ถอด program ออกจาก policy (ห้ามถอดจน policy ว่าง) · quota / budget / RPM / cooldown / `max_instructions` / กระเป๋า fee payer ใช้ร่วมทุกเกม

## 1. สถานะการรองรับ

| ความสามารถ | สถานะ |
|---|---|
| SPL Memo v2 (prefix ต่อโปรเจค) | ✅ v2 |
| quota ต่อ user ต่อวัน · RPM / budget ต่อโปรเจคและ cluster · kill switch | ✅ v2 |
| **Program action**: allowlist ตาม program id + discriminator | ✅ v3 (devnet staging, 2026-10-09) |
| **Payer slot** (relayer ออก rent ให้ `init`) · **refund slot** (rent คืนเข้า relayer) | ✅ v3 |
| Simulation + ตรวจ inner instruction ก่อนส่ง (ปิดได้ต่อ program) · pin version ของ program | ✅ v3 |
| ComputeBudget แบบมีเพดาน · budget fee/rent แยกกัน · cooldown/backoff ต่อโปรเจค · custom error ที่ retry ได้ | ✅ v3 |
| Crank แบบ user-signed · **cron sweep** ปิด account แล้วคืน rent ทุก 5 นาที | ✅ v3 |
| Mainnet | 🔜 ต้องได้อนุมัติจาก maintainer แยกต่างหาก |
| Address Lookup Table · Loader-v4 · ลงทะเบียน System / ComputeBudget / BPF loader / Token / Token-2022 / ATA / Memo v1-v2 / ALT program / **precompile (Ed25519, Secp256k1, Secp256r1)** เป็น program action | ❌ ไม่รองรับ (precompile ทำให้ค่า fee ที่ประกาศผิด จึงใช้ signature verification แบบ precompile ใน tx ที่ผ่าน relayer ไม่ได้) |

## 2. ใครจ่าย rent: เลือกก่อนออกแบบ account

```
program สร้าง account ไหม?
├─ ไม่สร้าง ─────────────► ไม่ต้องคิดเรื่อง rent (relayer จ่ายแค่ fee)
└─ สร้าง
   ├─ program เป็นของเรา + review ได้ + คุม upgrade ได้ ─► (P) Payer slot ⭐ ง่ายที่สุด
   ├─ อยากให้ relayer ไม่ต้อง trust เลย / program ของคนอื่น ─► (V) Rent vault ของ program
   └─ ผู้ใช้มี SOL (wallet ปกติ ไม่ผ่าน relayer) ─────────► ผู้ใช้จ่ายเอง (payer เป็น Signer ทั่วไป)
```

**กฎของ trust:** relayer ต้อง trust program **ทุกตัวที่ได้รับกระเป๋า fee payer เป็น account ใน instruction** เพราะ fee payer เป็น signer ระดับ tx และสิทธิ์ signer ส่งต่อผ่าน CPI ได้ · relayer ป้องกันด้วย 3 ชั้น: ตำแหน่ง slot ที่ลงทะเบียน (W3) + pin version (W6) + simulate แล้วตรวจว่าเงินจาก fee payer ไปที่ไหน (§4.3 ของ spec) · program ที่จ่าย rent เองจาก vault ไม่ต้องได้ fee payer จึงไม่ต้อง trust

### (P) Payer slot: MUST
relayer ยอมให้ fee payer อยู่ใน instruction **เฉพาะ** ตำแหน่ง `payer_index` (หรือ `refund_index`) ของ ix ที่ลงทะเบียน และ **ทุก top-level ix ต้องเป็นของ program ที่ลงทะเบียน** (+ ComputeBudget) ซึ่งปิดช่องแบบ Kora #720 program ต้อง:
1. **แยก `user: Signer` กับ `payer: Signer` (`mut`)** ในทุก ix ที่สร้าง account · ix ที่ไม่สร้าง account **ห้ามรับ `payer`**
2. **ใช้ `payer` เป็นแหล่งเงินของ `init` / `init_if_needed` เท่านั้น** (System `createAccount`, หรือ `transfer` → `allocate` → `assign` ของ Anchor เมื่อ account มี lamports อยู่ก่อน) · ห้าม transfer ไปที่อื่น · **ห้ามส่ง `payer` ผ่าน CPI ไป program อื่น** นอกจาก System · relayer simulate แล้ว reject ทุก inner ix ที่มี fee payer นอกเหนือจากนี้ (`inner_ix_rejected`)
3. **account ที่ `payer` จ่ายต้องเก็บ `rent_payer: Pubkey`** และ **ปิดแล้วคืนให้ `rent_payer` เท่านั้น** (`close = rent_payer` + `has_one = rent_payer`) · ห้ามให้ผู้เรียกเลือกปลายทาง (ปิด rent farming)
4. **มีทางคืน rent เสมอ:** account ที่จบแล้ว/หมดอายุต้องปิดได้โดยไม่ต้องรอผู้ใช้ (social user ส่วนใหญ่ไม่กลับมากด) → ix ปิดแบบ **permissionless** ที่คืนเงินให้ `rent_payer` (ปลายทางตรึงไว้ ใครกดก็ไม่มีใครขโมยได้) แล้วเลือกคนกด 1 ใน 2 ทาง: (a) **worker ของโปรเจคเอง** ใช้ wallet ของตัวเอง (เช่น lobby → Lobby Worker ตาม L14, 2026-10-10) หรือ (b) **cron sweep ของ relayer** (§2.1)
5. **ขนาด account กำหนดตอน `init` และมีเพดาน** · TTL ที่ทำให้ rent ค้างต้องมีเพดานระดับ program (const)
6. **Upgrade = ต้องแจ้ง relayer:** relayer pin `deploy_slot` ของ ProgramData (หรือ `immutable`) ถ้า upgrade โดยไม่แจ้ง ทุก ix ของ program นั้นจะตอบ **503 `program_changed`** จนกว่า maintainer จะ review code ใหม่แล้วอัปเดต `deploy_slot` · upgrade authority ควรเป็น multisig หรือ freeze ก่อน mainnet
7. `emit!` event ที่มี `rent_payer` ตอนสร้างและตอนปิด (ไว้ตรวจย้อนหลัง)
8. test ที่ assert ว่า lamports ของ `payer` ลดลง **เท่ากับ rent ของ account ที่สร้างพอดี** · ตั้ง `max_rent_lamports` ของ ix ≥ rent จริง (relayer หัก rent budget ตามค่านี้ ไม่ใช่ตามยอดจริง และ reject ถ้ายอดที่ simulate ได้เกิน)
9. **discriminator 8 bytes** (Anchor ปกติอยู่แล้ว): ix ที่มี payer/refund slot ต้องยาว 8 bytes

### Refund slot (ix ที่ผู้ใช้เรียกเพื่อปิด account)
- ลงทะเบียน `refund_index` = ตำแหน่งของ `rent_payer` (writable) · relayer simulate แล้วบังคับว่ายอดของ fee payer **ไม่ลดเกินค่า fee**
- program ต้อง **pin version** (`deploy_slot` หรือ `immutable`) เหมือน payer slot
- ใช้ได้ **เฉพาะ account ที่ fee payer ของโปรเจคเป็น `rent_payer`** · account ที่ผู้ใช้จ่าย rent เองต้องปิดด้วย wallet ของผู้ใช้ (ส่งผ่าน relayer จะ fail `has_one`)
- **ห้ามมี payer slot กับ refund slot ใน tx เดียวกัน**

### 2.1 ถ้าเลือกให้ cron sweep ของ relayer ปิด (ทางเลือก (b) ของ §2 P ข้อ 4)
cron ของ relayer (ทุก 5 นาที) ค้น account ด้วย `getProgramAccounts` แล้วสร้าง tx ปิดเอง **โดยมี signer แค่ fee payer** · ออกแบบตามนี้ (ตัวอย่าง: testkit `Room` + `sweep_close`):
1. account มี discriminator ของตัวเอง (Anchor `account:<Name>`) และ **`rent_payer` อยู่ที่ offset คงที่** (เช่น เป็น field แรกหลัง discriminator = offset 8)
2. ix ปิดมี data = **discriminator 8 bytes เท่านั้น** (ไม่มี argument) และ accounts ตาม template ที่ลงทะเบียน เช่น `[account (mut), rent_payer (mut, Signer)]` (+ account คงที่ได้ เช่น config PDA)
3. `close = rent_payer` + `has_one = rent_payer` และ **`rent_payer` เป็น signer คนเดียวที่ ix ต้องการ**
4. ยังปิดไม่ได้ (ยังไม่หมดเวลา/เกมยังไม่จบ) → **return custom error** (เช่น `RetentionNotReached`) · relayer simulate แล้วข้าม (ไม่เสีย fee) · ห้ามสำเร็จโดยไม่คืนเงิน: relayer ส่งเฉพาะ tx ที่ rent กลับเข้ากระเป๋าอย่างน้อย `rent(0 bytes)`
5. account ที่ปิดไม่ได้ตลอดจะกิน simulate ทุกรอบ (ไม่เสียเงิน แต่เปลือง RPC) → ให้มีเวลาหมดอายุเสมอ

### (V) Rent vault: MUST
1. vault = PDA ของ program ที่ **System เป็น owner และไม่มี data** · สร้าง account ด้วย `system_program::create_account` + `invoke_signed` (Anchor `init` ต้องการ payer ที่เป็น Signer จึงต้องเขียน CPI เอง ยังไม่ได้ verify กับ anchor 1.2.1)
2. ปิดแล้วคืนเข้า vault (ตรึงใน code)
3. **กัน griefing:** จำกัดจำนวน account ต่อผู้ใช้, ปิดเองเมื่อเลิกใช้นาน, ตั้งเพดานขนาด, monitor ยอด vault
4. ผู้รับผิดชอบเติมเงิน vault คือเจ้าของ program (ไม่ใช่ relayer)
5. relayer ใช้กฎปกติ (fee payer ห้ามอยู่ใน ix) · ลงทะเบียนแค่ ix ที่จะ sponsor fee

### ทั้งสองแบบ: SHOULD
- พิจารณา **compressed state** ก่อนสร้าง account ต่อผู้ใช้จำนวนมาก (cNFT/Bubblegum ~0.00001 SOL ต่อ mint, Metaplex Core ~0.003 SOL เทียบกับ Token Metadata ~0.022 SOL)
- rent ต้องถามจาก cluster (`getMinimumBalanceForRentExemption`) อย่า hardcode · ค่าที่วัดได้บน devnet 2026-10-09: 73 bytes = 1,021,080 · 81 bytes = 1,061,720 · 0 byte = 650,240

## 3. รูปแบบ transaction ที่ relayer รับ (program action)
1. **Fee payer = `pubkey` จาก `GET /relayer?project=<clientId>`** ที่ slot 0 · อย่า hard-code
2. **Signer 2 ตัวพอดี: fee payer กับผู้ใช้** (`num_required_signatures == 2`) · ห้ามใช้ keypair ใหม่ sign (เช่น mint keypair) ให้ใช้ PDA แทน
3. Message: Legacy หรือ v0 **ไม่มี ALT** · ห้ามมี account ซ้ำ · ขนาดจริง < 1232 bytes
4. **ทุก top-level ix ต้องเป็นของ program ที่ลงทะเบียน** (+ ComputeBudget) · ต้องมี ix ที่ลงทะเบียนอย่างน้อย 1 ตัว · จำนวน ix ≤ `max_instructions` (default 4, **นับ ComputeBudget ด้วย**) · ix ต้อง match discriminator + `min_accounts..max_accounts` + `max_data_len` ที่ลงทะเบียน
5. **ห้ามผสม memo กับ program action** ใน tx เดียว · Token / ATA / Token-2022 / Memo / System เป็น top-level ix ไม่ได้ (ให้ program ของเรา CPI ไปแทน)
6. **ComputeBudget:** `SetComputeUnitLimit` ≤ `max_cu` (default 400,000) และ `SetComputeUnitPrice` ≤ `max_cu_price_micro_lamports` (default 0 = ห้าม priority fee) อย่างละไม่เกิน 1 ครั้ง · **ตั้ง price ต้องตั้ง limit ด้วย**
7. **Crank ix** (เช่น `tick`, ใครก็เรียกได้): FE ใส่ **ผู้ใช้เป็น readonly signer ท้าย accounts ของ crank ix** (remaining account) เพื่อให้ relayer นับ quota ได้ · ลงทะเบียน `max_accounts` เผื่อ +1 · **program ต้องไม่อ่าน `remaining_accounts` ของ crank ix** หรือต้องข้าม account ท้ายสุดที่เป็น readonly signer (Anchor ไม่สนใจ remaining accounts ถ้าไม่ได้อ่านเอง)
8. **Conflict ต้อง fail ก่อนเข้า chain:** ตรวจ precondition (phase, counter, pointer) ตั้งแต่ต้น ix · relayer simulate ก่อนส่ง (เมื่อเปิด `verify_inner_ix`) ถ้า fail ด้วย error ปกติ → 400 `simulation_failed` + backoff ที่ยาวขึ้นเรื่อยๆ ต่อ wallet · program ที่ปิด `verify_inner_ix` (หรือ state เปลี่ยนระหว่าง simulate กับส่ง) จะไป fail ที่ preflight ของ RPC → 502 `rpc_rejected` + backoff แบบเดียวกัน · ทั้งสองทางไม่เสีย fee
9. **Race ที่คาดได้ (เช่น ห้องเต็ม, ถูกปิดไปก่อน) ให้ใช้ custom error ที่ลงทะเบียนเป็น `retryable_errors`** → 409 `retryable` + cooldown สั้น (`retry_cooldown_ms` ≥ 100 ms) ไม่สะสม backoff · relayer ผูก error กับ **top-level ix index** ใน `InstructionError[i, {Custom: c}]` ดังนั้น error ที่มาจาก program ที่ถูก CPI จะนับเป็นของ program ที่ลงทะเบียน → **เลือกเลข retryable ไม่ให้ชนกับ error ของ program ที่ตัวเอง CPI ไป** (เช่น Anchor `3012 AccountNotInitialized` ใช้ได้เมื่อ user ชนกับ sweep ที่ปิดไปก่อน)
10. **cooldown ต่อ wallet** ตั้งต่อโปรเจคได้ (`cooldown_ms`, default ของ Worker 10 s) · เกมที่ต้องส่ง tx ถี่ให้ขอ cooldown 0–1000 ms แล้วพึ่ง RPM + quota + budget แทน · ยังควรรวม action ไว้ใน ix เดียวเท่าที่ทำได้
11. **ค่าใช้จ่ายที่ relayer นับ:** fee = 10,000 (2 signatures) + `ceil(cu_limit × cu_price / 1e6)` → fee budget · rent = ผลรวม `max_rent_lamports` ของทุก ix ที่มี payer slot → rent budget (คนละ budget กัน, rent ที่คืนตอนปิดไม่คืนเข้า budget)
12. **CU ที่วัดจริงบน devnet** (testkit, Anchor 1.2.1, 2026-10-09): noop 502 · `init` account 73 bytes 7,933 · `init` 81 bytes + Clock 12,667 · close (refund slot) 3,680 · sweep close 2,005 · วัดของ program ตัวเองแล้วส่งให้ relayer (§5)

## 4. Frontend (Web3Auth + @solana/kit)
1. **D18 (placeholder):** Web3Auth embedded wallet ไม่ยอม partial-sign (error 5663009) → ใส่ **placeholder 64 bytes (ไม่ใช่ 0 ทั้งหมด) ลงใน slot อื่นนอกจากของผู้ใช้** ก่อนเรียก `solana:signTransaction` แล้ว relayer เขียนทับเอง · ดู `frontend/src/lib/solana.ts` (`withPlaceholders`, `signWithWallet`) · **verify ใน browser แล้ว (2026-10-09):** ใช้ได้กับ memo, crank (ผู้ใช้เป็นแค่ readonly signer) และ payer slot (fee payer เป็น writable signer ใน ix)
2. สร้าง ix ด้วย `@solana/kit` แล้ว `buildTx(relayer, ix)` (fee payer = relayer, blockhash `confirmed` ใหม่ทุกครั้งก่อนเปิด popup) · ตัวอย่างครบทุกแบบ: `frontend/src/lib/testkit.ts` (PDA ด้วย `getProgramDerivedAddress`, `AccountRole` ตามตำแหน่ง slot)
3. account ของผู้ใช้: เรียก `standard:connect` · sign ผ่าน wallet-standard feature โดยตรง (`useSignTransaction` กลืน error)
4. Build-time `VITE_SOLANA_CLUSTER` ต้องตรงกับ `cluster` ใน `GET /relayer` และ RPC ของ FE ต้องเป็น cluster เดียวกัน (ตรวจ genesis hash) ถ้าไม่ตรงให้ปิดการส่ง tx
5. **จัดการ error ตาม code** (ดู `frontend/src/lib/messages.ts`):

| HTTP / code | FE ควรทำ |
|---|---|
| 200 | poll สถานะ signature (`confirmed`) |
| 400 `validation` | bug ฝั่ง FE/policy: ix, slot, ComputeBudget หรือขนาดไม่ตรงที่ลงทะเบียน (`reason` บอกสาเหตุ) |
| 400 `simulation_failed` | แสดง error ของ program (`reason`) · รอ `retry_after_ms` (backoff) |
| 400 `inner_ix_rejected` | program ใช้เงินของ relayer ผิดกฎ: แจ้งผู้ดูแล ไม่ใช่ผู้ใช้ |
| 409 `retryable` | อ่าน state จาก chain ใหม่ แล้ว **build tx ใหม่** หลัง `retry_after_ms` |
| 409 `in_progress` | tx ก่อนหน้าของ wallet นี้ยังไม่จบ (ไม่มี `retry_after_ms`): รอสักครู่แล้ว build ใหม่ |
| 429 `cooldown` / `project_throttle` / `cluster_throttle` | รอ `retry_after_ms` แล้ว build ใหม่ |
| 502 `rpc_rejected` | RPC preflight ปฏิเสธ (เช่น program error เมื่อปิด `verify_inner_ix`, blockhash หมดอายุ): แสดง `reason`, รอ `retry_after_ms` (backoff) แล้ว build ใหม่ |
| 429 `user_quota` · 503 `project_budget` / `cluster_budget` (`reason`: `fee` หรือ `rent`) | แสดงเวลา reset (00:00 UTC) ไม่ต้องนับถอยหลัง |
| 503 `program_changed` | program ถูก upgrade และยังไม่ได้ re-approve: ปิดปุ่มที่ใช้ program นั้น |
| 503 `low_balance` / `upstream` | relayer ไม่พร้อม: ลองใหม่ภายหลัง |
| 504 `rpc_unknown` | **ไม่ใช่ fail:** poll `signature` ที่ได้มา หรืออ่าน account บน chain ใหม่ · ห้าม rollback state ฝั่ง client |

6. แสดง quota จาก `GET /quota?wallet=` และจาก response ของ `/relay`
7. ห้ามส่ง tx เดิมซ้ำหลังได้ 409/429 ให้ build ใหม่ด้วย blockhash ใหม่

## 5. ข้อมูลที่ต้องส่งให้ relayer ตอนลงทะเบียนโปรเจค
1. Web3Auth clientId (Sapphire network ตรงกับ cluster) + origin ของ FE (exact, https)
2. cluster (devnet / mainnet)
3. **program id + IDL ที่มี version + deploy slot ปัจจุบัน** (หรือยืนยันว่า immutable) + ผล review ของ code version นั้นถ้ามี payer/refund slot
4. ต่อ ix ที่จะ sponsor: `name`, `discriminator` (hex), `min_accounts` / `max_accounts` (crank +1), `max_data_len`, และถ้ามี `payer_index` + `max_rent_lamports` หรือ `refund_index`
5. `retryable_errors` (เลข custom error) · CU ต่อ ix ที่วัดจริง (ถ้าต้องใช้ ComputeBudget ให้บอก limit/price)
6. sweep (ถ้ามี): `account_discriminator` (hex ของ `account:<Name>`), `rent_payer_offset`, `ix_discriminator`, template ของ accounts, จำนวนสูงสุดต่อรอบ
7. **ประมาณการ:** tx ต่อผู้ใช้ต่อวัน, rent ต่อ action, ผู้ใช้ต่อวัน, ความถี่ของ tx ต่อ wallet → ใช้ตั้ง quota, RPM, fee budget, **rent budget** (default 0 = payer slot ใช้ไม่ได้), cooldown · **sweep ใช้ทรัพยากรของโปรเจคด้วย:** ทุก RPC call ของ sweep (list, blockhash, simulate, send) กิน RPM ของโปรเจคและ cluster 1 ช่อง และทุก tx ที่ส่งกิน fee budget 5,000 lamports (เช่น `max_simulations_per_run` 20 = สูงสุดราว 25 RPM ต่อรอบ 5 นาที)
8. กระเป๋า fee payer สร้างโดย maintainer ตาม runbook (Worker secret) · เจ้าของโปรเจคไม่ต้องถือ key · ตกลงกันว่าใครเติม SOL · ถ้ามี payer slot: `min_balance_lamports` ≥ `max_instructions × max(max_rent_lamports) + 1,000,000`

ตัวอย่าง policy (devnet staging, testkit):
```json
{"actions":[
 {"type":"memo","max_bytes":128,"prefix":"gasless-poc:"},
 {"type":"program","program_id":"Bh4GY79ytnEZ4t3ZGf7qUVooaSKYyC8sM1uJpiM2xdCW","deploy_slot":509032443,"retryable_errors":[6000],
  "instructions":[
   {"name":"create","discriminator":"181ec828051c0777","payer_index":1,"max_rent_lamports":1100000,"min_accounts":4,"max_accounts":4,"max_data_len":8},
   {"name":"close_thing","discriminator":"a75149fabad23e79","refund_index":2,"min_accounts":3,"max_accounts":3,"max_data_len":8},
   {"name":"noop","discriminator":"09b20d738123ed66","min_accounts":0,"max_accounts":1,"max_data_len":8},
   {"name":"fail_retryable","discriminator":"2d8f8610d2bf0cd4","min_accounts":0,"max_accounts":1,"max_data_len":8},
   {"name":"open_room","discriminator":"e4fbda56c3dd8a9f","payer_index":1,"max_rent_lamports":1100000,"min_accounts":4,"max_accounts":4,"max_data_len":16}],
  "sweeps":[{"name":"close_room","account_discriminator":"9cc7431bde17b95e","rent_payer_offset":8,"ix_discriminator":"f1ed4afddfe936dc",
   "accounts":[{"kind":"swept","writable":true},{"kind":"fee_payer","writable":true}],"max_sends_per_run":5,"max_simulations_per_run":20}]}],
 "limits":{"max_instructions":4,"max_cu":400000,"max_cu_price_micro_lamports":0}}
```
- `verify_inner_ix` (default `true`): ปิดได้เฉพาะ program ที่ pin แล้วและเราเขียนเองทั้งหมด (ลด latency แต่ความปลอดภัยพึ่ง code ที่ pin ไว้อย่างเดียว)
- `open_room` = ix ที่มี argument (`i64`, data 16 bytes) และสร้าง `Room` ที่ sweep `close_room` ปิด · `noop` / `fail_retryable` = crank (`max_accounts` 1 = ผู้ใช้ที่แนบท้าย)
- policy ที่ parse ไม่ผ่าน = **ทั้งโปรเจคใช้ไม่ได้ (รวม memo)**: `GET /relayer` ตอบ 404 เมื่อเรียกด้วย curl (ไม่มี Origin) แต่จาก browser จะได้ 403 `origin` / CORS error เพราะ origin ของโปรเจคหลุดจาก allowlist (FE จะเห็นเป็น "Relayer unreachable") · ต้องตรวจกับ relayer ในเครื่องก่อนใส่ D1 จริง

## 6. Checklist ก่อนขอลงทะเบียน
- [ ] เลือก rent model แล้ว (ไม่สร้าง account / P / V) และเขียนเหตุผลไว้
- [ ] (P) ข้อ 1–9 ของ §2 ครบ และมี test ข้อ 8
- [ ] ทุก account ที่ relayer ออก rent มีทางคืน: refund slot และ/หรือ ix ปิดแบบ permissionless ที่ worker ของโปรเจคกด หรือที่ cron sweep ของ relayer ปิดได้ตาม §2.1 (signer = `rent_payer` คนเดียว, error เมื่อยังปิดไม่ได้)
- [ ] (V) griefing มีทางจำกัดแล้ว และกำหนดคนเติม vault แล้ว
- [ ] ไม่มี ix ไหนส่ง fee payer / payer ผ่าน CPI ไป program อื่น (นอกจาก System)
- [ ] crank ix ไม่อ่าน `remaining_accounts` (หรือข้าม signer ท้ายสุด)
- [ ] race ที่คาดได้ใช้ custom error ที่ลงทะเบียนเป็น retryable และไม่ชนกับ error ของ program ที่ CPI ไป
- [ ] tx ของ flow หลัก: signer 2 ตัว, ไม่มี ALT, ไม่มี account ซ้ำ, วัด CU และขนาดแล้ว
- [ ] FE ทำ D18 placeholder, ตรวจ cluster, จัดการ error ตาราง §4 (โดยเฉพาะ `retryable` และ `rpc_unknown`)
- [ ] มีแผนแจ้ง relayer ก่อน upgrade program (ไม่งั้น 503 `program_changed`)
- [ ] ส่งข้อมูลตาม §5 ครบ
