//! The console's wave of 64 on a host device of 32 lanes: compute programs
//! that declare `WaveSize(64)` (the console's wave) run as host waves of 32,
//! each holding one 32-lane half of the console wave, where every lane
//! exchange stays inside a half (`Lower::split`). Each test dispatches a
//! synthetic wave64 program on the hardware adapter and compares what it
//! stores with a CPU model of the console's 64 lanes (RDNA 2 ISA: DPP rows of
//! 16 lanes, `ds_swizzle` groups of 32, section 10.4.4 for permutes); the
//! ones the split cannot make exactly keep declaring 64 lanes (and 32-lane
//! devices refuse them).

use super::crosslane_tests::{ds_swizzle_b32, encode_dpp16, v_add_nc_u32_dpp8, v_mov_b32_dpp8};
use super::host::{Dispatch, Host, Root, Stride, View};
use super::{HostShader, lower, validator};
use crate::gfx10::Dpp;
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// Lanes inside a half are told apart by this base: lane `l` holds
/// `VALUE + l` in v1 and `OLD + l` in v3 (what an inactive lane keeps).
const VALUE: u32 = 0x7000_0000;
const OLD: u32 = 0xCAFE_0000;

/// `v_add_nc_u32 v<dst>, <literal>, v0`.
fn v_add_lit(dst: u32, lit: u32) -> [u32; 2] {
    [0x4A00_00FF | dst << 17, lit]
}

/// What `build` returns: the program as the host's compute pipeline takes
/// it, and what the translator decided.
struct Built {
    shader: HostShader,
    lowered: lower::Lowered,
}

/// A wave64 compute program of `threads` threads: it loads the output
/// buffer's descriptor, sets `v1 = VALUE + lane` and `v3 = OLD + lane` (lane =
/// v0, so 1-D groups), runs `body`, and stores v3 at `lane * 4`.
fn build(threads: [u32; 3], body: &[u32]) -> Built {
    let mut words = vec![
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0A02,
        0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
    ];
    words.extend(v_add_lit(1, VALUE)); // v_add_nc_u32 v1, VALUE, v0
    words.extend(v_add_lit(3, OLD)); // v_add_nc_u32 v3, OLD, v0
    words.extend_from_slice(body);
    words.extend_from_slice(&[
        0x3420_0082, // v_lshlrev_b32 v16, 2, v0
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xE070_1000,
        0x800A_0310, // buffer_store_dword v3, v16, s[40:43], 0 offen
        S_ENDPGM,
    ]);
    build_words(threads, &words)
}

/// The program of `words` as a wave64 compute program of `threads` threads.
fn build_words(threads: [u32; 3], words: &[u32]) -> Built {
    build_lds(threads, 0, words)
}

/// The same with `granules` of 512 bytes of LDS.
fn build_lds(threads: [u32; 3], granules: u32, words: &[u32]) -> Built {
    let range = |kind, table_offset_dwords, dwords, stride| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride,
        dwords,
        user_data: 0,
        slot: 0,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 32,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![
            range(ResourceType::Srv, 0, 8, 0),
            range(ResourceType::Srv, 8, 4, 16),
            range(ResourceType::Uav, 16, 4, 0),
        ],
        io: StageIo::Cs(ComputeIo {
            // Two user SGPRs' worth is the table pointer; v0 and v1 (x, y)
            // for groups that are not 1-D.
            rsrc2: 1 << 1 | u32::from(threads[1] > 1) << 11 | granules << 15,
            threads,
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        view_marks: false,
    })
    .expect("lower");
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil.clone(),
        user_data: lowered.user_data.clone(),
        static_samplers: Vec::new(),
        samplers: lowered.samplers.clone(),
        root_cbvs: lowered.root_cbvs.clone(),
        root_srvs: lowered.root_srvs.clone(),
        root_uavs: lowered.root_uavs.clone(),
        memory: lowered.memory,
        memory_writes: lowered.memory_writes,
        view_addresses: lowered.view_addresses,
        heap: lowered.heap,
        heap_writes: lowered.heap_writes,
        view_marks: lowered.view_marks,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    }
    Built { shader, lowered }
}

/// Runs `shader` on the hardware adapter, one group, and returns the first
/// `dwords` of its output; `None` without a device. The adapter must make
/// the pipeline: a device of 32 lanes runs the split program.
fn run(shader: &HostShader, dwords: u32) -> Option<Vec<u32>> {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; skipped");
        return None;
    };
    let (root, pso) = host
        .compute_pipeline(shader)
        .unwrap_or_else(|e| panic!("the wave64 program has no pipeline on this adapter: {e}"));
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let views = [
        View::Texture(1, 1, &[[0.0; 4]]),
        View::Buffer {
            data: &[0; 4],
            stride: Stride::Mirrored(16),
        },
        View::RawUav(dwords),
    ];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &views,
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][..dwords as usize].to_vec())
}

/// The console's `ds_swizzle_b32` on 64 lanes: source of `lane` for
/// `offset` (quad permutation when bit 15 is set, else and/or/xor masks of
/// the lane's index in its group of 32).
fn swizzle_source(offset: u32, lane: u32) -> u32 {
    if offset & 0x8000 != 0 {
        (lane & !3) | (offset >> (2 * (lane & 3))) & 3
    } else {
        let (and, or, xor) = (
            offset & 0x1F | !0x1F,
            offset >> 5 & 0x1F,
            offset >> 10 & 0x1F,
        );
        (lane & !31) | (((lane & and) | or) ^ xor) & 31
    }
}

