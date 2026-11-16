# Völund: แบบของ harness สำหรับ PLE

> ชื่อ Völund (Völundr) มาจากช่างตีเหล็กและนักประดิษฐ์ชั้นครูในตำนาน Norse ผู้สร้างของใช้งานได้จริงจากวัตถุดิบ
> ตรงกับงานนี้ที่รับโฟลเดอร์โปรเจกต์มาแล้วต้องส่งงานวิศวกรรมที่ใช้ได้จริง

- **สถานะ:** M2 โค้ดเสร็จ: driver ให้ State Machine คุม phase, World บันทึกงานผ่าน tool `world_*`, MCP client ใช้ได้ `kit check` ผ่านทุกขั้นกับ mock model ส่วนที่ยังรอคือการรันกับโมเดลจริง (รอโปรเจกต์ GCP)
- **ปรับปรุงล่าสุด:** 16 พ.ย. 2026
- **ฉบับอ่านง่ายสำหรับทีม:** [docs/design.html](docs/design.html)
- **ปิดรับงาน:** 5 ม.ค. 2027 23:59 UTC (**6 ม.ค. 06:59 เวลาไทย**) ทีมตั้งเป้าส่งภายใน 4 ม.ค.

## 0. สิ่งที่ตัดสินใจแล้ว

| เรื่อง | ตัดสินใจ | เหตุผล |
|---|---|---|
| ภาษา | Rust ทั้งตัว harness | kit รับ server ภาษาอะไรก็ได้ที่ฟัง WebSocket บน `$PORT` |
| License | Apache-2.0 | การแข่งบังคับ MIT หรือ Apache-2.0 และโค้ดตัวอย่างของผู้จัดก็เป็น Apache-2.0 |
| แกนของแบบ | World Model + Phase State Machine | ดู §3 และ §4 |
| ที่รัน eval | ทดสอบบน Mac ก่อน แล้วค่อยขึ้น Linux VM แบบ amd64 | ดู §9 |
| Toolchain | Rust 1.98.1 (pin ใน `rust-toolchain.toml`) | build ใน container ต้องได้ผลเหมือนในเครื่อง ห้ามอัปเดตโดยไม่ตั้งใจ |

## 1. ข้อจำกัดที่แบบนี้ยึดตาม (จาก repo ทางการ `google-research/ple`)

**Protocol** (`docs/protocol.md`)
- harness เป็น WebSocket server บน `0.0.0.0:$PORT` ต้องรับการเชื่อมต่อภายใน 120 วินาที ห้ามส่ง ping (หรือตั้ง timeout ให้ยาวกว่าคำสั่งที่ยาวที่สุด)
- frame เป็น JSON ขนาดไม่เกิน 16 MiB รันพร้อมกันได้ไม่เกิน 8 request ถ้าส่ง frame ผิดรูปแบบเกิน 20 ครั้ง task จบทันที
- `exec` รัน `bash -c` แบบ subshell ใหม่ทุกครั้ง (`cd` และ export ไม่ติดไปคำสั่งถัดไป) timeout สูงสุด 600 วินาที output เกิน 100 KB จะเหลือแค่หัวกับท้าย
- `read_file` และ `write_file` รับส่งได้ครั้งละไม่เกิน 10 MB ส่วน `http` เรียกได้เฉพาะ URL ของ MCP server ของ task
- เมื่อได้ `cancel` มีเวลา 10 วินาทีให้ส่ง `done` เสมอ

**Container**
- serve container มี 2 CPU, RAM 4 GB, root เป็น read-only ส่วน `/tmp` และ `$HOME` เขียนได้อย่างละ 1 GB
- ออก network ได้เฉพาะ host ของโมเดลผ่าน egress proxy ห้ามใช้ web search และ tool ฝั่ง provider
- task container ไม่มี network ให้เวลา task ละ 6 ชั่วโมง
- build container (`debian:bookworm`) **ไม่มี Rust** มีแค่ build-essential, curl, git, python3, uv และ Node 22 ต้องเสร็จใน 30 นาที ทุกอย่างต้องอยู่ใต้ `/opt/harness` (ขนาดหลังบีบอัดไม่เกิน 2 GiB)

