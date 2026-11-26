# Xbox Series X|S compatibility

Verified means the stated scenario ran on the host or passed a synthetic
regression test. Implemented means the path exists and its listed live
scenario still needs verification. Partial and missing identify coverage gaps.

| Subsystem | Status | Evidence and limits |
|---|---|---|
| Native x86-64 execution | Verified | The Series X executable ran through boot, menus, and into world rendering. All 587 imports in the main executable bound. The guest runs directly on the host CPU. |
| Host memory | Partial | Series X mode needs about 21 GB of process commit and about 10 GB of GPU allocations in the world, which does not fit the tested host (15.5 GB RAM, commit limit 33 GB, about 11 GB taken by Windows and the other software left running). Series S mode fits (menus 9.3 to 10 GB process commit, 6.6 GB free commit; in the world 0.3 to 2.4 GB free). |
| Kernel, loader, and threads | Partial | PE mapping, TLS, imports, and execution passed the synthetic loader probes. Console services use HLE and compatible Windows APIs. |
| GPU command submission and presentation | Partial | The original title presented its menus at 30 FPS. World captures show the letterbox and interface over black (27 November: 0.7 to 1.1 FPS in Series S mode, see "World cutscene on the NVIDIA host"). Host submissions use one D3D12 direct queue with a fence wait after each, 18 to 41 a frame in the world, which also orders guest memory writeback; the GPU is 25% busy on average. |
| Shaders and pipelines | Partial | D3D12 supports compute, graphics, mesh, and indirect paths in VC. 144 distinct compute pipelines of the world declare `WaveSize(64)` and are refused by the 32-lane device (27 November: 150 before the splitter learned whole-wave sums and prefixes, lane masks read as numbers, and loads that cannot follow a store; every one logged with its declared wave size); their dispatches are dropped. Among them 3 of the title's ray-tracing BVH builder (`XboxBvhBuilder.dll`), whose hang when its dropped neighbours run is described below. 7 more shaders do not translate (`v_cvt_f32_f64`, `v_permlanex16_b32`, lane masks read as data, a descriptor read only as data, barycentrics read directly). The first world run on a device or after a translator change finds none of ~1,000 pipelines made: the library has to be built ahead (`translate --cache --pipelines`). The shader suite also reproduced an access violation in the heap-direct texture-read test. |
| GPU memory and descriptors | Partial | Synthetic tests exercise descriptor mirroring and persistent view pairs. The 1,245,184-slot guest heap has 934,464 mirrored slots on this adapter and uses dynamic slots for tables that extend past the mirror. Direct indexing of the unmirrored guest range still needs remapping. Dynamic slots (65,536 on a heap the host caps, 196,608 in Series S mode) ran out every world frame because each list kept a run of 256 for root views; lists now give back what they did not use when the title closes them (27 November). |
| Ray-traced lighting | Missing | The title's world acceleration structures still need to be built for ray-traced shadows and indirect light. |
| Storage and streaming | Partial | The runtime opened the original startup and world archives and initialized 12 DirectStorage workers. BCPack and zlib decoding paths are implemented. Isolated throughput during world loading still needs measurement. |
| Audio and video | Partial | The original intro video presented. The project implements XMA and Opus decoding. Audio fidelity and world playback still need separate validation on this host. |
| GameInput | Verified | Controller A and directional inputs advanced the original setup screens. The GameInput tests cover device identity, readings, callbacks, keyboard fallback, and rumble routing. |
| System services | Partial | The isolated local profile persisted setup and started another run. The loopback networking test failed in this environment. |

## Windows run on 26 November 2026

These measurements used a diagnostics build of VC and the original Series X
title. The test profile and shader caches were outside the workspace.

