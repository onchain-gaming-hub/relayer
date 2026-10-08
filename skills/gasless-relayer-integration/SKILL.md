---
name: gasless-relayer-integration
description: Best practices for designing a Solana program and frontend that use maintainer's gasless relayer (relayer pays fees, and may pay rent). Use when designing or reviewing a Solana program's accounts/instructions, deciding who pays rent, building transactions in a frontend that sends them to the relayer, or preparing a project to register on the relayer.
---

# Gasless Relayer Integration — Best Practices

> ต้นฉบับ: `solana-poc/relayer/skills/gasless-relayer-integration/SKILL.md` (ห้ามแก้สำเนาในโปรเจคอื่น ให้แก้ที่ต้นฉบับแล้ว copy ใหม่)
> อ้างอิง: spec relayer v2 (`solana-poc/docs/plans/2026-10-06_multi-project-relayer-v2-spec.md`) · research rent (`2026-10-08_rent-sponsorship-research.md`) · lobby compat (`2026-10-08_lobby-relayer-compat.md`)
> ฉบับ: 2026-10-08 · ผู้ดูแล: maintainer

## 0. ภาพรวม relayer
- **1 โปรเจค = 1 Web3Auth clientId (`aud`) = 1 กระเป๋า fee payer = 1 cluster** แต่ละโปรเจคมี quota, budget และกระเป๋าของตัวเอง ความเสียหายของโปรเจคหนึ่งไม่ลามไปโปรเจคอื่น
- ผู้ใช้ login ผ่าน Web3Auth (social) → sign tx → `POST /relay` พร้อม idToken → relayer ตรวจ policy, sign เป็น fee payer (slot 0) แล้ว broadcast
- relayer **ไม่ cache state ของ program** เสมอ chain คือ source of truth เดียว

## 1. สถานะการรองรับ ⚠️ อ่านก่อน

| ความสามารถ | สถานะ |
|---|---|
| SPL Memo v2 instruction เดียว (prefix ต่อโปรเจค) | ✅ **ใช้ได้แล้ว** (v2, devnet staging) |
| quota ต่อ user ต่อวัน, budget ต่อโปรเจค / cluster, cooldown, kill switch | ✅ ใช้ได้แล้ว (cooldown 10 s ใช้ค่าเดียวทุกโปรเจค) |
| **Instruction ของ program ที่ลงทะเบียน (allowlist ตาม discriminator)** | 🔜 **แผน v3** (ยังไม่มี spec) |
| **Trusted payer slot** (relayer จ่าย rent ให้ program ที่ลงทะเบียน) | 🔜 แผน v3 |
| ComputeBudget แบบมีเพดาน · cooldown/backoff ต่อโปรเจค · custom error ที่ retry ได้ · crank (cron) | 🔜 แผน v3 |
| Mainnet | 🔜 Phase 5 (ต้องได้อนุมัติจาก maintainer) |

**ออกแบบ program ตามกฎในเอกสารนี้ได้เลยตั้งแต่ตอนนี้** เพื่อให้ลงทะเบียนได้ทันทีเมื่อ v3 พร้อม และอย่าสัญญากับใครว่าฟีเจอร์ 🔜 ใช้ได้แล้ว

## 2. ใครจ่าย rent: เลือกก่อนออกแบบ account

```
program สร้าง account ไหม?
├─ ไม่สร้าง ─────────────► ไม่ต้องคิดเรื่อง rent (relayer จ่ายแค่ fee)
└─ สร้าง
   ├─ program เป็นของเรา + review ได้ + คุม upgrade ได้ ─► (P) Trusted payer slot ⭐ ง่ายที่สุด
   ├─ อยากให้ relayer ไม่ต้อง trust เลย / program ของคนอื่น ─► (V) Rent vault ของ program
   └─ ผู้ใช้มี SOL (wallet ปกติ ไม่ผ่าน relayer) ─────────► ผู้ใช้จ่ายเอง (ออกแบบ payer เป็น Signer ทั่วไป)
```

**กฎของ trust:** relayer ต้อง trust program **ทุกตัวที่ได้รับกระเป๋า fee payer เป็น account ใน instruction** เพราะสิทธิ์ signer ส่งต่อผ่าน CPI ได้ ถ้า program A ได้ fee payer มาแล้ว CPI ไป B โดยส่งต่อไปด้วย ต้อง trust ทั้ง A และ B · program ที่จ่าย rent เองจาก vault ไม่ต้องได้ fee payer จึง **ไม่ต้อง trust**