**โมเดล** (`docs/models.md`)
- ใช้ได้ทีละโมเดลเดียวตามที่ประกาศใน `submission.yaml` ถ้าเรียกโมเดลอื่นจะได้ 403
- ถ้าไม่ส่ง `reasoning_effort` gateway จะใช้ระดับสูงสุด (`max` สำหรับ Claude และ GLM, `high` สำหรับ Gemini และ gpt-oss)
- Claude Opus 5.5 คืน 400 ถ้าส่ง `temperature`, `top_p`, `top_k`, `budget_tokens` หรือบังคับ tool choice ต้องตั้ง `max_tokens` ให้พอ (32000) และใส่ `cache_control` ที่ system prompt กับข้อความล่าสุด
- Gemini 4 Argon รับ effort `low`, `medium`, `high` ถ้าส่ง thinking budget จะได้ 400 context ยาว 1,048,576 token

**การให้คะแนน** (`docs/rules.md`)
- งานแบบ constraint + objective ให้คะแนนต่อชิ้นด้วยสูตร `s = 0.4·p + 0.6·G·O'`
  - `p` คือสัดส่วน constraint ที่ผ่าน
  - `G = 0` ถ้าพลาด hard constraint แม้ข้อเดียว (เพดานคะแนนเหลือ 0.4)
  - `O'` คือผลด้าน objective เทียบกับ baseline ที่ไม่ได้ optimize
- งาน rubric ใช้ LLM judge (Claude Opus 5.5) อ่านไฟล์งานส่งคู่กับ `final_message`
- ห้าม hard-code คำตอบหรือแยกทางตาม task เด็ดขาด ผู้จัดจะสุ่มงาน dev มารันซ้ำจริง

## 2. ลักษณะงาน

**Dev 29 งาน:** rubric 14 (ในนี้มี 12 งานที่อยู่ใน "world" ซึ่งมีแอปจำลองให้ใช้ผ่าน MCP), checks 12, checks-judge 3 และมี 14 งานที่ grader อ่าน `final_message` ด้วย

**Test ลับ 56 งาน:** Mechanical 23, Electrical 18, Civil 9, Structural 6 (ME กับ EE รวมกันเป็น 73%)

รูปแบบงานส่งที่พบในงาน dev:

| รูปแบบ | ตัวอย่าง path | จุดที่ต้องระวัง |
|---|---|---|
| แพ็กเป็น zip | `/filesystem/<x>/output/submission.zip` | โครงสร้างภายใน zip ต้องตรงตามที่โจทย์สั่ง |
| เรขาคณิต | `.../model.ifc`, `.../output/solution.py` | ต้อง parse ได้และไม่มีชิ้นส่วนชนกัน |
| World + แอป | `/filesystem/`, `/app/output/`, `/app/tools/.apps_data/` | **ตรวจทั้งโฟลเดอร์และสถานะในแอป** ต้องทำ action จริง (ส่งอีเมล ปิดเคส เปิด work package) |
| เอกสาร | `/app/files/` | judge อ่าน docx, xlsx, pdf, md ได้โดยตรง |

## 3. World Model: สิ่งที่รู้ว่าจริงตอนนี้

บทสนทนากับโมเดลไม่ใช่บันทึกหลัก ข้อมูลทุกอย่างที่ขั้นตอนหลังต้องใช้ให้เก็บใน World (crate `volund-world`)

**ชั้นที่ 1: project state** (Rust typed, serde)
- `deliverables`: path, format, เนื้อหาที่ต้องมี, สถานะ (Missing → Draft → Written → Checked)
- `params`: ค่า, หน่วย, แหล่งที่มา (`SourceRef`: ไฟล์, หน้า, clause/cell/callout)
- `constraints`: hard หรือ soft, แหล่งที่มา, ค่าที่คำนวณได้, verdict
- `actions`: action ในแอป (Pending → Done → **Verified** ซึ่งต้องยืนยันด้วยการอ่านกลับผ่าน MCP)
- `model`, `checks`, `issues`, `assumptions`, `files`

