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

## 2026-11-24 — PC pipeline-stream RTV layout: formats-first, not count-first

**Correction.** `D3D12_RT_FORMAT_ARRAY`, the pipeline-stream subobject 15, is
`DXGI_FORMAT RTFormats[8]; UINT NumRenderTargets;` with the count at offset 32.
Only `D3D12_GRAPHICS_PIPELINE_STATE_DESC` is count-first. A speculative attempt
to reorder console subobject 15 into count-first read the first format (28, 87,
…) as a target count and raised PSO rejections from 31 to 50,987 in 130 s. That
reorder and its classifier were reverted; no other changes were involved.

**Evidence for formats-first on this machine**
- `TOMBOX_EXEC_HW=1 cargo test -p d3d12_xs --lib hw_db_count -- --ignored`:
  4 passed. `warp_rectlist`: 1 passed. Both hardware streams write the RTV
  formats first and the count at offset 32, and the driver accepts them.
- In-process minimal reproducer: take a stream the driver accepted and add one
  formats-first RTV (`RTFormats[0]=28`, count=1). Result `hr=0x00000000`, so the
  blend/rasterizer/depth/sample state of working pipelines can carry an RTV.
- With the layout restored: 0 driver rejections in the first 35 s; 196 driver
  rejections plus 20 semantic (`stream rejected with root`) in 150 s, versus
  50,987 in 130 s with the reorder. Backbuffer still 0/8,294,400 non-black.

**Cause of the remaining rejections (isolated, not yet fixed)**
- Stream probe on the correct layout: leaving out the pixel shader succeeds;
  leaving out the RTV does not. The PS dump
  (`%TEMP%`/probe `rejected*_ps.dxil`) has `SV_Target0` float4 and its inputs
  match the VS outputs exactly.
- The PS declares `s0,space70000` (literal sampler bank) and textures at
  `space60000`/`space4160000` (raw-CBV aliases); the stream's 112-byte console
  root signature exposes only one range at space 1, register 27. The shader's
  bindings are outside the root signature, which is E_INVALIDARG at PSO
  creation. `via_real=false` on that root.
- `TOMBOX_CBV_RAW=on` makes both rejection classes disappear: 0 driver and 0
  semantic rejections in 95 s and again in 175 s. That key is cache-cold, so
  the title was still compiling shaders (500 shaders, 1,800 PSOs) at 175 s and
  had not reached presentation; it was progressing, not stalled. `copy` is the
  default and the reason the title presents quickly from its warm cache.

**Checks**
- `cargo test -p d3d12_xs --lib mesh_pso`: 15 passed, including the new
  regression `pc_stream_rtv_is_formats_first_not_console_order`, which fails on
  the reordered classifier.
- No D3D12 SDK debug layer is installed (`C:\Windows\System32\d3d12SDKLayers.dll`
  absent), so the driver's own E_INVALIDARG text is unavailable.

**Next**
1. Prewarm the `cbraw=on` shader/PSO cache and verify whether the title presents
   and draws with zero rejected PSOs.
2. Otherwise make stream PSO root signatures go through the same `cbv_raw`
   patch as descriptor-created roots, so the shader spaces exist in the root.

### Correction and root-signature fix (same day, later)

- The earlier `cbv_raw=on` result above was a **pre-presentation** artifact: once a
  longer run reached the UI, the same 191 driver and 20 semantic rejections
  returned. `on` does not fix them.
- Title-created root signatures were never converted for the PC runtime in normal
  mode. `patch_real_device` installs its `CreateRootSignature` hook only when
  `rtcap::enabled()`; otherwise the wrapper (`create_root_signature_abi`) applied
  only `global_bind`, and `pso_lib::create_root_signature` forwarded the console
  blob untouched. `alias_blob` (the old path) adds the 60000/70000 spaces but
  does **not** perform the descriptor-table CBV→raw-SRV conversion in
  `cbv_raw::map_space`, so a PS reading `space4160000` failed PSO creation while
  the same stream without the PS was accepted.
- Fix: `cbv_raw::title_root_blob` (`table_blob` in `copy`, `patch_blob` in
  `on`), applied in both the wrapper and `pso_lib`'s root hook; idempotent on an
  already-patched blob. Regression test
  `cbv_raw::tests::title_root_with_a_console_cbv_table_gets_the_raw_srv_range`
  builds the 112-byte console root shape and asserts the range becomes a raw SRV
  in `map_space(1)`, that a positional range at 60000 exists, and that the
  literal sampler bank at 70000 was appended.
- Live: a 285 s run had **0 driver and 0 semantic rejections** (baseline at the
  same duration: 200 + 20). The title also compiles further (700 shaders vs 500)
  but did not reach frame 120 in that window: the patched-root key invalidated
  the shader pack and 224 shaders recompiled (206 s of DXC). Backbuffer pixels
  were not observed this run; no rendering claim.
- Tests: `cargo test -p d3d12_xs --lib` 716 passed, 1 failed (the known
  environmental `xs_exports` console-DLL oracle that needs
  `D:\xbox-pc-port\games`).
- Next: warm the shader/PSO pack over repeated runs (or run `xshader_prelaunch`
  to completion) with the new root hash, then capture frames and `PresentX.draws`.