| Phase | Frame rate and visible output | Timing and resource use |
|---|---|---|
| Menus | 30 FPS at 33.3 ms per frame. Original menus and first-boot screens visible. | About 0.3 to 0.9 ms of submission preparation and 0.8 to 2.6 ms of host queue wait per frame in steady menus. About 4.9 GB of local GPU memory and 13.7 GB of process commit. |
| World transition | About 0.1 FPS at 7.5 to 9.6 seconds per frame in later baseline samples. The first captured world frame was black. Later captures remained nearly black. | About 6.6 to 7.0 seconds of host queue wait and up to 1.37 seconds of submission preparation per frame. One sample recorded 2,344 draws, 4,550 indirect calls, and 4,009 dropped operations per frame. At a later sample, DXGI reported 5.3 GB local and 7.1 GB non-local GPU memory, with 22.1 GB process commit and 1.8 GB working set. |
| World with GPU timestamps | The black frame reproduced after restarting with the same profile and cache. | One sample measured 6,961.6 ms of GPU commands per frame. A single 16 x 16 x 16 dispatch used 6,523.1 ms. The same command repeatedly took about 6.5 seconds. |

During world loading, one 18-second sample used about 9.2 of 12 logical CPUs.
Six threads each used more than 92 percent of one logical CPU. Per-engine
utilization counters were unavailable in this environment. The GPU timing
figure comes from D3D12 timestamp queries. Guest-only retired instructions
still need a suitable process-side profiler.

The descriptor cap used to generate 310,720 warnings per boot. The runtime
now reports the capacity once and clips dirty-page scans to the mirrored
range. The previous warm-cache run first reported 30 FPS 41.1 seconds after
GameInput started. The patched run first reported 30 FPS at 6.4 seconds.
The cache and profile evolved between the two runs.

With the diagnostics loop guard temporarily set to 1,024 iterations, the
slow dispatch took about 6.7 ms. World captures stayed black. The sensitivity
to the guard identifies prolonged loop execution in that shader. The reason
its normal exit condition fails and the complete rendering failure still
need investigation. Controllable gameplay remains unverified on this host.

## Structured buffers on the NVIDIA adapter, 26 November 2026

Measured with the unit suites on the RTX 5060 (driver 32.0.16.1088, 32-lane
waves) by D3D12 dispatch and readback, not by the title.

**Finding.** A shader handle whose declared kind or stride differs from its
descriptor's addresses the buffer by the wrong stride on this adapter. A
heap-direct `buffer_load_dword idxen offset:4` of element `l` through a raw
handle on a 12-byte structured view returned element `3l + 1`, dword 0 (the
byte address divided by 4, then scaled by the descriptor's stride). A
structured handle declared with stride 4 on an 8-byte view returned element
`l + 1`. The address is computed in the declared stride's units and scaled by
the descriptor's. The stride of a view a shader picks from the heap is not
known when it is translated, and the old translation read structured views
through raw handles (SMEM scalar loads, heap-direct element reads) or through
handles with the entry's stride (table-bound element reads). On this adapter
the first two returned wrong data in the tests (strides 8 and 12). Whether the
title's reads of that kind are the cause of the black world and the
6.5-second dispatch has not been checked in the title: one dispatch whose
loop bound is read that way would explain it, nothing here shows that it is.

**Change.** The host mirrors every structured buffer view (shader or
unordered access, a stride that is a multiple of 4, a start aligned to
`D3D12_RAW_UAV_SRV_BYTE_ALIGNMENT`) as a raw view of the same bytes
(`vc_gpu::descriptor::host::raw_srv`, `raw_uav`), and translated shaders
address structured buffers through raw handles: byte = element × stride +
offset, the stride from the used-resource entry (tables) or the view-stride
table (heap). `getDimensions` of a structured buffer divides the byte count
by the stride. A raw view made at an offset that is not 16-byte aligned
removes the device on this adapter (`DXGI_ERROR_INVALID_CALL`; found by
`structured_views_off_their_buffers_elements_get_their_own_copy`), so such
views stay structured and keep the old behaviour.

| Suite | Before (`cfddfa3`) | After |
|---|---|---|
| `vc-shader` unit tests, `--test-threads=1`, access-violation test skipped | 298 passed, 3 failed | 303 passed, 0 failed |
| `vc-gpu` unit tests | 231 passed | 232 passed |

Fixed on this adapter: `a_heap_buffer_read_by_element_takes_the_views_stride`
and `gathers_at_a_level_match_the_hardware`. New:
`heap_buffer_reads_by_element_follow_each_strides_layout` (structured strides
8 to 64, raw, `R32_UINT`, on hardware and WARP) and
`scalar_loads_cross_structured_buffer_elements` (SMEM reads across elements
of a 24-byte view).

**Null buffer views on a heap this adapter caps.** The shaders' clamp
(`HEAP_VIEWS - 1`) and the slot of the host's null buffer views
(`HEAP_VIEWS - 2`) are 2^21 - 2 and 2^21 - 1 slots up, and the adapter's
shader-visible heap holds 1,000,000: it refused the test harness's 2^21-slot
heap (`E_INVALIDARG`, which is why
`a_heap_buffer_read_where_no_buffer_view_is_reads_zeros` failed), and a
shader reading a heap-direct buffer at a slot that holds no buffer view was
sent to a slot that does not exist. The host now keeps the null SRV and UAV
in the heap's last two slots when `HEAP_VIEWS - 2` is not in the heap (the
mirrored guest slots drop from 934,464 to 934,462) and publishes the SRV's
index in the view-stride table's word `HEAP_VIEWS - 4` (`NULL_VIEW_WORD`);
`buffer_guard` reads it instead of a constant. `view_stride` also clamped
nothing: an index from data past the table read past a root view, which has
no bounds (found by the extended test below, a device removal). The test reads
a heap buffer at slots 1, 1,500,000 and `HEAP_VIEWS + 77`, none of which holds
a buffer view, on hardware and WARP, and gets zeros. A heap-direct *texture*
read at a wild index is not guarded and can still fault on this adapter.