/// What 64 lanes hold after `ds_swizzle_b32 v3, v1, offset`: lanes outside
/// `exec` keep v3, lanes read 0 from sources outside it.
fn swizzled(offset: u32, exec: u64) -> Vec<u32> {
    (0..64u32)
        .map(|lane| {
            if exec >> lane & 1 == 0 {
                return OLD + lane;
            }
            let src = swizzle_source(offset, lane);
            if exec >> src & 1 != 0 { VALUE + src } else { 0 }
        })
        .collect()
}

/// The swizzles tried: and/or/xor masks across the 16, 8 and 4 lane
/// distances and a reversal of a whole half, a broadcast of lane 5, and quad
/// permutations (reverse, broadcast of lane 2, rotate).
const OFFSETS: [u16; 9] = [
    0x401F,
    0x201F,
    0x101F,
    0x7C1F,
    0x00A0,
    0x8000 | 0x1B,
    0x8000 | 0xAA,
    0x8000 | 0x39,
    0x1C1E,
];

/// A program swizzling `v1` by each of `OFFSETS` into its own VGPR (starting
/// from the lane's old value, `OLD + lane`), under the `exec` narrowing the
/// caller gives (and `restore` after it), then storing the VGPRs `64` dwords
/// apart: lanes store `swizzled(offset, exec)` for each.
fn swizzle_program(exec: &[u32], restore: &[u32], threads: [u32; 3]) -> Built {
    let mut words = vec![
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0A02,
        0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
    ];
    words.extend(v_add_lit(1, VALUE)); // v_add_nc_u32 v1, VALUE, v0
    words.push(0x3420_0082); // v_lshlrev_b32 v16, 2, v0
    for k in 0..OFFSETS.len() as u32 {
        words.extend(v_add_lit(20 + k, OLD)); // what an inactive lane keeps
    }
    words.extend_from_slice(exec);
    for (k, &offset) in OFFSETS.iter().enumerate() {
        words.extend(ds_swizzle_b32(20 + k as u8, 1, offset));
    }
    words.push(0xBF8C_C07F); // s_waitcnt lgkmcnt(0)
    words.extend_from_slice(restore);
    for k in 0..OFFSETS.len() as u32 {
        // buffer_store_dword v<20+k>, v16, s[40:43], 0 offen offset:k*256
        words.push(0xE070_1000 | k * 256);
        words.push(0x800A_0010 | (20 + k) << 8);
    }
    words.push(S_ENDPGM);
    build_words(threads, &words)
}

#[test]
fn wave64_swizzles_run_as_two_host_waves_of_32() {
    let built = swizzle_program(&[], &[], [64, 1, 1]);
    // Every exchange stays inside a half: the split needs no 64-lane wave.
    assert_eq!(built.lowered.wave_size, Some(32));
    let Some(got) = run(&built.shader, 64 * OFFSETS.len() as u32) else {
        return;
    };
    for (k, &offset) in OFFSETS.iter().enumerate() {
        let want = swizzled(u32::from(offset), u64::MAX);
        assert_eq!(
            &got[k * 64..(k + 1) * 64],
            &want[..],
            "offset {offset:#06x}"
        );
    }
}

#[test]
fn wave64_swizzles_read_zero_from_lanes_exec_leaves_out_in_each_half() {
    // Lanes below 40 run them: lanes 32..39 of the second half read sources
    // among lanes 40..63 (off), lanes of the first half read only their own.
    let exec = [
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE94_246A, // s_and_saveexec_b64 s[20:21], vcc
    ];
    let restore = [0xBEFE_0414]; // s_mov_b64 exec, s[20:21]
    let built = swizzle_program(&exec, &restore, [64, 1, 1]);
    assert_eq!(built.lowered.wave_size, Some(32));
    let Some(got) = run(&built.shader, 64 * OFFSETS.len() as u32) else {
        return;
    };
    for (k, &offset) in OFFSETS.iter().enumerate() {
        let want = swizzled(u32::from(offset), (1 << 40) - 1);
        assert_eq!(
            &got[k * 64..(k + 1) * 64],
            &want[..],
            "offset {offset:#06x}"
        );
    }
}

/// A group that is not one-dimensional: v0 and v1 are x and y of an 8 × 8
/// group, the lane is x + 8 y, and swizzling by xor 16 pairs the rows
/// (y, y ^ 2) of each half.
#[test]
fn wave64_lanes_of_a_two_dimensional_group_are_the_consoles() {
    let mut words = vec![
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0A02,
        0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
        0x3404_0283, // v_lshlrev_b32 v2, 3, v1 (8 y)
        0x4A0A_0102, // v_add_nc_u32 v5, v2, v0 (x + 8 y: the lane)
        0x4A04_0AFF,
        0x0000_1000, // v_add_nc_u32 v2, 0x1000, v5
    ];
    words.extend(ds_swizzle_b32(3, 2, 0x401F)); // v3 = v2 of lane ^ 16
    words.extend_from_slice(&[
        0x3420_0A82, // v_lshlrev_b32 v16, 2, v5
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xE070_1000,
        0x800A_0310, // buffer_store_dword v3, v16, s[40:43], 0 offen
        S_ENDPGM,
    ]);
    let built = build_words([8, 8, 1], &words);
    assert_eq!(built.lowered.wave_size, Some(32));
    let Some(got) = run(&built.shader, 64) else {
        return;
    };
    let want: Vec<u32> = (0..64u32).map(|flat| 0x1000 + (flat ^ 16)).collect();
    assert_eq!(got, want);
}