### The remaining PSO rejections are sampler arrays against static samplers

- A 560 s warm run presented frames 280 and 540 but had **112 driver
  rejections** (a 140 s run had 203), so the earlier 0/0 window only ended before
  the UI pipelines appeared. Backbuffer still 0/8,294,400 non-black.
- The root is now fully patched: `SRV(reg 27, space 4100001)`, unbounded
  `SRV`/`UAV` at 60000, unbounded raw `SRV` at 4160000 and 116 static samplers
  at 70000 (`console blob 6240 bytes`).
- Probe on that rejected stream (PS dumped with `SV_Target0` float4, inputs
  matching the VS outputs exactly):
  - removing the PS succeeds; removing anything else, including the RTV, fails;
  - a DXC-compiled, resource-free PS with the **same input signature** succeeds;
  - single bindings all succeed: `t1@60000`, `StructuredBuffer t6@60000`, and a
    single `SamplerState s0@70000`;
  - all four textures plus `t6` succeed;
  - **`SamplerState s[2]`, `s[8]`, `s[16]`, `s[64]`, `s[80]` all fail** with
    constant or dynamic indexing.
- Cause: the D3D12 specification says samplers in a descriptor heap can be
  dynamically assigned and indexed, **which static samplers cannot**. The
  translator emits `SamplerState tb_lsb[80]` and `litsamp` supplies the bank as
  static samplers, so every PSO that samples through the bank is rejected at
  creation. This accounts for the remaining UI/decal pipelines.
- The fix is a **sampler descriptor heap**: materialize `litsamp::bank()` as 116
  sampler descriptors, change the appended root entry from static samplers to a
  sampler descriptor range at space 70000, and bind that table for PSOs whose
  root carries it. The emulator has no sampler heap today (only `CreateSampler`
  forwarding and a heap-limit check), so this is a new feature, not a one-line
  patch.
- Translator fix in the same pass: `buffer_load_format_x/xy/xyz/xyzw` on a
  memory-based CBV table now translates as 1/2/3/4 raw dwords (the SDF shaders
  use `32_32_FLOAT`). The dumped `PS_AreaPolySdf` / `PS_AreaDecalPolySdf` no
  longer fail with `constant-buffer MUBUF form unsupported`; they remain blocked
  on the sampler-array issue at PSO creation.
- Tests: `cargo test -p d3d12_xs --lib` 716 passed, 1 known environmental
  failure. The PSO probe (`TOMBOX_PSO_PROBE_DUMP`) now also swaps known-good
  shaders/roots into a rejected stream and compiles matching test PS variants.

### The rejected UI shaders, and why a select chain is not the fix

- The rejected PSs were traced back to console shaders `506b2892ea485668` and
  `f10a4be178ecf3e3` via a new PC-bytecode-hash to console-hash map in
  `shader_xlat` (the probe logs it when it dumps a PS). This needed no
  translator change, so the warm pack stayed valid.
- Translated HLSL (obtained with `xshader_corpus --out` and the shader's own
  `.root`): `SamplerState tb_lsb[80] : register(s0, space70000)`, a constant
  `#define samp... tb_lsb[30]`, and one dynamic site,
  `tex.SampleLevel(tb_lsb[tb_lit_sampler_index(s36,s37,s38,false)], ..., 0.0)`.
- Tried: individual `SamplerState tb_lsb_N : register(sN, space70000)` for all
  80 entries, constants rewritten, and the dynamic site expanded into an 80-way
  select over the same sampler states. The generated HLSL had 0 arrays, 80
  individual samplers and the chain, and was formally correct, but DXC did not
  finish compiling the six real variants within 180 s (the array version
  compiles in under a second). That would stall the title, so it was reverted.
- Therefore the fix must keep the array and give it a sampler descriptor range:
  create the 116 `litsamp::bank()` samplers as descriptors in a shader-visible
  sampler heap, append a sampler-only root table at space 70000 (a table cannot
  mix samplers with the SRV ranges), and bind it whenever a PSO/root carrying
  it is set. `add_literal_samplers` changes from static-sampler records to that
  table. No sampler heap exists in the emulator today.

### Sampler heap implemented through direct heap indexing

- The title creates a shader-visible sampler heap (128 entries).
  `CreateDescriptorHeap` now grows every shader-visible sampler heap to the
  2048-entry D3D12 maximum and writes `litsamp::bank()` at index 1932..2047
  (increment 32, verified in the log). The title's own descriptors keep their
  indexes.
- The translator declares bank entries as individual static samplers for
  constant uses (no array) and emits, only for the dynamic S# site,
  `SamplerState tmp = SamplerDescriptorHeap[1932u + tb_lit_sampler_index(...)]`.
  DXC compiles the six real variants in 0.9 s (the 80-way select chain did not
  finish in 180 s).
- Roots now set `SAMPLER_HEAP_DIRECTLY_INDEXED` (0x20) and
  `CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED` (0x10); the patched title root reports
  flags 0x831.
- Measured in a 120 s run: 34 presents, **6 driver rejections** (from ~203 in
  140 s), no crash. The sampler bank is installed and the title runs.