ฟังก์ชันหลักที่ใช้ตัดสินใจ:
- `brief()`: render World เป็นข้อความใส่ prompt ทุก phase ตอนย่อ context ก็ render ใหม่จากตรงนี้ ไม่ต้องสรุปบทสนทนาเก่า
- `finish_blockers()`: ด่านก่อนจบ บอกว่ายังขาดอะไร
- `quality()`: ค่าประมาณรูปของ grader `checks · (0.4·p + 0.6·g)` ใช้ตัดสินว่าการแก้งานรอบนั้นเก็บไว้หรือ rollback (ตัด `O'` ออก เพราะต้องรู้ bound ของ task ซึ่ง harness ไม่มีทางรู้)

**ชั้นที่ 2: executable model** (แนว Code World Model) อยู่ใน task container ที่ `/tmp/volund/model/`
- `model.py`: โมเดลคำนวณของโปรเจกต์ เช่น load → shear, ขนาดสาย → voltage drop และต้นทุน
- `tests.py`: assert เทียบกับค่าที่เอกสารให้มา เช่น ผลรวมใน schedule หรือ load ที่กำหนด บรรทัดสุดท้ายพิมพ์ `{"total": N, "passed": M}` **harness เป็นคนรันเทสต์เอง** ไม่เชื่อคำบอกของโมเดล
- `solve.py`: ไล่หรือค้นหาทางเลือก ผ่าน hard constraint ทุกข้อก่อน แล้วค่อยลด objective

**กติกาเมื่อข้อมูลขัดกัน:** ไฟล์งานส่งชนะเสมอ เพราะเป็นสิ่งที่ถูกตรวจ ให้ re-derive World จากไฟล์ ห้ามทำกลับทาง

**ใครเขียนอะไรใน World:**
- harness เขียนส่วนที่หาได้แบบ deterministic: `files` (จาก PROBE), `model` (รันเทสต์เอง), `checks`
- โมเดลเขียนผ่าน tool ได้แก่ `world_deliverable`, `world_param`, `world_constraint`, `world_verdict`, `world_action`, `world_assumption`, `world_issue`

## 4. Phase State Machine: โค้ดเป็นคนตัดสินว่าจะไปขั้นไหนต่อ

```text
PROBE → INTAKE → MODEL → SOLVE → WRITE → CHECK ─ผ่าน→ VERIFY ─ผ่าน→ FINALIZE → DONE
                   ↑                      │ไม่ผ่าน         │มีช่องโหว่
                   └──────── FIX ←────────┴──────────────┘   (snapshot → Δ≥0 commit / rollback, ≤ N รอบ)
ทุก phase: cancel หรือถึงเวลา wrap-up → FINALIZE
```

`volund_fsm::next(phase, event, &world, &progress) -> Transition { to, reason, effects }` เป็น pure function จึงเทสต์ได้โดยไม่ต้องมีโมเดล (มีเทสต์ 10 ข้อ รวมข้อที่ยืนยันว่า "งบหมดแล้วต้องจบงานได้เสมอ")

| Phase | ใช้ LLM | งาน | tool | ออกเมื่อ (guard) | งบเริ่มต้น |
|---|---|---|---|---|---|
| PROBE | ไม่ | สำรวจไฟล์, เครื่องมือใน container (python, library, pdftoppm, libreoffice), MCP `initialize` + `tools/list` | — | เสร็จ | ≤ 2 นาที |
| INTAKE | ใช่ | อ่านโจทย์และไฟล์ บันทึกลง World | อ่านอย่างเดียว + `world_*` | ทุก deliverable มี path | 10% |
| MODEL | ใช่ | เขียน model.py และ tests.py | bash, ไฟล์, `world_*` | เทสต์ผ่านครบ (harness รันเอง) | 20% |
| SOLVE | ใช่ | solve.py: ผ่าน hard constraint แล้ว optimize | เหมือน MODEL | เสร็จ | 15% |
| WRITE | ใช่ | เขียน skeleton ของงานส่งก่อนแล้วเติม และทำ action ในแอป | ทุกตัว + `mcp_call` | เสร็จ | 20% |
| CHECK | ไม่ | ตรวจ deterministic (ดูด้านล่าง) | — | ส่งต่อตามผล | สั้น |
| VERIFY | ใช่ (context ใหม่) | คำนวณซ้ำด้วยวิธีอื่น ตรวจ action ในแอป รายงาน issue | อ่าน + bash ใน `/tmp/volund/verify` + `world_issue` | `finish_blockers()` ว่าง | 10% |
| FIX | ใช่ | แก้ issue | ทุกตัว | เสร็จ → CHECK | รวม 15% |
| FINALIZE | ไม่ | สร้าง `final_message` จาก World แล้วส่ง `done` | — | ส่ง `done` แล้ว | กันไว้ 4 นาที |