/// DPP: rows of 16 lanes (and quads, groups of 8) inside each half. The
/// model reads the console's lane `l` of 64.
fn dpp_source(dpp: Dpp, lane: u32, exec: u64) -> Option<u32> {
    // Some(source lane) where the lane reads (0 when the source is outside
    // EXEC, unless `fi`), None where the instruction leaves it alone.
    match dpp {
        Dpp::Lanes { sel, fi } => {
            let src = (lane & !7) | ((sel >> (3 * (lane & 7))) & 7);
            Some(if exec >> src & 1 != 0 || fi {
                src
            } else {
                u32::MAX
            })
        }
        Dpp::Row {
            ctrl,
            row_mask,
            bank_mask,
            bound_ctrl,
            fi,
        } => {
            if (row_mask >> (lane >> 4 & 3)) & 1 == 0 || (bank_mask >> (lane >> 2 & 3)) & 1 == 0 {
                return None;
            }
            let (row, pos, n) = (lane & !15, lane & 15, u32::from(ctrl & 15));
            let (src, inside) = match ctrl {
                0..=0xFF => (
                    (lane & !3) | (u32::from(ctrl) >> (2 * (lane & 3)) & 3),
                    true,
                ),
                0x101..=0x10F => (lane + n, pos + n < 16),
                0x111..=0x11F => (lane.wrapping_sub(n), pos >= n),
                0x121..=0x12F => (row | (pos.wrapping_sub(n) & 15), true),
                0x140 => (lane ^ 15, true),
                0x141 => (lane ^ 7, true),
                0x150..=0x15F => (row | n, true),
                0x160..=0x16F => (lane ^ n, true),
                _ => panic!("ctrl {ctrl:#x}"),
            };
            if inside && (exec >> src & 1 != 0 || fi) {
                Some(src)
            } else if bound_ctrl {
                Some(u32::MAX)
            } else {
                None
            }
        }
    }
}

/// What 64 lanes hold after a DPP `v_mov_b32 v3, v1`: lanes outside `exec`
/// and lanes the instruction does not write keep v3.
fn dpp_moved(dpp: Dpp, exec: u64) -> Vec<u32> {
    (0..64u32)
        .map(|lane| {
            if exec >> lane & 1 == 0 {
                return OLD + lane;
            }
            match dpp_source(dpp, lane, exec) {
                Some(u32::MAX) => 0,
                Some(src) => VALUE + src,
                None => OLD + lane,
            }
        })
        .collect()
}

#[test]
fn wave64_dpp_stays_inside_rows_of_each_half() {
    let row = |ctrl, row_mask, bank_mask, bound_ctrl, fi| Dpp::Row {
        ctrl,
        row_mask,
        bank_mask,
        bound_ctrl,
        fi,
    };
    let cases = [
        // quad_perm [3,2,1,0], row_shr:1 and row_shl:1 (bound_ctrl: edge reads 0),
        // row_ror:3 (wraps inside its row), row_mirror, row_half_mirror,
        // row_xmask:5, row_share:7, and a row and bank mask.
        row(0x1B, 0xF, 0xF, false, false),
        row(0x111, 0xF, 0xF, true, false),
        row(0x101, 0xF, 0xF, true, false),
        row(0x123, 0xF, 0xF, false, false),
        row(0x140, 0xF, 0xF, false, false),
        row(0x141, 0xF, 0xF, false, false),
        row(0x165, 0xF, 0xF, false, false),
        row(0x157, 0xF, 0xF, false, false),
        row(0x111, 0x5, 0xA, false, false),
        Dpp::Lanes {
            sel: 0b_001_000_011_010_101_100_111_110,
            fi: false,
        },
    ];
    let mut words = vec![
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0A02,
        0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
    ];
    words.extend(v_add_lit(1, VALUE));
    words.push(0x3420_0082); // v_lshlrev_b32 v16, 2, v0
    let exec = [
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE94_246A, // s_and_saveexec_b64 s[20:21], vcc
    ];
    for (name, masked) in [("every lane", false), ("lanes below 40", true)] {
        let mut w = words.clone();
        for k in 0..cases.len() as u32 {
            w.extend(v_add_lit(20 + k, OLD)); // what an inactive lane keeps
        }
        if masked {
            w.extend_from_slice(&exec);
        }
        for (k, &d) in cases.iter().enumerate() {
            match d {
                Dpp::Row { .. } => w.extend(encode_dpp16(20 + k as u8, 1, None, d)),
                Dpp::Lanes { sel, .. } => {
                    let mut s = [0u8; 8];
                    for (i, v) in s.iter_mut().enumerate() {
                        *v = (sel >> (3 * i) & 7) as u8;
                    }
                    w.extend(v_mov_b32_dpp8(20 + k as u8, 1, s, false));
                }
            }
        }
        if masked {
            w.push(0xBEFE_0414); // s_mov_b64 exec, s[20:21]
        }
        w.push(0xBF8C_C07F); // s_waitcnt lgkmcnt(0)
        for k in 0..cases.len() as u32 {
            w.push(0xE070_1000 | k * 256);
            w.push(0x800A_0010 | (20 + k) << 8);
        }
        w.push(S_ENDPGM);
        let built = build_words([64, 1, 1], &w);
        assert_eq!(built.lowered.wave_size, Some(32), "{name}");
        let Some(got) = run(&built.shader, 64 * cases.len() as u32) else {
            return;
        };
        let exec = if masked { (1u64 << 40) - 1 } else { u64::MAX };
        for (k, &d) in cases.iter().enumerate() {
            let want = dpp_moved(d, exec);
            assert_eq!(&got[k * 64..(k + 1) * 64], &want[..], "{name}: {d:?}");
        }
    }
}