- **Not yet visible game content.** Presented frames 30/60 are non-black
  (80,862 and 84,681 non-zero bytes of 24,883,200), but inspection shows those
  pixels are the emulator's own performance OSD, not title output. The title
  backbuffer is still 0/24,883,200.
- The six remaining rejections are the same UI PSs (plus `a6dbf98d29c8c2b3`).
  Next: compile a synthetic PS that uses `SamplerDescriptorHeap` on the real
  root to prove the PSO path, then determine why the real PS still fails
  (candidate: the PSO may still carry pre-rebuild bytecode for that shader).

### Root-signature flags were the residual rejection (fixed)

- A synthetic PS using `SamplerDescriptorHeap[1932 + x]` on the rejected
  stream's own root failed with `0x80070057` for both 2D and 3D sampling, so the
  root, not the title bytecode, was the defect.
- The flag mask was wrong: `0x10`/`0x20` are
  `DENY_GEOMETRY_SHADER_ROOT_ACCESS` / `DENY_PIXEL_SHADER_ROOT_ACCESS`, not the
  direct-heap flags. The correct bits are
  `CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED = 0x400` and
  `SAMPLER_HEAP_DIRECTLY_INDEXED = 0x800`. The previous patch denied the pixel
  shader root access on every title root.
- After correcting the mask, a run with the probe enabled recorded **zero**
  `0x80070057` rejections (previously ~203 per 140 s, then 6 per 120 s).
- Tests: `d3d12_xs` 716 passed + 1 known environmental failure; `tombox-xshader`
  126 passed.

### Why the title backbuffer is still black

- The present path is: `PresentX` plane 0 resource -> `CopyResource` into a
  swap-chain buffer -> overlay OSD drawn into the swap-chain buffer -> `Present`.
  The `title_backbuffer` snapshot is a copy of plane 0, so OSD pixels never
  enter that measurement.
- Measured at frames 60/120/240: `title_backbuffer` **0 of 8,294,400** pixels;
  `presented_swapchain` ~27,500-29,300 pixels, all OSD.
- `PresentX.draws` shows the title issues only **3 draws per present**, all into
  `rt#022` and `rt#000`, while plane 0 alternates between `rt#000` and `rt#001`:
  `draws_into_presented=0`, `dispatches=0`, no copy calls in the log. The plane
  parameters are `plane_count=1, res_count=1` (logged), so this is not a
  multi-plane read.
- The run is still in a startup precompilation phase: `pso_lib` compiled
  **50,883 PSOs** (117 hits) and the title kept presenting the same 3-draw
  content through present 780. The title is not yet rendering a menu/game frame;
  this is a loading/precompile stall, not a missing copy of a rendered frame.
- Open: the precompilation volume (50k driver PSO builds) is itself a
  performance defect.

### Heap binding and input are not the black-screen cause

- With the heap-binding probe re-enabled (safe after fixing a descriptor
  lifetime bug), the title calls `SetDescriptorHeaps n=2` with
  `type=0 num=1000000 flags=0x1` **and** `type=1 num=2048 flags=0x1`: the
  extended sampler heap really is bound, so the direct-heap reads are valid.
- A long run (MDBG_LIMIT=900) reached **present 1500** with snapshots at frames
  300/600/900/1200: `title_backbuffer` stayed **0 of 8,294,400** every time, and
  the draw count stayed exactly **3** (all into `rt#022` and `rt#000`), with
  `draws_into_presented=0` and no copy calls. `pso_lib` reached 25,711 hits /
  25,289 misses.
- `PresentX` planes are single-resource (`plane_count=1, res_count=1`), so the
  presented surface choice is not a multi-plane read either.
- A virtual-pad test (`press start`, `press a`) did run, but the title logged
  "has read the virtual pad 0 time(s)": it is not polling input at this stage.
- Conclusion: with zero pipeline rejections and a bound sampler heap, the title
  is still in a startup state that submits three draws into an offscreen target
  and presents a cleared buffer. The black title backbuffer is therefore a
  title-side startup/loading stall, not an OSD measurement error and not (per
  the measurements above) a missing present copy. No genuine title frame has
  been captured yet; this remains the open blocker.

### XUser: the confirmed interface was missing the calls the title makes

- The title queries `XUser(01acd177)`, then calls slots **9 (GetLocalId), 13
  (IsGuest), 19 (AgeGroup), 20 (CheckPrivilege) and 33
  (RegisterForChangeEvent)** on that same IID. The interface had been built from
  a confirmed subset `[3,4,5,7,8,11,29,30,34]`, so every one of those calls
  returned `E_NOTIMPL` right after `XUserAddAsync` succeeded. The title could
  never obtain a local user id, age group or privileges, and never registered
  for sign-in change events.
- Fix: all three XUser IIDs (and both XUserDevice IIDs) now expose the full
  layout from `user_layout()` / `device_layout()`; a regression test asserts
  slots 9/13/19/20/33 exist on every `XUser(...)` interface.
- Measured after the fix: no `unconfirmed-slot XUser` lines at all;
  `XUserAddAsync options=0x1 -> local user 'XboxEmulator' hr=0x0`;
  `XUserCheckPrivilege privilege=185/188/189 -> granted`; gamertag interface
  queried. The startup still submits the same three draws, so user setup was a
  real defect but not the whole stall.