Still failing or open, all unchanged from `cfddfa3`:

* `heap_direct_texture_reads_mark_their_heap_index` ends the test process
  with an access violation (Windows SDK 10.0.26100 `dxil.dll`; no newer
  validator is installed here).
* A raw load from a typed view wider than a dword is a raw handle on a typed
  descriptor and is addressed by the descriptor's element size on this
  adapter; the title reads typed views with typed loads.
* Compute programs that declare `WaveSize=64`: 25 of the 50 in the corpus now
  run on this adapter as two host waves of 32 (section below); the other 25
  keep the declaration, which a 32-lane adapter refuses (`E_INVALIDARG`).
* The 2^20-iteration loop guard (`LOOP_LIMIT`) is still in place: an
  unbounded loop would hang the GPU until the driver resets it.
* Not run against the title in this session: the commit headroom was about
  16 GB against the 22 GB a world run took on this machine (15.5 GB of RAM,
  a 33 GB commit limit).

## Wave-64 compute programs on a 32-lane adapter, 26 November 2026

Measured on the RTX 5060 (waves of 32 only) by D3D12 dispatch and readback
(`emit/split_tests.rs`) and by the host-pipeline pass over a Series X shader
corpus (`vc translate <corpus> --validate --pipelines --list --jobs 4`).

**Change.** A compute program the translator marked `WaveSize(64)` now runs
as host waves of 32, each holding one 32-lane half of the console's wave
(guest lane = thread id in the group & 63, host lane = lane inside its half),
when that is exact; it declares `WaveSize(32)` (none when it reads no lane).
Where it is not exact the program keeps `WaveSize(64)` as before. The
translator decides per program (`lower/split.rs`, `split_plan`):

* `ds_swizzle` (groups of 32 lanes), `ds_bpermute` (RDNA 2 ISA 10.4.4: lanes
  0-31 and 32-63 independently) and DPP (rows of 16) stay inside a half: host
  wave operations on the half's wave.