### (P) Trusted payer slot: MUST
relayer จะยอมให้ fee payer อยู่ใน instruction **เฉพาะ** (program id, discriminator, ตำแหน่ง account) ที่ลงทะเบียนไว้ และ **ทุก top-level ix ใน tx ต้องเป็นของ program ที่ลงทะเบียน** (+ ComputeBudget) เพื่อปิดช่องแบบ Kora #720 ที่แนบ `CreateAccount` ของใครก็ได้ไว้ข้างๆ program ต้อง:
1. **แยก `player: Signer` กับ `payer: Signer`** (payer = `mut`) ในทุก ix ที่สร้าง account · ix ที่ไม่สร้าง account **ห้ามรับ `payer`**
2. **ใช้ `payer` เป็นแหล่งเงินของ `init` / `init_if_needed` เท่านั้น** (System `CreateAccount`) · ห้าม `transfer`, ห้ามเก็บไว้ใช้ทีหลัง, **ห้ามส่ง `payer` ผ่าน CPI ไป program อื่น** นอกจาก System program
3. **ทุก account ที่ `payer` จ่ายต้องเก็บ `rent_payer: Pubkey`** และ **ปิดแล้วคืนให้ `rent_payer` เท่านั้น** (`close = rent_payer` + `has_one = rent_payer`) · ห้ามให้ผู้เรียกเลือกปลายทาง ข้อนี้คือสิ่งที่ปิด rent farming
4. **มีทางคืน rent เสมอ:** account ที่หมดอายุหรือจบแล้วต้องมี ix ปิดแบบ permissionless (ปลายทางตรึงไว้ จึงไม่มีใครขโมยได้) · account ที่ "เป็นของผู้ใช้" (เช่น claim/profile) ต้องมีทางปิดเมื่อเลิกใช้นาน ไม่ใช่ปิดได้แค่โดยเจ้าของ เพราะ social user ส่วนใหญ่ไม่กลับมากด
5. **ขนาด account กำหนดตอน `init` และมีเพดาน** · TTL ที่ทำให้ rent ค้างต้องมีเพดานระดับ program (const) ไม่ใช่ปล่อยให้ config ตั้งได้ไม่จำกัด
6. **Upgrade authority = multisig หรือ freeze ก่อน mainnet** · การ trust ผูกกับ code version ที่ review แล้ว **ทุก upgrade ต้องแจ้ง relayer ให้ review ใหม่** ก่อน deploy
7. `emit!` event ที่มี `rent_payer` ตอนสร้างและตอนปิด เพื่อให้ crank กวาดคืนได้
8. เขียน test ที่ assert ว่า lamports ของ `payer` ลดลงเท่ากับ rent ของ account ที่สร้างพอดี (ไม่มากกว่า)

### (V) Rent vault: MUST
1. vault = PDA ของ program ที่ **system เป็น owner และไม่มี data** · สร้าง account ด้วย `system_program::create_account` + `invoke_signed` (Anchor `init` ต้องการ payer ที่เป็น Signer จึงต้องเขียน CPI เอง ยังไม่ได้ verify กับ anchor 1.2.1)
2. ปิดแล้วคืนเข้า vault (ตรึงใน code)
3. **กัน griefing:** program แบบ permissionless (ใครก็เรียกตรงได้ไม่ผ่าน relayer) เปิดให้ใครก็ล็อกเงินใน vault ได้ → จำกัดจำนวน account ต่อผู้ใช้, ปิดเองเมื่อเลิกใช้นาน, ตั้งเพดานขนาด, และ monitor ยอด vault
4. ผู้รับผิดชอบเติมเงิน vault คือเจ้าของ program (ไม่ใช่ relayer)
5. relayer ใช้กฎปกติ (fee payer ห้ามอยู่ใน ix) จึงไม่ต้องลงทะเบียน payer slot · ลงทะเบียนแค่ program/ix ที่จะ sponsor fee

### ทั้งสองแบบ: SHOULD
- พิจารณา **compressed state** ก่อนสร้าง account ต่อผู้ใช้จำนวนมาก (cNFT/Bubblegum ~0.00001 SOL ต่อ mint, Metaplex Core ~0.003 SOL เทียบกับ Token Metadata ~0.022 SOL)
- สูตร rent (verify จาก RPC 2026-10-07): `(128 + bytes) × 5,080 lamports` (devnet และ mainnet ได้ 650,240 สำหรับ 0 byte)

## 3. รูปแบบ transaction ที่ relayer รับ
1. **Fee payer = `pubkey` จาก `GET /relayer?project=<clientId>`** และอยู่ที่ slot 0 · อย่า hard-code
2. **Signer มีแค่ fee payer กับผู้ใช้** · อย่าออกแบบ flow ที่ต้องให้ keypair ใหม่ sign (เช่น mint keypair) ถ้าไม่จำเป็น ให้ใช้ PDA แทน
3. Message: Legacy หรือ V0 **ไม่มี ALT** · ให้ tx ของ flow หลักใช้ ≤ ~20 accounts และวัดขนาดจริงให้ต่ำกว่า 1232 bytes
4. **ไม่มี priority fee หรือ ComputeBudget** จนกว่า v3 จะรองรับแบบมีเพดาน → วัด CU ของทุก ix ให้ต่ำกว่า default (200k ต่อ ix) หรือแจ้ง relayer ว่าต้องใช้เท่าไร
5. **Crank ix** (ใครก็เรียกได้ เช่น `tick`) ให้ FE ใส่ผู้ใช้เป็น signer เพิ่มใน tx (อยู่ใน account keys แม้ ix ไม่ได้ใช้) เพื่อให้ relayer นับ quota ได้ · ส่วนการคืน rent แบบ batch relayer จะกวาดเองด้วย cron (v3)
6. **Conflict ต้อง fail ตอน preflight:** ตรวจ precondition (phase, counter, pointer) ตั้งแต่ต้น ix เพื่อให้ tx ที่ชนกัน fail ตอน simulate (ไม่เสีย fee) · **ระบุ custom error ที่ retry ได้** (เช่น `StaleMatchmaker`) ให้ relayer ไม่ลงโทษด้วย backoff (v3)
7. **อย่าออกแบบให้ต้องส่งหลาย tx ติดกันเร็วๆ** (ตอนนี้ cooldown 10 s ต่อ user ต่อโปรเจค) · รวม action ไว้ใน ix เดียวเท่าที่ทำได้ (เช่น `request_start` ที่ประเมิน ready ใน ix เดียว)