ตัวเลขงบเป็นค่าตั้งต้น จะปรับตามผลวัดใน M5

**กติกาที่ใช้ทุก phase**
1. ได้ `cancel` หรือเวลาเหลือ ≤ wrap-up → ไป FINALIZE ซึ่งสรุปจาก World ได้โดยไม่ต้องเรียก LLM
2. งบของ phase หมด → ไปข้างหน้าเสมอ ห้ามค้าง เช่น MODEL ที่เทสต์ยังไม่ผ่านจะไป WRITE ด้วยสิ่งที่ใช้ได้
3. Δ≥0 commit gate: ก่อนเข้า FIX ให้ snapshot ไฟล์งานส่งและ World ถ้าหลังแก้แล้ว `quality()` ลดลง ให้ rollback แล้วจบงาน (ไม่เสี่ยงทำของที่ดีอยู่แล้วให้พัง)
4. INTAKE และ MODEL ข้ามได้ตามสิ่งที่บันทึกใน World (ไม่มี constraint ก็ข้าม MODEL/SOLVE ได้) **ห้ามแยกทางตาม task id, ชื่อ หรือ path ของงาน dev**

**CHECK (deterministic)**
- ทุก path ที่ต้องส่งมีอยู่จริงและไม่ว่าง
- เปิดได้ตาม format: zip (list entry ได้), docx/xlsx/pptx, pdf (header), ifc (header ISO-10303-21 และ parse ได้ถ้ามี library), json/yaml/csv parse ได้
- ไม่มีไฟล์ทดค้างในโฟลเดอร์ที่ถูกตรวจ (`/filesystem/`, `/app/files/`, `/app/output/`) **ไฟล์ทดทั้งหมดอยู่ใน `/tmp/volund/` เท่านั้น**
- ไฟล์งานส่งไม่เปลี่ยนระหว่าง VERIFY (เทียบ hash ก่อนกับหลัง)

## 5. หลักการ 14 ข้อ (ใช้ตรวจทุกการเปลี่ยนแปลง)

**วิศวกรรม**
1. ผ่าน hard constraint ให้ครบก่อน แล้วค่อย optimize
2. ตัวเลขทุกตัวได้จากโค้ดและมีหน่วยกำกับ เก็บ script ไว้เป็นหลักฐาน
3. อ่านแบบด้วยการดูภาพด้วย (render แล้วซูม) ไม่พึ่งแค่ text extraction
4. งาน world ต้องทำ action จริงในแอปและยืนยันด้วยการอ่านกลับ

**ความถูกต้อง**

5. World เป็นแหล่งความจริงเดียวของสิ่งที่ต้องทำ ส่วนไฟล์เป็นแหล่งความจริงของผลลัพธ์
6. ตรวจแยกเป็น 2 ชั้น: deterministic (CHECK) แล้วตามด้วย context ใหม่ (VERIFY) ห้ามให้โมเดลตรวจงานตัวเองใน context เดียวกัน
7. ด่านก่อนจบ (`finish_blockers`) กันการหยุดก่อนเวลา

**การส่งงาน**

8. ต้องมีงานส่งตลอดเวลา: เขียน skeleton ตั้งแต่ต้น และ `done` ต้องไม่ว่างแม้โดน cancel
9. `final_message` เป็นส่วนหนึ่งของงาน ต้องมีไฟล์และ path, ผลหลัก, code และมาตรฐานที่ใช้, สมมติฐาน, ข้อจำกัด

**ต้นทุน**

10. ปรับต้นทุนได้จาก effort, จำนวนรอบ, งบต่อ phase และ best-of-N (ใช้โมเดลถูกช่วยไม่ได้)
11. ประหยัด context: PROBE ทำโดยไม่ใช้ LLM, ตัด output เก็บหัวท้าย, render ใหม่จาก World, ใส่ cache mark
12. คิดอันดับแบบ Pareto: ทีมคะแนนสูงสุดอยู่บนเส้นเสมอ ทีมอื่นต้องถูกกว่าทุกทีมที่คะแนนสูงกว่าตัวเอง