/// `v_readlane_b32 s<dst>, v<src>, <lane>` (an inline-constant lane).
fn v_readlane(dst: u32, src: u32, lane: u32) -> [u32; 2] {
    [0xD760_0000 | dst, (128 + lane) << 9 | 0x100 | src]
}

/// `v_mov_b32 v<dst>, s<src>`.
fn v_mov_from_s(dst: u32, src: u32) -> u32 {
    0x7E00_0200 | dst << 17 | src
}

/// A program that reads `lanes` of v2 into SGPRs, then each lane stores them
/// one per 256 bytes; `exec` narrows EXEC after v2 is set (lanes keep `OLD +
/// lane` in v2 outside it; inside, v2 = `VALUE + lane`).
fn readlane_program(lanes: &[u32], threads: [u32; 3]) -> Built {
    let mut words = vec![
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0A02,
        0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
    ];
    words.extend(v_add_lit(2, OLD)); // v2 = OLD + lane
    words.push(0x3420_0082); // v_lshlrev_b32 v16, 2, v0
    words.extend_from_slice(&[
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE94_246A, // s_and_saveexec_b64 s[20:21], vcc
    ]);
    words.extend(v_add_lit(2, VALUE)); // lanes below 40: v2 = VALUE + lane
    words.push(0xBEFE_0414); // s_mov_b64 exec, s[20:21]
    for (k, &lane) in lanes.iter().enumerate() {
        words.extend(v_readlane(10 + k as u32, 2, lane));
        words.push(v_mov_from_s(20 + k as u32, 10 + k as u32));
    }
    words.push(0xBF8C_C07F); // s_waitcnt lgkmcnt(0)
    for k in 0..lanes.len() as u32 {
        words.push(0xE070_1000 | k * 256);
        words.push(0x800A_0010 | (20 + k) << 8);
    }
    words.push(S_ENDPGM);
    build_words(threads, &words)
}

/// `v_readlane_b32` reads a lane of either half: the lane's thread stores
/// the value, a barrier orders it, all 64 threads read it. Inactive lanes
/// are read too (they hold their old value).
#[test]
fn wave64_readlane_reads_a_lane_of_either_half() {
    let lanes = [0, 5, 31, 32, 39, 40, 63];
    let built = readlane_program(&lanes, [64, 1, 1]);
    assert_eq!(built.lowered.wave_size, Some(32));
    let Some(got) = run(&built.shader, 64 * lanes.len() as u32) else {
        return;
    };
    for (k, &lane) in lanes.iter().enumerate() {
        let want = if lane < 40 { VALUE + lane } else { OLD + lane };
        assert!(
            got[k * 64..(k + 1) * 64].iter().all(|&g| g == want),
            "lane {lane}: {:x?}",
            &got[k * 64..(k + 1) * 64]
        );
    }
}

/// Each console wave of a larger group reads its own lane: 128 threads are
/// two console waves, four host waves; lane 33 of the second wave is thread
/// 97.
#[test]
fn wave64_readlane_reads_the_lane_of_its_own_console_wave() {
    let built = readlane_program(&[33], [128, 1, 1]);
    assert_eq!(built.lowered.wave_size, Some(32));
    let Some(got) = run(&built.shader, 128) else {
        return;
    };
    // v2 = VALUE + v0 where v0 < 40 (a 128-thread group's v0 is the lane's
    // thread id in the group): in the second wave thread 97 (v0 = 97) is not
    // below 40 and keeps OLD + 97.
    for (t, &g) in got.iter().enumerate() {
        let w = t as u32 / 64;
        let v0 = w * 64 + 33;
        let want = if v0 < 40 { VALUE + v0 } else { OLD + v0 };
        assert_eq!(g, want, "thread {t}");
    }
}