### Virtual pad: real reads confirmed

- Before: a scripted `press start` / `press a` was consumed and the title logged
  "read the virtual pad 0 time(s)" for the whole run.
- After: `IGameInput::GetCurrentReading` reaches the virtual device
  (`virtual pad reading (kind=0x40000) (#0)`, `(#1)`, `(#2)`), the title's own
  read counter reaches 1 and more GameInput reads follow at each UI update. The
  pad state bytes are served from `Reading::state`; no stub is involved.

### Presentation ordering (measured, correct double buffering)

- Present 2: `src=rt#001`, draws = `rt#022 x2, rt#000 x1`.
- Present 3: `src=rt#000`, draws = `rt#022 x2, rt#001 x1`.
- The title therefore draws the *next* frame's image and presents the image
  rendered on the previous frame. `draws_into_presented=0` is an attribution
  artifact of measuring only the current frame's draws; it does **not** mean the
  present path skips a copy. The presented `rt#000` at present 3 contains the
  single draw submitted at present 2, and that draw still leaves the buffer at
  **0 of 8,294,400** non-black pixels (frames 60/120/180/240).
- The one per-frame draw into the presented image is preceded by two draws into
  `rt#022`; the four descriptors written per frame at increasing heap slots
  (e.g. 978669/982765/986861) are rejected by `title_heap` with
  "has no PC resource/format match" and replaced by null views. Their payload
  (`[45700000, 45070000, ...]` = 3840.0, 2160.0, `[bf800000, 3f800000]` = -1.0,
  1.0) looks like constant/viewport data, not SRV/UAV descriptors, so the
  decoder is interpreting a non-resource table as resource descriptors.

### New failure observed: device removal during cold PSO setup

- A cold-cache pass-capture run failed before present 2:
  `centroid root signature with the centroid priority constants rejected
  hr=0x887a0005; created without them`, immediately followed by
  `dred.removed exiting now with code 0x887a0006 (device removed)`. This is a
  separate GPU-stability defect (invalid root-signature/driver work), not the
  black-frame cause; warm-cache runs reached present 360+ without removal.

## Session: black title buffer localized to the title's own empty UI table

### Fixed: stale pipeline library caused the cold-run device removal

The 255 MB pipeline library on disk predated the current stream-PSO translation
ABI. It was loaded, then every lookup missed and the driver recompiled ~50k PSOs;
the first cold run died with `0x887A0005`/`0x887A0006` while building them. The
library file now carries `FILE_VERSION=3`, the `PIPELINE_ABI` hash in its cache
identity and an FNV64 checksum trailer, so a library from another ABI is refused
instead of half-loaded, and a damaged file is rejected rather than trusted.
Measured on the last three runs: `0 rejected loads, 0 unkeyed`; the warm library
answers `50,920 hits, 165 misses` (all misses are genuinely new stream PSOs).
A tamper/truncation test covers the checksum path (12 pso_lib tests).

### Fixed: root-signature layouts, pages above the host ceiling, cleared slots

- Root layouts are registered from the title's original root-signature blob
  (versioned parser), so a descriptor table is mirrored with the parameter's real
  kinds and ranges instead of guessing.
- A console descriptor page past the host heap capacity is remapped onto a host
  page no live descriptor uses (`descriptors 979/983/987/991/995 -> host
  974/973/972/970/969` in the title run), so high console indices translate
  instead of clamping.
- A cleared console slot is the null descriptor: an existing host copy is reset so
  it cannot keep sampling an unbound resource, but a null never allocates a page
  and never keeps one out of the reclaim pool (`live` is tracked separately from
  "has a host copy"). Doing this naively (allocating for every zero slot) made the
  title miss frame 800 entirely: one run presented only a few frames in 102 s.
- A table whose root-signature layout is unknown now binds the longest contiguous
  hosted run starting at its first descriptor (a D3D12 table must be one contiguous
  range) instead of falling back to the null table for small heaps.

Tests: `d3d12_xs` 717 passed, 1 failed (the known environmental
`xs_exports` oracle that needs `D:\xbox-pc-port\games`); `tombox-xshader` 126
passed. Zero PSO rejections in every run of this session.

### Measured: the presented image is the OSD, the title buffer is exactly zero

With `TOMBOX_PRESENT_SNAP` on the final build:

| frame | title backbuffer | presented swapchain | distinct rows |
|------:|-----------------:|--------------------:|--------------:|
| 10    | 0 / 8,294,400    | 26,295 / 8,294,400 (0.32%) | 185 |
| 400   | 0 / 8,294,400    | 30,279 / 8,294,400 (0.37%) | 205 |

Earlier runs measured frame 800 and 1000 at `0 / 8,294,400` and
`29,422 / 8,294,400 (0.35%)`, 194-195 distinct rows. The non-black pixels are the
OSD overlay text, which is drawn only into the presented copy; the title's own
backbuffer is read before that draw.

### Measured: what the rendering passes actually do

- Pass capture at present 300 sees three draws: two into a 3840x2160 offscreen
  target with a DSV (flags `0x1`), one into the presented-chain buffer. All three
  captures are black.
- Viewport and scissor are full for every captured draw
  (`0,0 3840x2160`, scissor `0..3840 x 0..2160`), so this is not a raster area
  problem.