**การแข่งและการทดลอง**

13. ทำให้ใช้ได้ทั่วไป ห้ามผูกกับงาน playbook แยกตามประเภทงานส่ง ไม่ใช่ตาม task
14. วัดผลทุกครั้งและเปลี่ยนทีละอย่าง จดสิ่งที่ไม่ได้ผลไว้ด้วย (เป็นเนื้อหาของ writeup)

## 6. สถาปัตยกรรม

```text
evaluator ─ws─► volund (serve container: 2 CPU / 4 GB)
  volund-proto   type ของ protocol v1                                         [มีแล้ว]
  server         WS accept, จับคู่ request id, Semaphore(8), cancel, deadline  [M1]
  llm            OpenAI-compatible streaming, เก็บข้อความเป็น JSON ดิบ, retry   [M1]
  mcp            JSON-RPC ผ่าน `http` ของ protocol, SSE, index ของ catalog       [M2]
  volund-world   World Model                                                  [มีแล้ว]
  volund-fsm     Phase State Machine                                          [มีแล้ว]
  driver         รัน phase, ทำ effect, render prompt, log                     [M2]
  tools          bash, read/write/edit, view_page, mcp_find/mcp_call, world_*, finish
```

**LLM client (จุดที่ Rust พลาดง่าย)**
- **เก็บข้อความ assistant เป็น `serde_json::Value`** ห้าม map เข้า struct เพราะ field ที่ไม่รู้จัก เช่น thought signature ของ Gemini หรือ thinking block ของ Claude จะหาย แล้วรอบถัดไปจะได้ 400 หรือเหตุผลขาดตอน
- stream ทุก call เพราะ proxy ตัดการเชื่อมต่อที่ไม่มีข้อมูลวิ่งนาน 30 นาที ไม่ตั้ง total timeout แต่ตั้ง idle timeout ให้ยาว
- retry เมื่อเจอ 429, 5xx, timeout, stream ขาด หรือไม่มี finish reason แบบ backoff 5 วินาทีถึง 2 นาที และทิ้ง response ที่มาไม่ครบ **ห้าม retry 400 ด้วย request เดิม**
- ส่ง `reasoning_effort` เองทุกครั้ง ใช้ `tool_choice: auto` เท่านั้น และส่ง `usage` frame ทุก call
- `reqwest` อ่าน `HTTPS_PROXY` จาก env ให้เอง ใช้ rustls

**MCP:** catalog อาจมีหลายร้อย tool (~500 KB) ถ้ามี tool ไม่เกิน 128 ตัวและ schema ไม่เกิน 40,000 ตัวอักษร ให้ส่งเป็น function ตรงๆ ถ้าใหญ่กว่านั้นให้ใช้ `mcp_find` และ `mcp_call` โดยใช้ index ที่สร้างไว้ตอน PROBE

**การดูแบบ (`view_page`):** render ใน task container ด้วย `pdftoppm` ถ้ามี ถ้าไม่มีให้ดึงไฟล์มา render ใน serve container ด้วย pdfium (BSD) **ห้ามใช้ MuPDF หรือ PyMuPDF เพราะเป็น AGPL** เปิดใช้เมื่อ gateway รับรูปภาพสำหรับโมเดลที่เลือก (ต้องวัดก่อน ดู §11)

**Context**
- system prompt คงที่ (cache ได้) + `World::brief()` + คำสั่งของ phase + บทสนทนาล่าสุด
- output ของ tool ตัดไว้ราว 30,000 ตัวอักษร และเก็บฉบับเต็มไว้ที่ `/tmp/volund/out/` ใน container
- เมื่อ prompt ใกล้เต็ม ให้ตัดบทสนทนาเก่าแล้ว render brief ใหม่

## 7. Build และแพ็กเกจ

ไฟล์จริงคือ [`submission.yaml`](submission.yaml) (ผ่าน `kit validate` แล้ว)