/// A uniform loop reads the same lane every pass: the pass's stores must
/// not run ahead of the other wave's reads of the pass before.
#[test]
fn wave64_readlane_in_a_uniform_loop_waits_for_every_reader() {
    let mut words = vec![
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0A02,
        0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
    ];
    words.extend(v_add_lit(1, VALUE)); // v1 = VALUE + lane
    words.extend_from_slice(&[
        0xBE86_0380, // s_mov_b32 s6, 0
        0xBE88_0380, // s_mov_b32 s8, 0
        // loop:
        0x4A02_0290, // v_add_nc_u32 v1, 16, v1
    ]);
    words.extend(v_readlane(7, 1, 33)); // s7 = v1 of lane 33
    words.extend_from_slice(&[
        0x8008_0708, // s_add_u32 s8, s8, s7
        0x8006_8106, // s_add_u32 s6, s6, 1
        0xBF0A_8306, // s_cmp_lt_u32 s6, 3
        0xBF85_FFF9, // s_cbranch_scc1 loop
        0x7E06_0208, // v_mov_b32 v3, s8
        0x3420_0082, // v_lshlrev_b32 v16, 2, v0
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xE070_1000,
        0x800A_0310, // buffer_store_dword v3, v16, s[40:43], 0 offen
        S_ENDPGM,
    ]);
    let built = build_words([64, 1, 1], &words);
    assert_eq!(built.lowered.wave_size, Some(32));
    let Some(got) = run(&built.shader, 64) else {
        return;
    };
    let want = (VALUE + 33).wrapping_mul(3).wrapping_add(16 * (1 + 2 + 3));
    assert!(got.iter().all(|&g| g == want), "{got:x?}");
}

/// What the split cannot make exactly keeps the console's wave of 64 (a
/// 32-lane device refuses its pipeline); each program needs another lane
/// operation to declare it at all.
#[test]
fn what_the_split_cannot_do_keeps_the_wave_of_64() {
    let swizzle = ds_swizzle_b32(4, 1, 0x401F);
    // A lane read where the lanes of the group part ways: the barrier of
    // the exchange would be undefined.
    let mut divergent = swizzle.to_vec();
    divergent.extend_from_slice(&[
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE94_246A, // s_and_saveexec_b64 s[20:21], vcc
        0xBF88_0003, // s_cbranch_execz +3
    ]);
    divergent.extend(v_readlane(10, 1, 40));
    divergent.extend_from_slice(&[
        0x7E06_020A, // v_mov_b32 v3, s10
        0xBEFE_0414, // s_mov_b64 exec, s[20:21]
    ]);
    // A lane count below the lane: it names lanes of both halves.
    let mbcnt = [
        0xD765_0003,
        0x0001_00C1, // v_mbcnt_lo_u32_b32 v3, -1, 0
        0xD766_0003,
        0x0002_06C1, // v_mbcnt_hi_u32_b32 v3, -1, v3
    ];
    // A value of the first lane of the wave, which in the second half is
    // the first lane of that half.
    let mut first = swizzle.to_vec();
    first.extend_from_slice(&[
        0x7E00_0501, // v_readfirstlane_b32 s0, v1
        0x7E06_0200, // v_mov_b32 v3, s0
    ]);
    // Memory a lane wrote and a lane of the other half reads: the halves are
    // separate waves, which no barrier orders.
    let mut memory = swizzle.to_vec();
    memory.extend_from_slice(&[
        0x3420_0082, // v_lshlrev_b32 v16, 2, v0
        0xE070_1000,
        0x800A_0310, // buffer_store_dword v3, v16, s[40:43], 0 offen
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xE030_1000,
        0x800A_0510, // buffer_load_dword v5, v16, s[40:43], 0 offen
    ]);
    for (what, body) in [
        ("a lane read in divergent flow", &divergent[..]),
        ("v_readfirstlane of a lane's value", &first[..]),
        ("memory written and read by lanes", &memory[..]),
    ] {
        // (Each starts with a swizzle, which makes it need the console's lanes.)
        let built = build([64, 1, 1], body);
        assert_eq!(built.lowered.wave_size, Some(64), "{what}");
    }
    // `v_mbcnt` counts the lanes of both halves through the exchanged masks
    // and the console lane: it splits (`wave64_mbcnt_counts_*`).
    let built = build([64, 1, 1], &mbcnt);
    assert_eq!(built.lowered.wave_size, Some(32), "v_mbcnt");
}

/// `ds_write_b32 v<addr>, v<data>` and `ds_read_b32 v<dst>, v<addr>`.
fn ds_write(addr: u32, data: u32) -> [u32; 2] {
    [0x36 << 26 | 0x0D << 18, data << 8 | addr]
}

fn ds_read(dst: u32, addr: u32) -> [u32; 2] {
    [0x36 << 26 | 0x36 << 18, dst << 24 | addr]
}

/// The start of an LDS program (512 bytes of LDS, 64 threads): v1 = VALUE +
/// lane, v3 = OLD + lane, v4 = 0, v5 = lane × 4 and v6 = (lane ^ 32) × 4, the
/// address of the partner lane in the other half; then `body`, then stores
/// of v3 and v4 (64 dwords each, `v3` first).
fn lds_program(body: &[u32]) -> Built {
    let mut words = vec![
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0A02,
        0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
    ];
    words.extend(v_add_lit(1, VALUE));
    words.extend(v_add_lit(3, OLD));
    words.extend_from_slice(&[
        0x7E08_0280, // v_mov_b32 v4, 0
        0x340A_0082, // v_lshlrev_b32 v5, 2, v0
        0x3A0C_00A0, // v_xor_b32 v6, 32, v0
        0x340C_0C82, // v_lshlrev_b32 v6, 2, v6
    ]);
    words.extend_from_slice(body);
    words.extend_from_slice(&[
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xE070_1000,
        0x800A_0305, // buffer_store_dword v3, v5, s[40:43], 0 offen
        0xE070_1100,
        0x800A_0405, // buffer_store_dword v4, v5, s[40:43], 0 offen offset:256
        S_ENDPGM,
    ]);
    build_lds([64, 1, 1], 1, &words)
}