- Both offscreen draws report `ia=[no vertex buffer bound]` (procedural geometry)
  and both pipelines have a real VS and PS (`stages=(true,true)`); no accepted
  stream pipeline in any run lacks a pixel shader.
- Pipeline statistics at present 300: offscreen draw 1 `PSInvocations=8,294,400`
  (full screen); offscreen draw 2 `GSPrimitives=2, PSInvocations=4,147,200`
  (exactly half the screen); the composite draw into the presented buffer
  `IAVertices=3, GSPrimitives=2, CInvocations=2, PSInvocations=0`.
- The offscreen target's alpha is `mean 0.50000, min 0, max 1` with half the
  pixels at alpha=1 (the half-screen rect) and RGB zero everywhere.

### Corrected: the four "rejected descriptors" are not descriptors

The per-frame `has no PC resource/format match` lines are **not** the title's UI
descriptor table. Probing the composite's table address (`0x5678a0253dd80` ->
host slot 696,044) shows its six SRVs are all null, and the failing slots
(1,220,333/335/337 and 1,220,464) are produced by the whole-heap `DIRECT_RESOURCES`
mirror, which scans past the live descriptor tail into a constant/vertex ring. The
payload (`3840.0, 2160.0, -1.0, 1.0`, `ffffffff`) is viewport-like constant data.
Those lines are mirror noise, not a descriptor-decoding defect, and fixing them
would not change the image.

### Remaining blockers (honest)

- The title's UI descriptor table is literally all-zero at present 300, and the
  composite draw shades zero samples on a two-primitive rect while a real PS is
  bound: the rect is degenerate or entirely off-screen. That is the immediate
  black-frame cause, and it is consistent with a title that has not uploaded its
  UI constants yet rather than with a translation bug.
- The title raises `0xC0000005` writing to `0x0` at
  `game_scarlett_master.exe+0x2215c35` after 135-150 s (its own heap allocator,
  3-4 GB RSS), before it ever leaves this state. Runs that reach frame 1000 exist,
  but the crash usually lands first, so the title has not been observed
  completing its first scene.
- The warm pipeline library still misses ~165 stream PSOs per run, and every miss
  is a driver compile. That is the dominant cold-start cost, not a correctness
  defect.

## Session: the composite draw's state, measured at draw time

`passcap` used to record only whole passes, so a draw whose render targets were
set before the shot armed (the presented-buffer composite) had no record at all.
Each shot now also writes `draws.txt`: every colour draw recorded while the shot
was armed, with the draw arguments, the console PSO, the topology the title set,
RTs/DSV, viewport/scissor, bound vertex buffers, root constants, root CBVs with
their first bytes, descriptor-table handles, and the PC pipeline variant the list
actually bound (with its raw stream subobjects). That is what finally attributed
the failing draw.

### What the failing draw is

```
kind=0 args=DrawInstanced v=3 inst=1 sv=0 si=0 topo=Some(17) rt=[presented buffer]
       dsv=0x0 stages=(true, true) viewport[3840x2160 at 0,0] scissor[x0..3840 y0..2160]
       vb[]  bound PC variant <rect-expanded stream>
```

- Topology 17 is RECTLIST, so the emulator expands it to two TRIANGLELIST
  triangles with a synthesized geometry shader; the variant really is bound and
  the GS really runs (`GSInvocations=1, GSPrimitives=2`).
- The console pipeline carries a real VS and PS (`stages=(true,true)`), a sample
  mask of `0xffffffff`, FillMode solid, CullMode back, FrontCounterClockwise 1.
  No vertex buffer is bound, so the three vertices come from `SV_VertexID` plus
  whatever the shader reads.
- Pipeline statistics for it: `IAVertices=3 GSPrimitives=2 CInvocations=2
  CPrimitives=2 PSInvocations=0` — both primitives reach the rasterizer and not
  one sample is shaded.

### The rect expansion is not the defect (measured A/B)

- With `TOMBOX_NO_RECTLIST=1` (console PSO kept, no GS, one triangle) the same
  draw reports `IAVertices=3 GSPrimitives=0 CInvocations=1 CPrimitives=1
  PSInvocations=0`. Removing the expansion does not change the result.
- The other RECTLIST draw in the same frame, expanded by the same synthesized GS,
  shades exactly half the screen: `GSPrimitives=2 CInvocations=2 CPrimitives=2
  PSInvocations=4,147,200`. The expansion path is therefore producing correct,
  visible geometry elsewhere in the same frame.
- Forcing CULL_NONE on the expanded rect variant changed nothing either
  (`PSInvocations` stayed 0), so the two triangles are not being culled; they are
  degenerate or entirely outside the viewport. That change was reverted rather
  than shipped on an unproven theory.

Conclusion: the composite draw is faithfully translated. Its three vertices
rasterize to zero area because of the data the title gives the vertex shader at
this stage, not because of a bound-state translation defect. The black frame is
the title's own black frame (the two offscreen UI draws shade the full screen and
half the screen and write RGB 0 with alpha 0.5), and this draw then fails to copy
it anywhere.

### Measured on the shipped build