## 4. Frontend (Web3Auth + @solana/kit)
1. **D18:** Web3Auth embedded wallet ไม่ยอม partial-sign (error 5663009) → ใส่ **placeholder 64 bytes (ไม่ใช่ 0 ทั้งหมด) ลงใน slot ของ fee payer** ก่อนเรียก `solana:signTransaction` แล้ว relayer จะเขียนทับเอง · ดู `solana-poc/frontend/src/lib/solana.ts` (`withPlaceholders`, `signWithWallet`)
2. account ของผู้ใช้: เรียก `standard:connect` (`useSolanaWallet().accounts` จะเป็น null) · sign ผ่าน wallet-standard feature โดยตรง (`useSignTransaction` กลืน error)
3. blockhash ใช้ commitment `confirmed` และดึงใหม่ทุกครั้งก่อนเปิด popup
4. Build-time `VITE_SOLANA_CLUSTER` ต้องตรงกับ `cluster` ใน `GET /relayer` และ RPC ของ FE ต้องเป็น cluster เดียวกัน (ตรวจ genesis hash) ถ้าไม่ตรงให้ปิดการส่ง tx
5. **`504 rpc_unknown` ไม่ใช่ fail:** relayer คืน `signature` มา ให้ poll สถานะ หรืออ่าน account บน chain ใหม่ · ห้าม rollback state ฝั่ง client
6. แสดง quota จาก `GET /quota?wallet=` และจาก response ของ `/relay` · error ที่ต้องรอยาว (`user_quota`, `project_budget`, `cluster_budget`) ให้แสดงเวลา reset (00:00 UTC) แทนการนับถอยหลัง
7. ห้ามส่ง tx เดิมซ้ำหลังได้ 409/429 ให้ build ใหม่ด้วย blockhash ใหม่

## 5. ข้อมูลที่ต้องส่งให้ relayer ตอนลงทะเบียนโปรเจค
1. Web3Auth clientId (Sapphire network ตรงกับ cluster) + origin ของ FE (exact, https)
2. cluster (devnet / mainnet)
3. **program id + IDL ที่มี version** + รายการ ix ที่จะให้ sponsor: discriminator และ (ถ้าใช้ P) **index ของ `payer`** ในแต่ละ ix
4. **ประมาณการปริมาณ:** tx ต่อผู้ใช้ต่อวัน (รวม tx ระหว่างเกม), rent ต่อ action (lamports), จำนวนผู้ใช้ต่อวัน เพื่อใช้ตั้ง quota, budget และ cooldown
5. Custom error ที่ retry ได้ · CU ต่อ ix (วัดจริง)
6. Upgrade authority (address และรูปแบบ: multisig / freeze) · ถ้าใช้ P ต้องมีผล review ของ code version ที่จะลงทะเบียน
7. กระเป๋า fee payer ของโปรเจคสร้างโดยผู้ดูแล relayer (maintainer) ตาม runbook และเก็บเป็น Worker secret · เจ้าของโปรเจคไม่ต้องถือ key และไม่ต้องส่ง key ให้ใคร · ต้องตกลงกันว่าใครเติม SOL

## 6. Checklist ก่อนขอลงทะเบียน
- [ ] เลือก rent model แล้ว (ไม่สร้าง account / P / V) และเขียนเหตุผลไว้
- [ ] (P) ข้อ 1–8 ของ §2 ครบ และมี test ข้อ 8
- [ ] (V) griefing มีทางจำกัดแล้ว และกำหนดคนเติม vault แล้ว
- [ ] ไม่มี ix ไหนส่ง fee payer หรือ payer ผ่าน CPI ไป program อื่น (นอกจาก System)
- [ ] ทุก account ที่ relayer จ่าย rent มีทางคืนเงินให้ `rent_payer`
- [ ] tx ของ flow หลัก: signer 2 ตัว, ไม่มี ALT, วัด CU และขนาดแล้ว
- [ ] FE ทำ D18 placeholder, ตรวจ cluster, จัดการ `rpc_unknown`
- [ ] ส่งข้อมูลตาม §5 ครบ