const WAIT: u32 = 0xBF8C_C07F; // s_waitcnt lgkmcnt(0)

/// Lanes of both halves share LDS with no barrier in the program, as the
/// console's compiler leaves it out for a group of one wave: each write the
/// other half reads, and each read the other half writes after it, needs one.
#[test]
fn wave64_lds_shared_by_both_halves_is_ordered_by_barriers() {
    let mut body = Vec::new();
    body.extend(ds_write(5, 1)); // lane's slot = VALUE + lane
    body.extend(ds_read(4, 6)); // v4 = the partner's slot
    body.push(WAIT);
    body.push(0x4A0E_0881); // v_add_nc_u32 v7, 1, v4
    body.extend(ds_write(5, 7)); // lane's slot = v4 + 1 (after the partner read it)
    body.extend(ds_read(3, 6)); // v3 = the partner's new slot
    let built = lds_program(&body);
    // No wave size (no lane exchanged by wave operations), a barrier before
    // the read, the write that follows it, and the last read.
    assert_eq!(built.lowered.wave_size, None);
    assert_eq!(built.lowered.barriers, 3);
    let Some(got) = run(&built.shader, 128) else {
        return;
    };
    for lane in 0..64u32 {
        assert_eq!(
            got[64 + lane as usize],
            VALUE + (lane ^ 32),
            "v4 of lane {lane}"
        );
        assert_eq!(got[lane as usize], VALUE + lane + 1, "v3 of lane {lane}");
    }
}

/// Lanes of one half read what the whole wave wrote, inside a branch only
/// they enter: the barrier goes where all lanes meet, before the branch.
#[test]
fn wave64_lds_read_by_a_few_lanes_is_ordered_before_their_branch() {
    let mut body = Vec::new();
    body.extend(ds_write(5, 1));
    body.extend_from_slice(&[
        0x7D88_0088, // v_cmp_gt_u32 vcc, 8, v0
        0xBE94_246A, // s_and_saveexec_b64 s[20:21], vcc
        0xBF88_0002, // s_cbranch_execz +2
    ]);
    body.extend(ds_read(4, 6)); // lanes 0..7 read lanes 32..39
    body.push(0xBEFE_0414); // s_mov_b64 exec, s[20:21]
    let built = lds_program(&body);
    assert_eq!(built.lowered.wave_size, None);
    assert_eq!(built.lowered.barriers, 1);
    let Some(got) = run(&built.shader, 128) else {
        return;
    };
    for lane in 0..64u32 {
        let want = if lane < 8 { VALUE + lane + 32 } else { 0 };
        assert_eq!(got[64 + lane as usize], want, "v4 of lane {lane}");
        assert_eq!(got[lane as usize], OLD + lane, "v3 of lane {lane}");
    }
}

/// A scalar made from a value every lane holds alike is the same in both
/// halves.
#[test]
fn wave64_readfirstlane_of_a_uniform_value_is_the_same_in_both_halves() {
    let body = [
        0x7E10_0287, // v_mov_b32 v8, 7
        0x7E12_0508, // v_readfirstlane_b32 s9, v8
        0x7E08_0209, // v_mov_b32 v4, s9
    ];
    let built = lds_program(&body);
    assert_eq!(built.lowered.wave_size, None);
    let Some(got) = run(&built.shader, 128) else {
        return;
    };
    assert!(got[64..].iter().all(|&g| g == 7), "{:x?}", &got[64..]);
}

/// What the LDS split cannot make exactly keeps the console's wave of 64:
/// lanes that part ways write and read each other's slots in a branch no
/// barrier can enter, and a scalar read from a value that differs by lane.
#[test]
fn what_the_lds_split_cannot_do_keeps_the_wave_of_64() {
    let mut inside = vec![
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE94_246A, // s_and_saveexec_b64 s[20:21], vcc
        0xBF88_0004, // s_cbranch_execz +4
    ];
    inside.extend(ds_write(5, 1));
    inside.extend(ds_read(4, 6));
    inside.push(0xBEFE_0414); // s_mov_b64 exec, s[20:21]
    let by_lane = [
        0x7E12_0501, // v_readfirstlane_b32 s9, v1
        0x7E08_0209, // v_mov_b32 v4, s9
    ];
    for (what, body) in [
        ("a hazard where only some lanes run", &inside[..]),
        ("a scalar from a value that differs by lane", &by_lane[..]),
    ] {
        let built = lds_program(body);
        assert_eq!(built.lowered.wave_size, Some(64), "{what}");
    }
}