* `v_readlane_b32` of a constant lane: that lane's thread stores the value in
  group-shared memory, a group barrier orders it, every thread of its console
  wave reads it (a second barrier on a loop, so the next pass's store waits).
  Allowed only where every thread of the group arrives (no lane-dependent
  branch into the block, not in a loop whose lanes leave at different times),
  where a barrier is defined.
* LDS shared by both halves (the console compiler leaves the barriers out of
  a one-wave group): a group barrier before an LDS access that conflicts with
  one since the last barrier (a write after any access, a read after a write),
  in a block every thread enters, or at the end of such a block before lanes
  part ways. A hazard inside code only some lanes run has no place for a
  barrier: the program keeps 64 lanes.
* `v_readfirstlane_b32` is the first lane of the host wave, so it needs a
  value every lane holds alike (made from scalars, constants and uniform
  loads by instructions every lane runs; a waterfall loop's read is the
  lane's own). Else the program keeps 64 lanes, as it does for `v_writelane`,
  lane masks held as numbers, a lane read at a computed lane, and an LDS
  hazard inside code only some lanes run.
* Added 27 November 2026, each only where every thread of the group arrives:
  * Whole-wave sections (the compiler's reductions and prefix sums, which
    `wave.rs` simulates): each half runs the host wave operation on its 32
    lanes, the halves' totals meet in group-shared memory behind a group
    barrier, and the result is the sum, bit operation, minimum or maximum of
    both (a prefix: the upper half adds the lower half's total). Integer
    operations only; float reductions and 16-bit sums keep 64 lanes.
  * A lane mask read as a number (`s_bcnt1`, `s_ff1`, `s_cmp` on EXEC or a
    saved mask): the ballot of each half is exchanged and read as the low and
    high dword, where the mask is made.
  * Global memory written and also read: refused only when a load can run
    after a store (a later block, or a loop). The console runs a wave's
    instructions in order, so a load that precedes every store sees nothing
    the other lanes wrote; the halves agree there.
  * `v_mbcnt_lo` and `v_mbcnt_hi`: the lane is the console lane (thread id &
    63, not the lane in its half) and the mask dwords are the exchanged
    ballots (or an all-ones constant), so the upper half counts the lower
    half's lanes too.
  * `v_readfirstlane_b32` of `v_lshrrev_b32 v, k, v0` with k of 6 or more in a
    1-D group: the console wave's index, the same in both halves.
  * The exchanges share one pair of group-shared dwords per console wave, each
    bracketed by two group barriers (store, read).

**Evidence.** Fourteen new tests dispatch wave64 programs on the 32-lane
device and compare with a CPU model of the console's 64 lanes: swizzles (nine
masks and quad permutations, with and without EXEC narrowed to lanes 0-39, in
a 1-D and an 8 x 8 group), DPP (nine row patterns, a DPP8 permutation, row
and bank masks, with and without EXEC), constant-lane reads of lanes 0, 5, 31, 32, 39, 40 and 63
(also in a loop and in a 128-thread group of two console waves), LDS written
by one half and read by the other (write, read, write, read; read by lanes
0-7 in a branch; a loop of exchanges: the number of barriers is asserted),
`v_readfirstlane` of a uniform value, and the cases that must keep 64 lanes
(a lane read in divergent flow or in a loop whose lanes leave at different
times, `v_mbcnt`, `v_readfirstlane` of a lane's value, memory written and
read by lanes, an LDS hazard inside a branch, a scalar from a lane's value).
Replacing the half base of a swizzle's source lane by 0 fails the swizzle test
(second half reads the first's lanes), and without the check on lanes that
leave a loop at different times the divergent-loop program would have been
split (both found by running them). The two existing wave64 `ds_bpermute`
tests (`wave64_permutes_*`), which skipped on this adapter, now run on it and
pass.

| Suite | Before | After |
|---|---|---|
| host pipelines over the corpus, compute | 113 of 163 | 138 of 163 |
| host pipelines over the corpus, vertex and pixel, mesh | 979 of 979, 36 of 36 | same |
| `vc translate` of the corpus with `--validate` | 1,179 of 1,179 translated, 2 validator rejections, 1 emit refusal | same |
| `vc-gpu` unit tests, `--test-threads=1` | 231 passed | 232 passed (a test the tip added) |
| `vc-shader` unit tests, `--test-threads=1`, access-violation test skipped | 302 passed, 1 failed | 317 passed, 0 failed |

(The failed test before was the 2^21-slot heap harness case, fixed in the
tip by the null-view change; the 14 new tests are the rest of the difference.)

**Split now (25 programs).** 16 that use `ds_swizzle` and nothing else (8 x 8
groups, straight-line code), 8 straight-line ones that also read lanes 0, 4,
32 and 36 with `v_readlane_b32` (8 x 8 groups), 1 with three reads of lane 32
(8 x 8, three blocks).

**Still declaring 64 lanes (25 programs), by what stops the split:**

| Programs | Stop |
|---|---|
| 8 LDS-only (16 x 4, 32 x 2, 64 x 1 groups) | `v_readfirstlane` of a buffer load or a float-to-integer conversion that the analysis does not show is the same in every lane; their LDS accesses also sit in `if (lane < N)` branches with a hazard inside |
| 2 LDS-only (8 x 8) | an LDS hazard inside code only some lanes run (one has LDS atomics) |
| 2 LDS-only (8 x 8, 384 bytes) | barriers placeable (1 before an access, 2 at ends of blocks), but the program stores to global memory and also loads through descriptors the used-resource list does not show to be shader resources; lane-private addresses are not proven |
| 2 (64 x 1, three blocks) | a global store and a load through a descriptor not shown to be a shader resource (one of them reads and writes one buffer through the same descriptor), addresses not proven lane-private |
| 2 | `v_readlane_b32` in a block not every thread enters (one with DPP and LDS) |
| 4 | `v_mbcnt` (three with LDS, a barrier and `v_readfirstlane`; one large) |
| 1 | a lane mask read as a number (88 ballots, LDS atomics) |
| 4 | whole-wave sections (the compiler's reductions and prefix sums over 64 lanes) |

None of the 25 was made by skipping an instruction, faking a result or
limiting a loop. Not tried: two guest lanes per host thread (every register
would double), if-converting branches so a barrier can sit inside them, and
proving lane-private addresses. Not run on an AMD adapter: the programs
split here declare 32 lanes (or none) there too, instead of the 64 they
declared before.

## World cutscene on the NVIDIA host, 27 November 2026

Measured with a diagnostics build of `6b8df38` plus the changes below, by the
title's own caption and the VC statistics line (5-second windows), on the
RTX 5060 (8 GB, 32-lane waves, driver 610.88), a Ryzen 5 7500F (12 threads),
15.5 GB of RAM and a 33 GB commit limit. The profile, shader cache and
pipeline library were outside the workspace. First boot's Privacy Policy and
Terms of Service screens were accepted on the user's instruction. The
pipeline library was warmed with `translate --cache --pipelines` for the
2,038 pipelines an earlier world run had drawn (1,268 made, 891 already in
the library), so the title starts them while it loads.

| Phase | Frame rate and picture | Where the time went |
|---|---|---|
| Menus, Series X | 30 FPS (33.3 ms). Menus and first-boot screens as on the console. | 0.3 to 1.3 ms preparing, 0.7 to 1.6 ms waiting for the GPU. GPU 4.2% busy on average, 5.0 GB of video memory plus 1.9 GB of GPU allocations in system memory. Process commit 13.0 GB; the system's 33 GB limit had 1.0 to 1.3 GB left. |
| World, Series X, empty pipeline library | 0.1 to 1.2 FPS (0.8 to 17.7 s a frame). Letterbox and interface over a flat dark blue, then over black. | 440 to 5,700 draws a frame, 550 to 4,565 dropped without a host pipeline: about 1,000 to 1,200 were queued and the 5 to 6 workers made about 1 a second. Five threads ran at 92 to 95% of a core and one at 66% (probably the 6 pipeline workers; the threads were not named in that run), the title's own threads waited (about 5% each). Submission preparation and GPU wait were 0.3 to 1.4 s of those frames. 7.6 of 8.1 GB of video memory, 5.6 GB of GPU allocations in system memory. |
| World, Series X, warm library | First world frames 1.8 FPS, then 0.1 FPS (11.7 s a frame, 5,270 draws, 260 dropped, 2.0 GB of title memory copied in a frame). | The title ended itself 3 minutes in (cause 5, which also ended three Series S runs). |
| Menus, Series S | 30 FPS. | Process commit 9.3 to 10 GB, 6.6 GB of commit free. |
| World, Series S, before the changes (39 windows) | 0.73 FPS on average (0.00 to 2.0), 3.83 s a frame. Letterbox and interface over black. | 1,844 draws a frame, 701 dropped, 835 indirect dropped; 315 ms preparing, 272 ms waiting for the GPU; GPU 25% busy on average with peaks of 100%, 6.6 GB of video memory. |
| World, Series S, after (34 windows, final code) | 1.05 FPS on average (0.1 to 3.9), 2.44 s a frame on average (an earlier run of the same code without the log lines: 1.05 FPS, 1.22 s). Same picture. About 170 s in the world, no title crash. | 2,498 draws a frame, 510 dropped, 215 indirect dropped; 220 ms preparing, 310 ms waiting for the GPU; dynamic descriptor slots 15,460 to 22,171 of 196,608 in use. The frame rate is within the run-to-run spread of the "before" row: the change shows in the draws dropped and run, not in FPS (different cutscene moments, not a controlled comparison). |

Nothing in this table shows the cutscene's 3D scene: every run ended with the
letterbox and credits or title card over a flat colour. Controllable gameplay
was not reached.

**Causes found, in the order of the evidence**

1. *Dynamic descriptor slots ran out every frame (fixed).* The log said
   `no dynamic slots for root views` and `for its tables`: in a Series S
   world run these reasons were more than half of the draws and indirect
   executions dropped (sums of the 5-second lines: 19,860, 4,511, 4,228 and
   2,642 for the four largest), among them most mesh draws. The host heap holds at most
   1,000,000 slots on this adapter, so 65,536 are left for dynamic ones
   (196,608 in Series S mode). Each title list took a run of 256 slots for its
   root views and kept it until the title reset the list; a frame closes
   about 240 lists and keeps them for several frames. Now `State::finish`,
   called when the title closes a list, gives back the part of the run no
   root view took, and a run is taken shorter when the heap has no whole one
   free (the slots given back leave holes). After the change
   `no dynamic slots for root views` no longer appears; a few draws (sums of
   94, 76 and 64) still miss slots for a fresh table, which needs a run of
   its own.
2. *Pipelines not made (library cold).* `dropped N without a host pipeline
   (M being made)`: see the table. Building the library first is the fix
   (`translate --cache --pipelines`, early list first, about 4 pipelines a
   second on 10 jobs).
3. *Compute pipelines refused by the host (150 distinct in the runs before
   the splitter change below, 144 in r14 after it).* All of them declare
   `WaveSize(64)` (the refusal logs the declared wave size, read from the
   container's `PSV0` part). `vc translate` with `VC_SPLIT_REPORT=1 --list`
   names, for each wave64 program, what kept it at 64 lanes.
4. *Dispatches dropped for their root view, and the runaway behind them.* Nine
   compute programs, 3 to 5 a frame each, are dropped with
   `a root view's offset: no root view, or outside title buffers`. The log
   (now with the slot and what the title set there) shows the title passing
   the pointer as 14 root constants in slot 8 (the first two dwords are the
   64-bit address), where the translator, which sees only user registers,
   called the slot a root view. Taking the address from the constants runs
   those dispatches (`VC_ROOT_CONSTANT_ADDRESS=all`, diagnostics builds only;
   off otherwise) and then a host list of 4,616 commands ran for 31 s and
   removed the device (`DXGI_ERROR_DEVICE_HUNG`) at the first world frame,
   twice (r10, r11); without it the world ran (r12). Run r13 repeated the
   hang with device-removed data and per-list timing: a list of 4,653
   commands ran 14.4 s, and the next submission removed the device at
   command 4,386 of 7,616, in the first of a series of dispatches of
   `5552dac6a2b7a1fd` (1,055 groups, then 9, 2, 2, ...). (Per-command timing
   covered only a list's first 4,096 commands then, so it could not time the
   guilty one; it covers 16,384 now.)
   Every program in the story (the nine, the hung one, the two that run
   away) is one of the 52 compute programs of `XboxBvhBuilder.dll`, the
   title's acceleration-structure builder (found with `vc shader-info
   --verbose --disasm`, which now prints each container's hash). The hung
   `5552dac6…` is a bottom-up pass: each thread climbs the tree from its
   leaf, adds 1 to its parent's counter (`buffer_atomic_add`, stopping when
   it was first) and merges boxes, until the root; its loops are bounded by
   data only (the translator cuts a loop at 2^20 passes per lane). Six of the
   52 programs declare `WaveSize(64)` and were refused by this adapter, among
   them the two most dispatched in that list (`caa78b8c…` 2,226 and
   `28918b71…` 1,134 dispatches: their code extracts digits and sums them
   across the wave, probably radix-sort passes). Inference made after r13 (not borne out, see r16 below): with those
   passes missing the tree the climbing pass reads is not the one the title
   built, and its lanes climb to the loop cap. What the runs show is that
   the dispatches stay dropped by default, and that the hang needs them to
   run. One more program (slot 2, constants 1, 512, 0xFFFFFFFF) does not
   take an address there either.
5. *The title ends itself.* In 4 of the 8 world runs that did not lose the
   device (runs r4, r5, r6 and r9, 45 to 308 s into the world) the title
   stopped with an access violation writing address 0 at the same place of the
   mapped image, on a job worker: a deliberate fatal path reached after a
   check of 64 timestamps against a time limit (the registers were the same in
   every run). Its own context log names the intro cutscene in its pre-stream
   state, 663 streaming requests and 3.0 of 4.0 GB of tile heap in use
   (console type Lockhart, i.e. Series S, in the last one). The upload of its
   report was refused by the offline policy. What it waits for is not
   established: an 8-minute Series X run at 0.1 to 1.2 FPS (r3) and a Series S
   run of 205 s (r12) did not trigger it, and frames of 3 to 17 s occurred in
   both kinds of run.
6. *Serial submission.* 18 to 41 host submissions a frame, each waited for
   (`ExecuteCommandLists`, `Signal`, an event wait). Preparing (150 to 440 ms
   a frame, 220 to 470 MB of title memory copied and compared) and the GPU
   wait (300 to 560 ms) never overlap, and the GPU is idle between them.
7. *Host memory.* Series X mode does not fit the 33 GB commit limit (see the
   matrix); Series S mode does, but its world run still left 0.3 to 2.4 GB.

**Changes made with these measurements**

* `vc-gpu::draw`: `State::finish` and `trim_run` (the unused tail of a
  root-view run goes back at `Close`), shorter runs when the heap is
  fragmented, a log line (slot, what the title set, pipeline) for each failing
  root-view offset. New test
  `a_closed_list_gives_back_the_unused_tail_of_its_root_view_run`.
* `vc-gpu::pipeline::host`: a refused compute pipeline logs its declared
  wave size (`declared_waves`, tested).
* `vc-gpu::descriptor`, `host`: `dynamic descriptor slots in use: N of M` in
  the statistics.
* `vc-shader::emit::lower::split`: `split_plan` returns its reason;
  `split_stops` and `VC_SPLIT_REPORT` report them, and `vc translate --list`
  names the reason per program. The splitter now also runs, over both halves
  of a console wave, whole-wave integer reductions and prefix sums, lane masks
  read as numbers, and programs whose loads cannot follow a store (see
  "Wave-64 compute programs on a 32-lane adapter"). Three hardware tests:
  `wave64_a_prefix_sum_over_the_whole_wave_adds_the_lower_half` (64, 128 and
  192 threads), `wave64_a_reduction_over_the_whole_wave_sums_both_halves`
  and `wave64_a_lane_mask_read_as_a_number_holds_both_halves` (64 and 128
  threads).
* `vc-shader::emit::lower::split`: `v_mbcnt` and `v_readfirstlane` of the
  wave index split (test `wave64_mbcnt_counts_the_lanes_of_both_halves_below_the_thread`;
  `what_the_split_cannot_do_keeps_the_wave_of_64` no longer lists `v_mbcnt`).
  `VC_SPLIT_TRUST_PRIVATE_LDS` (diagnostics builds): takes an LDS hazard in
  code only some lanes run as no hazard, the LDS being each thread's own;
  unsound where lanes share it, so an experiment only, and its translations
  are cached apart (`shader_cache::build`).
* `vc-gpu::draw`: `VC_ROOT_CONSTANT_ADDRESS` (diagnostics builds; pipeline
  names or `all`) takes a root view's address from root constants, to
  reproduce the hang above; `vc-gpu::host`: per-command timing covers 16,384
  commands a list instead of 4,096. `vc shader-info --verbose` prints each
  shader's hash.

Run r15 repeated the r13 experiment on the new build
(`VC_ROOT_CONSTANT_ADDRESS=all`, Series S, 224 s): a list of 4,642 commands
ran 14.6 s and the device was removed in the next submission, at command 4,320
of 7,597, the first of the same series of dispatches of `5552dac6a2b7a1fd`
(1,055 groups, then 9, 2, 2, ...). Splitting 3 of the 6 refused programs did
not change that, as expected while `caa78b8c…` and `28918b71…` are still
refused; it is not a test of the inference above. (The timestamps of the
removed device read 0 again, so the guilty command is named by its breadcrumb,
not by its time.) The switch stays off by default.

Run r16 asked the question the inference raised: with
`VC_ROOT_CONSTANT_ADDRESS=all` and `VC_SPLIT_TRUST_PRIVATE_LDS=1` the two
refused programs that dominate the chain (`caa78b8c…` and `28918b71…`, which
use LDS as a private 96-byte array per thread, written inside per-lane loops
where no group barrier can go) ran split: in the list that hung, 2,228 and
1,126 dispatches of them ran, 8,176 BVH dispatches in all, and only
`e63d22c4…` (8 dispatches) was still refused. The hang came back at the same
place: a list of 4,655 commands ran 26.4 s (14.6 s before) and the device was
removed at command 4,400 of 9,308, the first dispatch of `5552dac6…` (1,055
groups). So a complete chain is not what the climbing pass needs; what it
needs is unknown. (The two programs ran under an assumption, so this does not
say they computed what the console's would.) Left to find: what the pass
reads that makes its lanes climb to the loop cap, for instance the title's
own buffer contents at that point (a parent array, counters not cleared), or
a loop whose exit the translation changes. Lifting the LDS hazard soundly
needs bounds on the loop counters that index each thread's array; the
experiment says it would not remove the hang, so it was not built.

Over the Series X BVH builder DLL (52 programs), the programs that kept 64
lanes went from 9 to 6 (`vc translate --list`, `VC_SPLIT_REPORT=1`): 3 of
the 6 refused by the host split now (`14394a32…`, `5e19e7c7…`, `f6eb6498…`).
Still at 64 lanes: `caa78b8c…` and `28918b71…` (an LDS hazard inside code
only some lanes run), `e63d22c4…` (a `v_readlane` at a computed lane) and 3
programs not dispatched in the runs (`v_readfirstlane` of other values).
Run r14 (Series S, default settings, library warmed for that translator, 360 s,
no crash or device removal): 22 world windows, 0.68 FPS on average (0.1 to
1.4), 2,956 draws a frame, 680 dropped without a host pipeline, 274 indirect
dropped, 144 distinct pipelines refused (150 before). Compared with the
earlier runs that is within their spread (1.05 FPS, 3,884 draws; 0.73 FPS,
1,844 draws): no gain or loss is claimed from it. The `v_mbcnt` and
`v_readfirstlane` lifts came after r14, so the final build had its own run, r18
(Series S, default settings, library and shader cache warmed for that
translator, 380 s): no crash, no device removal, no new crash log from the
title's own watchdog; 25 world windows at 1.14 FPS on average (0.5 to 2.3),
3,186 draws a frame, 454 dropped without a host pipeline, 218 indirect
dropped, 141 distinct pipelines refused (150 before the splitter changes,
144 in r14). Again within the earlier runs' spread, so no gain or loss is
claimed; the point is that nothing regressed. (A first attempt, r17, was
stopped by the commit watchdog at the menus: 17.4 GB of commit free at the
start, about 21 needed.)

| Suite | Before | After |
|---|---|---|
| `vc-gpu` unit tests, `--test-threads=1` | 232 passed | 234 passed, 0 failed, 1 ignored |
| `vc-shader` unit tests, `--test-threads=1`, access-violation test skipped | 317 passed, 0 failed | 321 passed, 0 failed |
