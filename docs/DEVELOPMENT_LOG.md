# Development log

Reverse chronological. Each entry: what was done, evidence, blockers, next action.

---

## 2026-11-24 — GTA VI live: no hang, but the title's backbuffer is empty

- **Runs.** Rebuilt `d3d12_xs` with the descriptor translation, the literal sampler bank and a
  `PresentX.draws` diagnostic. Ran the original `game_scarlett_master.exe` for 120 s and 250 s
  (`mdbg`, 3840x2160, offline). Both runs ended on the time limit with **no GPU hang, no device
  removal and no unhandled exception**.
- **Descriptor ceiling, exercised by the title.** It creates the 1,245,184-slot heap and at t≈7 s
  writes console pages 976+ (descriptors ≥ 999,424). The log shows page 976 → host page 358,
  977 → 357, … 1004 → 330: real remapping, not a clamp. No console address reached the driver and
  no table needed the null fallback in this run. 600 and 1500 frames were presented.
- **The black screen is now measured, not assumed.** GPU readback (`TOMBOX_PRESENT_SNAP`): the
  title's presentation texture is **0 of 8,294,400 pixels non-black at frames 1, 10, 30, 60, 400 and
  1500** (one distinct row). The presented swapchain holds only the emulator's FPS overlay
  (~0.3%, 13,611–30,950 pixels); `PresentX.draws` reports `draws=0 draws_into_presented=0` for the
  first present. The title presents a texture it never draws into — the copy to the swapchain is
  correct, the source is empty.
- **Translator blocker.** Two shaders fail: `translate: constant-buffer MUBUF form unsupported
  [buffer_load_format_xy @ 0x60]` (hash `aa564c879df9c2c3`) and the same at 0x1c0 (hash
  `eac983a93adb437b`). A format load takes its DFMT/NFMT from the V#; a raw PC root CBV has no
  format, so the binding has to carry it (or the shader needs a typed buffer view).
- **PSO blocker.** 31 native stream PSOs are rejected by the driver with `E_INVALIDARG`
  (`subs=[… 15:0x18/0x1a …]`). In 250 s the title also compiled 25,528 PSOs while hitting 25,487
  cached ones, so its pipeline set is still far from warm.
- **Next.** Fix the format-load-on-CBV binding, then re-measure `draws_into_presented`; separately
  decode one rejected stream PSO and compare it with the same stream compiled by the cached path.

---

## 2026-11-24 — GTA VI: descriptors past the host heap ceiling, literal sampler bank

- **Descriptor ceiling.** GTA VI creates one shader-visible CBV/SRV/UAV heap with 1,245,184
  descriptors. A PC D3D12 runtime refuses a shader-visible heap above 1,000,000
  (`D3D12_MAX_SHADER_VISIBLE_DESCRIPTOR_HEAP_SIZE_TIER_2`). Until now the heap was retried at the
  ceiling and the title kept addressing the missing tail, so a console handle at or above the PC
  size was sent to the driver untranslated — the invalid-descriptor path that ended in the GPU hang.
- **Fix.** `title_heap` now keeps the title's slot count (`console`) separate from the PC heap's
  (`host`), recognises the full console range, and maps console pages past the ceiling onto host
  pages the title never wrote (page size 1024, no aliasing: a page given away is no longer the
  identity page of its own console slots). A table that cannot be hosted as one contiguous range
  binds a reserved null descriptor at the end of the host heap and increments
  `UNREPRESENTABLE_TABLES`; a console address is never handed to the driver. `HOST_DESCRIPTOR_LIMIT`
  is the single definition of the ceiling (the heap-retry code uses it too).
- **Evidence.** `cargo test -p d3d12_xs --lib` → 714 passed, 1 failed (the console-DLL oracle test,
  which needs `D:\xbox-pc-port\games`; not present on this host). Nine new tests in
  `title_heap::descriptor_capacity_tests` cover identity below the ceiling, a real host slot above it,
  no host slot outside the heap, non-aliasing, table contiguity and the null table.