/// A uniform loop of LDS exchanges: each pass writes the lane's slot and
/// reads the partner's, so the next pass's write must wait for every read
/// of this one (a barrier at the top of the loop as well as before the read).
#[test]
fn wave64_lds_in_a_uniform_loop_is_ordered_on_every_pass() {
    let mut body = vec![
        0xBE86_0380, // s_mov_b32 s6, 0
    ];
    // loop:
    body.extend(ds_write(5, 1));
    body.extend(ds_read(4, 6));
    body.extend_from_slice(&[
        WAIT,
        0x4A02_0901, // v_add_nc_u32 v1, v1, v4
        0x8006_8106, // s_add_u32 s6, s6, 1
        0xBF0A_8306, // s_cmp_lt_u32 s6, 3
        0xBF85_FFF7, // s_cbranch_scc1 loop
        0x7E06_0301, // v_mov_b32 v3, v1
    ]);
    let built = lds_program(&body);
    assert_eq!(built.lowered.wave_size, None);
    assert_eq!(built.lowered.barriers, 2);
    let Some(got) = run(&built.shader, 128) else {
        return;
    };
    // a(l) after each pass: a(l) + a(l ^ 32).
    let mut a: Vec<u32> = (0..64).map(|l| VALUE + l).collect();
    for _ in 0..3 {
        a = (0..64).map(|l| a[l].wrapping_add(a[l ^ 32])).collect();
    }
    assert_eq!(&got[..64], &a[..]);
}

/// A loop whose lanes leave at different times has no point where the
/// group meets: no barrier can go in it, and the lane read inside keeps the
/// console's wave.
#[test]
fn wave64_a_lane_read_in_a_loop_lanes_leave_at_different_times_keeps_the_wave_of_64() {
    let mut body = vec![
        0x3612_0083, // v_and_b32 v9, 3, v0
        0x4A12_1281, // v_add_nc_u32 v9, 1, v9   (passes the lane makes)
        0x7E0E_0280, // v_mov_b32 v7, 0
        0xBE94_047E, // s_mov_b64 s[20:21], exec
        // loop:
        0x4A0E_0E81, // v_add_nc_u32 v7, 1, v7
    ];
    body.extend(v_readlane(10, 1, 33));
    body.extend_from_slice(&[
        0x7D82_1307, // v_cmp_lt_u32 vcc, v7, v9
        0x87FE_6A7E, // s_and_b64 exec, exec, vcc
        0xBF89_FFFA, // s_cbranch_execnz loop
        0xBEFE_0414, // s_mov_b64 exec, s[20:21]
        0x7E06_020A, // v_mov_b32 v3, s10
    ]);
    let built = lds_program(&body);
    assert_eq!(built.lowered.wave_size, Some(64));
}

// ---- whole-wave sections and lane masks as numbers, over both halves ----

const fn sop1(sdst: u32, op: u32, ssrc0: u32) -> u32 {
    0xBE80_0000 | sdst << 16 | op << 8 | ssrc0
}

const fn vop1(vdst: u32, op: u32, src0: u32) -> u32 {
    0x7E00_0000 | vdst << 17 | op << 9 | src0
}

const fn vop2(vdst: u32, op: u32, src0: u32, vsrc1: u32) -> u32 {
    op << 25 | vdst << 17 | vsrc1 << 9 | src0
}

const fn vop3(vdst: u32, op: u32, src0: u32, src1: u32, src2: u32) -> [u32; 2] {
    [0xD400_0000 | op << 16 | vdst, src0 | src1 << 9 | src2 << 18]
}

/// An exclusive prefix sum of the thread ids over the console's 64 lanes, as
/// the console compiler expands it: the halves' sums meet through group-shared
/// memory, and each console wave of a larger group has its own.
#[test]
fn wave64_a_prefix_sum_over_the_whole_wave_adds_the_lower_half() {
    for threads in [64u32, 128, 192] {
        let mut body = super::wave::tests::exclusive_prefix_words(true);
        body.push(vop1(3, 0x01, 258)); // v_mov_b32 v3, v2
        let built = build([threads, 1, 1], &body);
        assert_eq!(built.lowered.wave_size, Some(32), "{threads} threads");
        let Some(got) = run(&built.shader, threads) else {
            return;
        };
        for t in 0..threads {
            let (wave, lane) = (u64::from(t / 64), u64::from(t % 64));
            let want = 64 * wave * lane + lane * lane.saturating_sub(1) / 2;
            assert_eq!(u64::from(got[t as usize]), want, "thread {t} of {threads}");
        }
    }
}