- ติดตั้ง toolchain และ target ไว้ใน `/tmp` เพื่อไม่ให้ไปปนใน bundle ใต้ `/opt/harness` แล้ว copy มาเฉพาะ binary
- ตั้ง `RUSTUP_TOOLCHAIN=1.98.1` เพื่อข้าม `rust-toolchain.toml` ไม่ให้โหลด rustfmt กับ clippy ที่ build ไม่ได้ใช้
- ตั้ง `CARGO_BUILD_JOBS=2` เพราะเครื่องทดสอบมีงานอื่นรันใน Docker ด้วย ถ้าไม่จำกัด build จะกิน CPU ทุก core
- TLS ใช้ rustls คู่กับ ring ซึ่ง build ด้วย C compiler อย่างเดียวได้ (build image ไม่มี cmake) และอ่าน root certificate จากระบบของ serve image
- `serve.py` กับ `harness.py` (หน้า Kaggle ระบุว่าต้องมี) แค่ `exec` ตัว binary
- บน Mac ตัว kit build เป็น `linux/amd64` แบบ emulation ซึ่งช้า ต้องจับเวลา build ครั้งแรก และต้องเสร็จใน 30 นาทีบนเครื่องผู้จัด
- license ของ dependency: อนุญาตเฉพาะ MIT, Apache-2.0, BSD, ISC, Zlib, Unicode ตรวจ 16 พ.ย. ด้วย `cargo metadata` แล้วผ่านครบทั้ง 114 crate ใน binary ส่วน `cargo-deny` จะเพิ่มทีหลังเพื่อตรวจซ้ำอัตโนมัติ

## 8. ต้นทุน

- ตัวที่ปรับได้: `reasoning_effort`, `max_fix_rounds`, งบต่อ phase, best-of-N ใน SOLVE (เลือกผลด้วย `quality()`)
- วัด token แยกตาม phase ทุก run เพื่อหาว่าคะแนนตัน (plateau) ตรงไหน (Figure 2 ของ writeup)
- อันดับสุดท้ายดูจากคะแนน Stage 2 แต่ต้องอยู่บนเส้น Pareto ระหว่างคะแนนกับต้นทุน และคะแนนเฉลี่ยต้องไม่ต่ำกว่า 0.138

## 9. แผน eval

**ขั้น A: Mac (เพื่อพัฒนา)**
- Docker ได้ 6 CPU / 16 GB ซึ่งพอสำหรับ 1 ช่อง เพราะ `eval/run.py` จองช่องละ 6 CPU / 16.25 GB จึงรันได้ทีละงาน
- รัน `kit check` ให้ครบทุกขั้น (build, listen, smoke, mcp, `--cancel-test`)
- รันงานที่ไม่ใช่ world ทีละงาน ด้วย `eval/run.py <entry> --tasks <id,...>`
- เก็บ baseline โดยรัน sample `custom-openai` ของผู้จัดกับ 3–4 งาน
- ข้อจำกัด: image ของงาน world ใหญ่ราว 17 GB ต่อตัว (มี 12 งาน) แต่ดิสก์ในเครื่องว่างแค่ราว 55 GB และรัน amd64 แบบ emulation ทำให้ช้า และ verifier ที่จับเวลาให้คะแนนต่ำลง **ตัวเลขจาก Mac ใช้พัฒนาเท่านั้น ห้ามใช้เป็นตัวเลขที่ส่ง**

**ขั้น B: Linux VM แบบ amd64 (ตัวเลขจริง)**
- ใช้สำหรับงาน world, การรันเต็ม 29 งาน, การเทียบโมเดล และการสร้าง `artifacts/` ที่จะส่ง
- ขนาดที่ใช้ได้: ≥ 16 vCPU / 64 GB / ดิสก์ 300 GB ได้ 2 ช่อง
- **ต้องตั้งงบค่า VM และค่าโมเดลก่อน run ที่เสียเงินครั้งแรก** การเรียกโมเดลผ่าน `kit/model_proxy` คิดเงินจากโปรเจกต์ GCP ของเรา ทั้งค่า harness และค่า judge
- judge: ค่าเริ่มต้นคือ `claude-opus-5-5` ผ่าน proxy ถ้าใช้ judge ตัวอื่นต้องระบุใน writeup

**Log:** ทุก `log` frame เป็น JSON หนึ่งบรรทัด `{"t":…, "phase":…, "kind":"transition|tool|llm|check|world", …}` อ่านได้จาก `runs/<run>/attempt1/jobs/<task>/agent/` แล้วรวมเป็น CSV และกราฟสำหรับ writeup