| frame | title backbuffer | presented swapchain |
|------:|-----------------:|--------------------:|
| 10    | 0 / 8,294,400    | 27,117 / 8,294,400 (0.33%), 187 rows |
| 200   | 0 / 8,294,400    | 28,582 / 8,294,400 (0.34%), 218 rows |

Zero PSO rejections (`0 rejected loads, 0 unkeyed`); `d3d12_xs` 717 passed with
the one known environmental `xs_exports` failure, `tombox-xshader` 126 passed.

### Still open

Reachability is now the whole blocker: the title raises `0xC0000005` writing to
`0x0` at `game_scarlett_master.exe+0x2215c35` after 135-150 s (its own heap
allocator) and never reaches the frames where it has content to composite.

## Session: bundle inheritance measured; the composite's vertex source identified

The previous session's next step assumed the presented-buffer composite was issued
from a command bundle, so its root arguments would live on the parent list. The
capture was extended to settle that with measurements rather than assumptions:

- `passcap` now hooks `ExecuteBundle` (slot 27) and records, at execution time, the
  calling list's root signature, every root constant with its float value, every
  root CBV address with its first 32 bytes, and the bundle's own state.
- Every draw record now carries `list=`/`key=`, the vertex buffer views bound in
  the emulator's own list state (not only in the optional rtcap trace) with the VA
  resolved through `gpu_va` and the first bytes of the data, plus the index buffer.

### Measured: there is no bundle inheritance to fix here

In the captured frame around present 300 there are **no `ExecuteBundle` records at
all**, and all three draws report `list == key` (the same command list, not a
bundle and not a split segment). The composite is therefore not inheriting root
state from a parent list: its list only ever sets a graphics root signature
(`groot=…`), with no root constants, no root CBVs and no descriptor tables
recorded. The bundle-inheritance hypothesis is disproven for this draw.

### Measured: the composite does have a vertex buffer

```
draw 02: DrawInstanced v=3 inst=1 sv=0 si=0  topo=17 (RECTLIST)
         vb[0=0x…ddf0/4080B/24 → resource 0x…fea0+0xabddf0 of 0xae00000]
         ib[none]  viewport 3840x2160  scissor full
```

The resource is the 182,452,224-byte buffer created by `CreateResourceFromAddressX`
and placed on an *aliased shadow heap*, so the GPU reads the title's own pages. The
other rect draw in the same frame (which does shade 4,147,200 samples) binds the
same resource at `0xabee10`, 4128 bytes later, with the same size and stride.