/// The sum over the console's 64 lanes, as the console compiler expands it
/// (DPP groups of 8 and the mirrored row, `v_permlanex16` across rows,
/// `v_readlane` of lane 32): every lane gets the wave's total.
#[test]
fn wave64_a_reduction_over_the_whole_wave_sums_both_halves() {
    let mut body = vec![sop1(0, 0x25, 193)]; // s_or_saveexec_b64 s[0:1], -1
    body.extend(vop3(1, 0x101, 128, 256, 0)); // v_cndmask_b32 v1, 0, v0, s[0:1]
    for sel in [
        [1, 0, 3, 2, 5, 4, 7, 6],
        [2, 2, 0, 0, 6, 6, 4, 4],
        [4, 4, 4, 4, 0, 0, 0, 0],
    ] {
        body.extend(v_add_nc_u32_dpp8(1, 1, 1, sel, false));
    }
    let mirror = Dpp::Row {
        ctrl: 0x140,
        row_mask: 0xF,
        bank_mask: 0xF,
        bound_ctrl: true,
        fi: false,
    };
    body.extend(encode_dpp16(1, 1, Some(1), mirror));
    body.extend(vop3(2, 0x378, 257, 128, 128)); // v_permlanex16_b32 v2, v1, 0, 0
    body.push(vop2(1, 0x25, 258, 1)); // v_add_nc_u32 v1, v2, v1
    body.extend(vop3(6, 0x360, 257, 128 + 32, 0)); // v_readlane_b32 s6, v1, 32
    body.push(vop2(1, 0x25, 6, 1)); // v_add_nc_u32 v1, s6, v1
    body.extend(vop3(6, 0x360, 257, 128, 0)); // v_readlane_b32 s6, v1, 0
    body.push(sop1(126, 0x04, 0)); // s_mov_b64 exec, s[0:1]
    body.push(vop1(3, 0x01, 6)); // v_mov_b32 v3, s6
    for threads in [64u32, 128] {
        let built = build([threads, 1, 1], &body);
        assert_eq!(built.lowered.wave_size, Some(32), "{threads} threads");
        let Some(got) = run(&built.shader, threads) else {
            return;
        };
        for t in 0..threads {
            let wave = t / 64;
            assert_eq!(
                got[t as usize],
                4096 * wave + 2016,
                "thread {t} of {threads}"
            );
        }
    }
}

/// `s_bcnt1_i32_b64` of EXEC counts the lanes of both halves: the halves'
/// ballots are exchanged and read as the low and high dwords of the mask.
#[test]
fn wave64_a_lane_mask_read_as_a_number_holds_both_halves() {
    // Lanes with 20 < v0 run; each gets the number of lanes that run.
    let body = [
        0x7D82_0094,         // v_cmp_lt_u32 vcc, 20, v0
        0xBE94_246A,         // s_and_saveexec_b64 s[20:21], vcc
        sop1(10, 0x10, 126), // s_bcnt1_i32_b64 s10, exec
        0x7E06_020A,         // v_mov_b32 v3, s10
        0xBEFE_0414,         // s_mov_b64 exec, s[20:21]
    ];
    for threads in [64u32, 128] {
        let built = build([threads, 1, 1], &body);
        assert_eq!(built.lowered.wave_size, Some(32), "{threads} threads");
        let Some(got) = run(&built.shader, threads) else {
            return;
        };
        for t in 0..threads {
            let (wave, lane) = (t / 64, t % 64);
            let running = |l: u32| wave * 64 + l > 20;
            let count = (0..64).filter(|&l| running(l)).count() as u32;
            let want = if running(lane) { count } else { OLD + t };
            assert_eq!(got[t as usize], want, "thread {t} of {threads}");
        }
    }
}

/// `v_mbcnt_lo` and `v_mbcnt_hi` count the lanes of a mask below the thread,
/// of both halves (the upper half counts the lower half's too); with an
/// all-ones mask they give the console lane.
#[test]
fn wave64_mbcnt_counts_the_lanes_of_both_halves_below_the_thread() {
    // Odd lanes run; each counts the odd lanes of EXEC below it.
    let mut odd = vec![
        vop2(4, 0x1B, 129, 0), // v_and_b32 v4, 1, v0
        0x7D84_0881,           // v_cmp_eq_u32 vcc, 1, v4
        0xBE94_246A,           // s_and_saveexec_b64 s[20:21], vcc
    ];
    odd.extend(vop3(3, 0x365, 126, 128, 0)); // v_mbcnt_lo_u32_b32 v3, exec_lo, 0
    odd.extend(vop3(3, 0x366, 127, 259, 0)); // v_mbcnt_hi_u32_b32 v3, exec_hi, v3
    odd.push(0xBEFE_0414); // s_mov_b64 exec, s[20:21]
    // An all-ones mask: the lane itself.
    let mut lane = vop3(3, 0x365, 193, 128, 0).to_vec(); // v_mbcnt_lo_u32_b32 v3, -1, 0
    lane.extend(vop3(3, 0x366, 193, 259, 0)); // v_mbcnt_hi_u32_b32 v3, -1, v3
    for threads in [64u32, 128] {
        let built = build([threads, 1, 1], &odd);
        assert_eq!(built.lowered.wave_size, Some(32), "{threads} threads");
        let Some(got) = run(&built.shader, threads) else {
            return;
        };
        for t in 0..threads {
            // The odd lanes below an odd lane l of its console wave: l / 2.
            let want = if t % 2 == 1 { (t % 64) / 2 } else { OLD + t };
            assert_eq!(got[t as usize], want, "odd lanes, thread {t} of {threads}");
        }
        let built = build([threads, 1, 1], &lane);
        assert_eq!(built.lowered.wave_size, Some(32), "{threads} threads");
        let got = run(&built.shader, threads).expect("ran above");
        for t in 0..threads {
            assert_eq!(got[t as usize], t % 64, "lane, thread {t} of {threads}");
        }
    }
}