**ป้ายสาเหตุที่พลาด (Figure 3):**

| ป้าย | ความหมาย |
|---|---|
| READ_DRAWING | อ่านแบบหรือ callout ผิด |
| WRONG_CLAUSE | ใช้ clause หรือตาราง code ผิด |
| UNIT | หน่วยหรือการแปลงหน่วยผิด |
| HALLUCINATED_INPUT | ใช้ค่าที่ไม่มีในไฟล์ |
| HARD_CONSTRAINT | ผิด hard constraint |
| NOT_OPTIMIZED | ผ่าน constraint แต่ `O'` ต่ำ |
| DELIVERABLE_FORMAT | path, format หรือการแพ็กผิด |
| MISSED_ACTION | ไม่ได้ทำ action ในแอป |
| STOPPED_EARLY | จบก่อนงานครบ |
| TIMEOUT_CRASH | harness ล่มหรือหมดเวลา |

**Ablation** (ทุกข้อรันกับงานชุดเดียวกัน)
- A0: sample `custom-openai` (baseline)
- A1: Völund แบบ loop เดียว (ให้ได้เท่า A0)
- A2: + PROBE
- A3: + World และการ render ใหม่จาก World
- A4: + FSM และด่านก่อนจบ
- A5: + MODEL/SOLVE (executable model)
- A6: + VERIFY และ Δ≥0 commit
- A7: + `view_page`
- Scaling: effort low/medium/high × `max_fix_rounds` 0–3 × best-of-N
- โมเดล: ตัวที่จะส่ง + อย่างน้อยอีก 1 ตระกูล

## 10. Milestones

| รหัส | ช่วงเวลา | งาน |
|---|---|---|
| M0 | 15–17 พ.ย. | โครง repo (เสร็จแล้ว), ขอสิทธิ์ dataset บน HF, ตั้งโปรเจกต์ GCP และ `kit/model_proxy`, ทีมอ่านโจทย์ dev 3–4 งาน |
| M1 | 18–24 พ.ย. | server + LLM client + loop เดียว, `submission.yaml`, `serve.py`, `kit check` ผ่านบน Mac, baseline A0 |
| M2 | 25 พ.ย.–1 ธ.ค. | driver + World + FSM + tool + MCP, รันเต็มชุดครั้งแรกบน VM, ติดป้ายสาเหตุที่พลาด |
| M3 | 2–8 ธ.ค. | MODEL/SOLVE, CHECK, VERIFY, ด่านก่อนจบ, เทียบ 2 โมเดลขึ้นไปแล้ว**เลือกโมเดล** |
| M4 | 9–15 ธ.ค. | `view_page`, ตัวตรวจการแพ็ก, ขั้น optimize, ablation |
| M5 | 16–22 ธ.ค. | เส้นกราฟคะแนนกับต้นทุน, ทดสอบ cancel และ retry, ปรับงบต่อ phase |
| M6 | 23–29 ธ.ค. | หยุดเพิ่มฟีเจอร์, รันเต็มชุด 2 รอบด้วยโมเดลที่จะส่ง, `artifacts/` |
| M7 | 30 ธ.ค.–4 ม.ค. | writeup ≤ 2,000 คำ + 3 รูป + ภาพปก, `kit check` ได้ `ready to submit`, ส่ง |

**สถานะ 16 พ.ย.:** ส่วนของ M1 ที่ไม่ต้องใช้โมเดลจริงเสร็จแล้ว

- `kit check --cancel-test` บน Mac ผ่านทุกขั้น โดยใช้ mock model แทน proxy
  - build ใช้เวลา 173 วินาที (amd64 แบบ emulation, 2 job)
  - smoke และ cancel ผ่าน
  - `mcp` ขึ้น warning จนกว่า M2 จะทำ MCP
  - `artifacts` ขึ้น warning เพราะยังไม่ได้รัน eval

ส่วนที่เหลือของ M1 คือ baseline A0 และการรันกับโมเดลจริง ซึ่งรอโปรเจกต์ GCP ที่เปิด Vertex AI ไว้

**สถานะ 16 พ.ย. (บ่าย):** โค้ด M2 เสร็จแล้ว