`title_shadow::read` (the write-back shadow's merged view, i.e. what the GPU sees)
and a new `title_shadow::raw` (the title's live bytes) now print the first 96 bytes
of every bound vertex buffer, so the two can never be confused again:

```
draw 01  gpu=[-1 1 0 NaN 0 0  -1 -1 0 NaN 0 1  1 -1 0 NaN 1 1]
         title=[-1 1 0 NaN 0 0  -1 -1 0 NaN 0 1  1 -1 0 NaN 1 1]   → 4,147,200 px
draw 02  gpu=[0 0 0 NaN 0 0  0 2160 0 NaN 0 1]
         title=[0 0 0 NaN 0 0  0 2160 0 NaN 0 1]                  → 0 px
```

Three facts follow. First, the shadow path delivers the title's bytes exactly
(`gpu == title` byte for byte), so global/aliased vertex data is not being lost.
Second, the vertices are `(top-left, bottom-left, third)` with a don't-care `z/w`
(the working draw uses clip-space NDC corners, the composite uses **pixel-space**
corners `0,0` and `0,2160` for a 3840x2160 target). Third, the composite's list has
no root constants, no root CBVs and no descriptor tables recorded at all, so its
vertex shader has nothing that could turn pixel coordinates into clip space — the
leading measured explanation for a full-target rect rasterizing zero pixels while
`CInvocations=2 CPrimitives=2` (clipped away, not culled).

Capture caveat: `draw_record` only scans descriptor tables at params 0..4 / slots
0..4 and does not hook root SRV/UAV (Xbox slots 44/46), so "no transform bound" is
currently proven only for root constants, root CBVs and those tables.

### State on the shipped build

Composite: `IAVertices=3 GSPrimitives=2 CInvocations=2 CPrimitives=2 PSInvocations=0`.
Title backbuffer 0 of 8,294,400 non-black pixels; presented swapchain OSD only.
Zero PSO rejections. `d3d12_xs` 717 passed (1 known environmental), `tombox-xshader`
126 passed.

## Rect-list corner convention: the three vertices are *consecutive* corners

The synthesised RECTLIST geometry shader treated `v0` as the corner diagonally
opposite the one it generated, so it completed the rectangle as `v1 + v2 - v0`.
The title's own rect lists say otherwise, on both of its RECTLIST draws:

```
draw 01 (clip space)  v0=(-1,1) uv(0,0)   v1=(-1,-1) uv(0,1)   v2=(1,-1) uv(1,1)
draw 02 (pixels)      v0=(0,0)  uv(0,0)   v1=(0,2160) uv(0,1)  v2=(3840,2160) uv(1,1)
```

Both are top-left, bottom-left, bottom-right, so `v1` is the corner *between* the
other two, the omitted corner is top-right, and its texture coordinate must be
`(1,0)`. Only `v0 + v2 - v1` yields that: `v1 + v2 - v0` gives uv `(1,2)`, outside
the source rect, and its quad `(v0,v1,v3,v2)` only covers half the target. The
arithmetic matches the measurement exactly: the old rule covered 2 of the 4 clip
units, i.e. 4,147,200 of 8,294,400 pixels.

`rectlist.rs` now completes the parallelogram as `v0 + v2 - v1` (with the same
projective `1/w` handling, roles permuted) and emits the fan in quad order
`(v0,v1,v2)(v0,v2,v3)` so both triangles keep one winding instead of the previous
`(v0,v1,v3)(v0,v3,v2)`, which alternates it. Measured on the title:

```
draw 01  PSInvocations 4,147,200 -> 8,294,400   (full 3840x2160 target)
         occlusion samples 4,147,200 -> 8,294,400
```

### The composite is the draw into the presented buffer, and its VS is not a passthrough

`draw 02` is pass#2 on the same list, `rt` = the presented source, no DSV, full
viewport/scissor. Its translated VS is 7,972 bytes / 722 lines and computes
`SV_Position = vertex term + cbuffer term` (`fadd` of a per-vertex value and an
`FMad` chain), reading two raw buffers:

```
sbuf3932160002  texture struct r/o  t2, space60000      (positional SRV range)
sbuf268697665563 texture struct r/o t27, space4100001   (descriptor-table CBV range)
```

So the pixel-to-clip transform does exist and comes from the descriptor table the
title binds at root parameter 0 (`SetGraphicsRootDescriptorTable(0, ...)`), whose
ranges cover both an SRV (t2) and a CBV (b27). The journal for that list records

```
SetPipelineState(0x23525ea5bd0)
SetGraphicsRootSignature(0x235067f5300) was 0x235067f5300     <- same signature
SetGraphicsRootDescriptorTable(0, 0x2334c8add80)
```

The re-set is the *same* root signature and root arguments are only cleared when
it differs, so "bound then lost" is disproved for this draw: nothing was dropped
between the table bind and the draw. With the corrected corner rule the composite
still reports `GSInvocations=1 GSPrimitives=2 CInvocations=2 CPrimitives=2
PSInvocations=0`, i.e. its quad is still degenerate, which now points at the
constants its VS reads rather than at the rect.

### State on this build

Composite `PSInvocations=0`; title backbuffer 0 of 8,294,400 non-black pixels;
presented swapchain OSD only. Open lead: `title_heap` reports 1,032
`console descriptor [...] has no PC resource/format match; null view written`
events, and the one-shot block dump shows the first one in a UAV range over an
all-zero heap region, so which of the composite's table descriptors (if any) are
null is not yet established.

## The composite's transform constants: a table CBV written where the PC signature declares a raw SRV

The open lead was the constants. `cbv_raw` (mode `copy`) rewrites every console
descriptor-table CBV range to `RANGE_SRV` in `space 4100000+n` and rewrites the
translated shader to read it as a `ByteAddressBuffer` (that is how it keeps a
console CBV readable on PC without a heap of its own). Individually created CBVs
go through `cbv_raw::create_cbv`, which writes a *raw buffer SRV* to match. Heap
slots do not: `title_heap::write_slot`'s `KIND_CBV` branch called
`CreateConstantBufferView` (device vfn 17) unconditionally. A real CBV handed to
a range the PC root signature declares as an SRV has a buffer address where the
view format belongs, so the read returns nothing: the composite's VS took its
`t27, space4100001` term as zero, `SV_Position` collapsed and the quad shaded no
pixels.

Fix (`title_heap::write_slot` + `cbv_raw::table_cbv_as_raw`): when the CBV ranges
of the signature were rebound (the same predicate `create_cbv` converts under),
the heap slot is written with `CreateShaderResourceView` (vfn 18) over
`cbv_raw::raw_view(va, cbv_size, gpu_va::lookup)` - the identical view the
CreateConstantBufferView path writes - and recorded as `Desc::Srv` in the
binding trace. The plain CBV path stays for signatures that were not rebound. No
draw is special-cased; every table CBV slot of every signature goes through it.

### Measured on the title (`pass-c10`, presents 100 and 140)

```
draw 00 topo=TRIANGLELIST rt=0x1e3538700c0 ... PSInvocations=8294400
draw 01 topo=RECTLIST     rt=0x1e3538700c0 ... PSInvocations=8294400
```

Both full-target draws went from `PSInvocations=0` to **8,294,400** invocations,
exactly the 3840x2160 pixel count, in two consecutive captured frames (presents
100 and 140). The composite's VS now reads the constants the title set.

### Attribution of the null descriptors (asked and answered)

The binds are not among the mis-decoded ones. `title_heap` reported
`console slots=1245184 host slots=1000000 ... slot 0 at 0x1cb95840000, console
stride 32`; the composite's table handle `0x1cb974fede0` is console slot
**964,495**, and the `no PC resource/format match; null view written` events of
that run (1,508 of them, min slot 11, max slot 1,236,848) contain **zero** slots
in the 964,xxx band - they are float data (`3840.0 2160.0 0 NaN 1.0 1.0 ...`,
`0 0 0 0 -1.0 1.0 0 NaN`) sitting in unused regions of the 1.25M-slot bindless
heap, not the composite's range. The register mapping is also not at fault: the
root-signature patch appends parameters, so console parameter indices are
preserved, and the journal shows the same signature re-set rather than replaced.

### Next break, located and measured but not fixed

The composite shades but the target is still black, and the frame never reaches
the presented surface:

* every capture of the frame is `zero_px=1.0000`: RT0 (`cap 0000`, 0x1d3dba03680,
  flags=0x1), the DSV (`cap 0001`, all-zero depth), and the presented source
  (`cap 0005`, 0 of 8,294,400 non-black);
* both draws render into a *placed* 3840x2160 target (VA `0x1e1e6260000`), while
  `PresentX` names a *committed* 3840x2160 resource, and the recorded frame has
  `resolves=0 copies=0` with `WRITES-PRESENTED-SRC ps=[]`.

The composite's PS is not a passthrough either: it declares inputs
`SV_Position/COLOR0/TEXCOORD0`, one `SV_Target0`, and a
`Resource bind info for sbuf...` buffer - an SRV buffer from the same table. The
PS therefore still reads black there, which is the next thing to attribute.

### State on this build

Composite `PSInvocations=8,294,400` (was 0); title backbuffer 0 of 8,294,400
non-black pixels; presented source 0 of 8,294,400 non-black, so the swapchain
still shows only the OSD the emulator draws after the copy. The visible frame is
still black.

## Is the black real, and what does the composite's PS actually compute?

### The capture path is honest: the window itself was read

Two independent readbacks (`passcap`, `rtcap`) use the same copy-into-a-readback
buffer mechanics, so neither can settle a "maybe the capture lies" question. An
outside measurement can: a Windows `PrintWindow` capture of the title's own
window (`wincap.ps1`, PW_RENDERFULLCONTENT, 2x2 sampled) at present ~60 reports
`sampled=379760 nonblack=14479 frac=0.0381 max=255 mean=8.94`, and the image is
the emulator's OSD text and frametime graph on pure black. The OSD is drawn by
`present.rs` **after** `CopyResource(swapchain, src)`, so this separates the two:
OSD = 3.81% of the window's pixels, the title's own contribution = **0**. The
capture path is not lying; the title's target really is black, and the DSV
reading all-zero is consistent (both full-target draws write z=0 everywhere).

### What the composite's PS computes (translated DXIL, draw 01)

```
Resource Bindings:
 sbuf268697665563   texture  struct  r/o  T0t27,space4100001  1
%1  = createHandle(0, 0, 27)                       ; SRV t27, space 4100001
%2..%5 = loadInput(1, 0..3)                        ; COLOR0 (interpolated)
%6..%13 = rawBufferLoad(handle, i32 2, 0|4|8|12)   ; element 2 of the struct
%14 = bitcast i32 %7 to float
%16 = fcmp oeq float %14, 0
%18 = fmul fast float %14, %2
...
```

The PS is a **full-target fill whose colour is element 2 of the very same
constant block** (`t27, space4100001`, a struct of `[4 x i32]` per element) whose
transform the VS reads, multiplied by the interpolated vertex colour, with an
explicit `== 0` guard per channel. So one 16-byte element decides both geometry
and colour: if the block holds black, the frame is black **by construction**, and
the `==0` guards turn "read nothing" into exactly the same picture.

### Correction to the previous entry's causal claim

`title_heap::write_slot`'s CBV-to-raw-SRV path did **not** run in the latest
title run: with `cbv_raw=copy` active it logged no conversion and no conversion
failure, while 868 `no PC resource/format match; null view written` events came
from other slots. The composite's own table slots therefore go through the
generic console-descriptor decode (kind 1), not `KIND_CBV`. The earlier
`PSInvocations=0` -> `8,294,400` step cannot be attributed to that commit: the two
runs' vertex buffers differ (pixel-space corners then, NDC corners now), and an
NDC vertex term covers the target even when the cbuffer term is zero. The
`write_slot` change is still right on its own terms - a CBV mirror written into a
range the patched signature declares as an SRV can never be read - but it is not
what made this draw cover its target, and the claim that it was is withdrawn.

### New measured leads

* A **third draw** exists in the same captured frame: `topo=17 rt=0x170c7dc24a0`
  with `PSInvocations=0` while the two draws into the other target report
  8,294,400 - same signature, same list.
* `title_heap heap 0x170be140270: 4 slot ranges are bound as different view kinds
  by lists in one submission; the most recently recorded binding is mirrored`:
  one mirror per range cannot serve two kinds in one submission, so at least one
  list in that submission reads a descriptor written for the other.
* One signature parses as `0 table parameter(s); param 0 = None` even though the
  title binds a table at parameter 0 on it.

### State on this build

Visible frame black: window = OSD only (3.81% of sampled pixels), title 0 of
8,294,400 non-black; drawn target 0 of 8,294,400; presented source 0 of
8,294,400. The next thing to measure is the 16 bytes of element 2 in the
constant block behind `t27, space4100001` at draw time (and the same block's
transform element), which requires a read that is valid at that moment - the
sample added for it never fired, because the `KIND_CBV` path it hung off is not
the path this title's tables take.