- **Literal samplers.** A shader that builds its S# in SGPRs has no sampler in the title's root
  signature. Every title root signature now carries a bank of 116 static samplers in register space
  70000, and the translator binds the entry that decodes to the shader's S#. `cargo test -p
  tombox-xshader` unit tests (124) and the DXC integration test `literal_sampler` pass; unrepresented
  S# combinations are rejected, not guessed (`TOMBOX_LITSAMP_LOG` records them).
- **Blocker.** Direct heap indexing (`ResourceDescriptorHeap[tb_bi/32]`) still carries the console
  index, so a descriptor *above* the ceiling that a shader indexes directly needs a shader-side
  remap (root CBV page table). The host-side translation and the safety gate are in place; the
  shader ABI change is next.

---

## 2026-11-21 — past "Checking for updates": login complete, game-mode select

- Backend body logging showed the client requests `grant_type=client_credentials&token_type=eg1`;
  the local backend now issues `eg1~<JWT>` tokens with real `exp`/`iat`.
- Still stalled. WinHTTP tracing (ReceiveResponse / QueryHeaders / ReadData) showed zero reads:
  the server answered every request but the title never consumed a response. Cause: the
  emulator's `WinHttpOpen` hook cleared `0x30000000` (WINHTTP_FLAG_SECURE_DEFAULTS), which
  includes WINHTTP_FLAG_ASYNC (`0x10000000`), so the session ran synchronously and the
  title's completion callbacks never fired. Fix: clear only the secure-defaults bit, keep ASYNC.
- Run 15 (log evidence): 59 ReceiveResponse / 620 ReadData calls; `HandleLoginRequestComplete
  [Result=2]`; `CheckComplete UpdateSuccess`; `UI State changing from [UI.State.Startup.Login]
  to [UI.State.Startup.SubgameSelect]`. 43 distinct backend endpoints called (6 answered 404).
- Next: in-process service emulation inside the emulator (replacing the stand-in Node server)
  and getting from game-mode select into the Battle Royale lobby without mouse input.

---

## 2026-11-21 — local backend, stuck at "Checking for updates"

- Offline, login fails (`Unable to join the Fortnite login queue`). Added a local-backend mode
  to `net_guard`: `TOMBOX_NET_REDIRECT=127.0.0.1` + `TOMBOX_NET_LOCAL_PORT=3551` sends every
  WinHTTP connection to a local LawinServer, clears `WINHTTP_FLAG_SECURE` per request and the
  secure-defaults session flags (UE's session otherwise refuses plain HTTP, error 5). No traffic
  leaves the machine.
- Result: waiting room and splash screen pass; `POST /account/api/oauth/token` answered 200;
  the screen then sits at "CHECKING FOR UPDATES..." — the hotfix v2/dispatch check starts but
  no cloudstorage request follows.
- Added WinHTTP tracing (OpenRequest path, SendRequest, async REQUEST_ERROR, failed options).
  Datarouter telemetry reaches the backend (204). Xbox Live `userstats` `/batch` never arrives
  (client-cert option fails on plain HTTP) and times out after ~11 s.
- Run 8 crashed in the first callback wrapper (events routed to the wrong owner); fixed with
  per-handle parent tracking. Run 9 showed that reporting the client-cert option as successful
  makes the stats request hang instead of failing; reverted.

---

## 2026-11-21 — first boots on this machine

**Done**
- Decrypted archive.org ZIP downloaded (27,385,950,541 bytes, exact match to
  the server's content-length) and extracted in 1.3 min.
- Dump kept pristine. TomBox DLLs no longer go into the game folder: a run
  overlay at `D:\TomBox\run\Mount` mirrors the dump with junctions and
  hardlinks; only `Binaries\XSX` in the overlay gets the runtime DLLs. `G:` is
  `subst`ed to the overlay.
- Run 1: process started, GDK runtime services and the xmem allocator came up,
  then access violation at `EXE+0xa45d63`. Last compat-log lines:
  `[xcrdapi] DStorageGetFactory [real dstorage.dll not available -> E_NOTIMPL]`.
  TomBox forwards Xbox file streaming to PC DirectStorage; the machine had none,
  so the title's I/O source buffer was null (`[r12+0x18] = 0`).
- Fix: Microsoft.Direct3D.DirectStorage 1.3.0 from NuGet, x64 DLLs
  (Authenticode: Microsoft Corporation) staged in the overlay.
- Run 2: `DStorageGetFactory forwarded (PC IID) -> hr=0`. Engine init ran 11.5 s
  / 14,440 log lines, then `MFCreateDXGIDeviceManager failed: 0x80004005` and a
  fatal error. Cause: TomBox's mfplat forwarder loads the real Media Foundation
  from `mfplat_sys.dll` next to itself; it was not staged.
- Fix: system `mfplat.dll` copied into the overlay as `mfplat_sys.dll`.

- Run 3: Media Foundation forwarded. **First frames on this machine: 2,400+
  PresentX frames at 3840x2160.** Ended by mdbg's default 70 s limit, not a
  crash. Online subsystem errors (`connection_failure`, hotfix enumeration) are
  the network guard working as intended.
- The title creates its own 3840x2160 window, which spanned both monitors
  until the first present resized it. Runtime change in `d3d12_xs`
  (`present::start_window_clamp`): from device creation, a watcher fits the
  title window to the primary monitor (16:9) whenever it extends past it.
- Host window title renamed to "Xbox Emulator"; project renamed Xbox Emulator
  (repo `omonkeyz/xbox-emulator`).
- `MDBG_LIMIT=86400` in the launcher so runs last until the window is closed.

**Correction**
- An earlier note blamed a `prot=0x1->0x1` allocation. That call is
  reserve-only (`win_type=0x102000`), so NOACCESS is correct. Not a bug.

---

## 2026-11-21 — TomBox staged, Fortnite dump downloading

**Done**
- Cloned `drycheetah84/TomBox` → `D:\TomBox`.
- Baseline `cargo build --release` passed in 29 s.
- Built the six static-CRT compat DLLs (`d3d12_xs`, `xmem`, `xcrdapi`, `acphal`,
  `dxcompiler_xs`, `mfplat`) with `RUSTFLAGS=-C target-feature=+crt-static`.
  Sizes: d3d12_xs 1.85 MB, acphal 705 KB, xcrdapi 724 KB, mfplat 698 KB,
  dxcompiler_xs 689 KB, xmem 702 KB.
- Built `mdbg.exe` tracer (229 KB) from `tools/dev/mdbg.rs`.
- Wrote `D:\TomBox\run_fortnite.ps1` — extract + stage DLLs + `subst G:` + run
  under `mdbg.exe` with log summary.
- Discovered first attempt at the Fortnite dump was a 27 GB `.xvc` (encrypted
  retail container). Verified — first 64 bytes are high-entropy with no magic,
  consistent with AES ciphertext. Deleted. Downloading the archive.org
  decrypted ZIP (27.4 GB) instead. URL verified live (HTTP 200).

**Evidence**
- `.xvc` bytes 0..64 are random; a valid ZIP would start with `50 4b 03 04`.
- Microsoft Gaming Services not installed on this machine. Required for
  `xgameruntime.dll`.

**Blockers**
- Dump download in progress.
- Gaming Services install needed before first run.

**Next**
1. `run_fortnite.ps1 -Extract` once ZIP arrives.
2. `run_fortnite.ps1 -Run` — capture baseline log.
3. Decide on first contribution: keyboard input is the smallest high-value PR
   (upstream README lists input as "partial (mouse)").
