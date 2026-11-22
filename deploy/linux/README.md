# pTerminal บน Linux — launch profiles + ย้ายน้องๆ จาก Ghostty

สรุปงาน branch `launch-profiles` (ร่างโดย Zero, 2026-11-22). เป้าหมาย: ให้น้องๆ 4 คน
(Zero/Zeed/Zelot/Zeno) รันใน pTerminal และ **ปิด/เปิด pTerminal แล้วกลับมาครบเอง**
โดยแต่ละแท็บถือ "launch profile" ของตัวเอง (channels + `DISCORD_STATE_DIR` + settings)
ติดตัวทุกครั้งที่ spawn/restart/resume — กันไม่ให้น้องไปใช้บอทพี่ซีเรมเด็ดขาด

## สิ่งที่เพิ่มในโค้ด (ย้อนกลับได้, อยู่ branch `launch-profiles`)
- `LaunchProfile { extra_args, env, merge_settings }` ต่อแท็บ agent
  - `extra_args` → ต่อท้าย `--settings <per-tab>` ใน argv ของ claude (เช่น `--channels plugin:discord@...`)
  - `env` → ตั้งบน child process (เช่น `DISCORD_STATE_DIR=...`) ผ่าน PTY (`spawn_with_env`)
  - `merge_settings` → deep-merge ไฟล์ settings ของน้อง (เช่น `zero-settings.json`) **เข้าเป็นไฟล์ `--settings` เดียว** กับ hooks ของ pTerminal (ไม่ส่ง `--settings` 2 ตัวชนกัน; hooks ของ pTerminal ชนะเสมอบน key ที่ซ้ำ)
- persist ลง `state::SavedTab.launch_profile` → resume/respawn ใช้ profile เดิมเป๊ะ (`claude --resume <id>` + args/env เดิม)
- seed แท็บใหม่จาก `~/.config/pterminal/launch-profiles.json` key ด้วย **cwd** → เปิดแท็บ Claude ใน `~/zerem-team/zero` ได้ profile ของ Zero อัตโนมัติ ไม่ต้องมี UI
  - ไฟล์ seed พัง/อ่านไม่ได้ = **spawn ล้มเหลวดังๆ** (ไม่เงียบๆ แล้วปล่อยบอทผิดตัว)

## ⚠️ เครื่องนี้ยังไม่มี C compiler → build ตัวจริงต้องให้เจ้านายลงก่อน
`gcc`/`clang`/`cc` ไม่มีบนเครื่อง (มีแค่ `ld` + `rust-lld`) นี่คือเหตุผลที่ pTerminal
ยังไม่เคย build ที่นี่ การจะออก **rc2 + ติดตั้ง `~/.local/bin`** ต้องมี toolchain ปกติก่อน:

```
sudo apt install build-essential     # gcc + linker (มาตรฐาน, ~30–200MB)
```

(ทีมลงเองไม่ได้ ต้องเจ้านาย) เสร็จแล้ว Zero build ให้ด้วย:
```
cd ~/projects/pTerminal
cargo build --release -j 2            # -j2 กัน OOM; swap 16GB
install -Dm755 target/release/pterminal ~/.local/bin/pterminal
install -Dm755 target/release/pterm_hook ~/.local/bin/pterm_hook
```

> หมายเหตุ verify: Zero ตรวจว่าโค้ดคอมไพล์/เทสต์ผ่านด้วย `cc`-shim ครอบ `rust-lld`
> (verify เท่านั้น ไม่ใช่ไบนารีที่จะ ship) — ดูผลใน report ที่ส่งพี่ซีเรม ตัว rc2 จริง
> ควร build ด้วย gcc ปกติ

## ติดตั้ง launch profiles (ก่อนย้าย)
```
install -Dm644 deploy/linux/launch-profiles.json ~/.config/pterminal/launch-profiles.json
```

## ย้ายน้องทีละคน (พี่ซีเรมทำเอง — Zero ไม่ย้ายตัวจริง)
ทำทีละคน ตรวจให้ชัดก่อนปิดตัวเก่า:
1. เปิดแท็บ **Claude** ใน pTerminal ที่ cwd = `~/zerem-team/<ชื่อ>` (เช่น zero)
   → แท็บจะได้ `--channels` + `DISCORD_STATE_DIR` ของคนนั้นอัตโนมัติจาก launch-profiles.json
2. **ตรวจว่าบอทถูกตัว** ก่อนทำต่อ (ห้ามพิมพ์ token): ให้น้องในแท็บนั้นเช็ค Discord `/users/@me`
   ว่า bot id/username ตรงกับของตัวเอง ไม่ใช่ของพี่ซีเรม
3. ปิด session เดิมใน Ghostty ของคนนั้น
4. ทดสอบ resume: ปิด pTerminal แล้วเปิดใหม่ → แท็บต้องกลับมาเอง (`--resume <id>` + args/env เดิม)
5. ค่อยทำคนถัดไป

## Autostart (หลังย้ายครบ + ทดสอบผ่าน)
- แทนที่ส่วนน้องใน start-all.sh ด้วยไฟล์นี้ (เหลือเฉพาะ Zerem):
  ```
  cp deploy/linux/start-all.sh ~/zerem-team/start-all.sh
  ```
- เพิ่ม autostart ของ pTerminal (มัน restore แท็บน้องเอง):
  ```
  install -Dm644 deploy/linux/pterminal.desktop ~/.config/autostart/pterminal.desktop
  ```
- `~/.config/autostart/zerem-team.desktop` เดิม (รัน start-all.sh) คงไว้ → เปิด Zerem ใน Ghostty
- ตัว `running()` ใน start-all กันซ้ำ: ถ้า pTerminal เปิด bot ใน cwd นั้นแล้ว start-all จะข้าม

## หมายเหตุ prompt auto-start
launch profile ไม่รวม prompt `$NOTE`. pTerminal resume ด้วย `--resume <session-id>`
→ น้อง "ต่อ conversation เดิม" อยู่แล้ว ไม่ต้องยิง NOTE ซ้ำ. เปิดสดครั้งแรก ถ้าอยากฝาก
NOTE ให้พิมพ์ตอนเปิดแท็บได้