- `volund-mcp` คุยกับแอปของโจทย์ผ่านคำขอ `http` ได้
- driver รันครบทุก phase ตาม §4
  - PROBE และ CHECK ใช้โค้ดล้วน
  - VERIFY เริ่ม context ใหม่
  - FIX มี snapshot และ rollback
  - กลับเข้า phase เดิมได้ไม่เกิน 3 ครั้ง
- loop เดียวยังเลือกใช้ได้ด้วย `VOLUND_AGENT=loop` (A1)
- `kit check --cancel-test` กับ driver ผ่านทุกขั้น รวม `mcp`: build 181 วินาที, smoke ใช้ 4 request, mcp ใช้ 7 request
- เทสต์ 82 ข้อ

ส่วนที่เหลือของ M2 คือการรันเต็มชุดบน VM และการติดป้ายสาเหตุที่พลาด ซึ่งต้องใช้โมเดลจริง

## 11. สิ่งที่ยังไม่รู้ (ต้องวัดใน M0–M1)

1. ใน task container มี python, library อะไรบ้าง, pdftoppm, libreoffice และ `/tmp` เขียนได้หรือไม่ (วัดด้วย PROBE ใน dev task)
2. gateway รับรูปภาพสำหรับโมเดลที่จะใช้หรือไม่ (ถ้าไม่รับ หลักข้อ 3 ต้องเปลี่ยนวิธี)
3. ค่าใช้จ่ายของ 1 งานต่อโมเดล
4. ~~เวลา build Rust ใน kit (amd64)~~ วัดแล้ว 173 วินาทีบน Mac แบบ emulation 2 job ส่วนบนเครื่องผู้จัดที่เป็น amd64 จริงน่าจะเร็วกว่านี้ (16 พ.ย.)
5. โครงสร้าง `submission.zip` ที่แต่ละงานต้องการ (อ่านจากโจทย์ ห้ามอ่านจาก grader)

## 12. บทบาทในทีม (ให้ทีมตกลงชื่อผู้รับผิดชอบ)

| สาย | งาน |
|---|---|
| Core (Rust) | server, LLM client, driver, MCP |
| Tools และเอกสาร | PROBE, CHECK, `view_page`, ตัวตรวจการแพ็ก |
| วิธีทางวิศวกรรม | playbook ตามประเภทงานส่ง, checklist ของ VERIFY, ไล่ดูงานที่พลาดด้วยสายตาวิศวกร (ต้องมีพื้น EE หรือ ME) |
| Eval และ writeup | รัน eval, ติดป้าย, ablation, รูป, เขียน writeup |

## 13. Fair play

- ห้ามอ่านหรือใช้ grader และไฟล์อ้างอิงของงาน dev และห้ามให้ harness อ่าน `artifacts/` ของตัวเอง
- `artifacts/` ต้องมาจาก `eval/run.py` ด้วย harness และ `model_api` ที่ส่งจริง ห้ามแก้ด้วยมือ
- ห้ามติดต่อ host อื่นนอกจาก model endpoint และห้ามเก็บหรือเผยแพร่ prompt หรือไฟล์ของงาน test
- `model_api` ต้องตรงกับโมเดลที่เรียกจริง

## 14. อ้างอิง

- `google-research/ple` commit `8320f04` ได้แก่ README, `docs/protocol.md`, `docs/rules.md`, `docs/models.md`, `kit/README.md`, `eval/README.md`, `eval/dev_set.yaml` และ sample `minimal`, `plan-verify`, `custom-openai`
- `katopz/katgpt-rs` commit `b14cbaa` (MIT) หยิบมา**เฉพาะแนวคิด** ไม่ได้ใช้โค้ด:
  - Deterministic Validator / ConstraintPruner (LLM เสนอ โค้ดตัดตัวที่ผิดทิ้ง)
  - Induced Code World Model
  - Δ≥0 commit gate (Step Attribution Qualifier)
  - Claim evidence ladder
  - GOAT gate และตาราง "What Didn't Work"
  - กฎ FSM ที่ห้ามเปลี่ยน state ด้วย threshold เดียวแบบแข็งๆ
- *Code World Models for General Game Playing*, Lehrach et al. (Google DeepMind), arXiv:2510.04542 (ตามที่ KatGPT-RS research note 275 สรุปไว้ ทีมควรอ่านฉบับเต็มก่อนนำไปอ้างใน writeup)
