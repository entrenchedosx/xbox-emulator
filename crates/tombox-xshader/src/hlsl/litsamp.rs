//! Samplers a console shader builds itself.
//!
//! On the console a shader can write a sampler (the 4-dword S#) into four SGPRs with `s_mov_b32` / `s_movk_i32` and
//! hand them to an `image_sample*` instruction; the title's root signature then declares no sampler for it at all.
//! The PC has no such thing: a sampler is either a heap descriptor or a static sampler in the root signature. The
//! root signature of a title is created before any of its shaders exist, so the translator cannot ask for "the
//! sampler this shader needs". Instead every title root signature gets a fixed *bank* of static samplers, in the
//! register space `TB_LITSAMP_SPACE`, register = index in `bank()`, and the translator (which sees the constants)
//! binds the shader's sampler to the bank entry that decodes the same way.
//!
//! The bank covers the combinations titles build in practice (address modes, point / linear / anisotropic
//! filtering, the usual LOD range, the common depth-compare functions). Two S# fields are not modelled:
//! the W address mode is taken from V (it only matters for volume textures) and the border colour is transparent
//! black (S# word 3 selects another one). A sampler outside the bank is not guessed: `pick` returns `None` and the
//! shader is rejected with the usual "not a sampler descriptor" reason.
//!
//! To widen the bank, set `TOMBOX_LITSAMP_LOG=<file>`: every literal S# that no bank entry matches is appended there.

use super::durango::SSharp;
use crate::meta::StaticSampler;
use std::sync::OnceLock;

/// Register space of the bank in the root signature.
pub const TB_LITSAMP_SPACE: u32 = 70_000;

/// First descriptor of the bank in the title's shader-visible sampler heap. The emulator
/// extends every such heap to `D3D12_MAX_SHADER_VISIBLE_SAMPLER_HEAP_SIZE` (2048) and writes
/// the bank at this fixed index, so a run-time S# index can read a sampler descriptor.
pub const TB_LITSAMP_HEAP_BASE: u32 = 1932;

const WRAP: u32 = 1;
const MIRROR: u32 = 2;
const CLAMP: u32 = 3;
const BORDER: u32 = 4;
const MIRROR_ONCE: u32 = 5;
const MODES: [u32; 5] = [WRAP, MIRROR, CLAMP, BORDER, MIRROR_ONCE];

/// The LOD range (4.8 fixed point, as decoded from the S#) every bank entry has: 0 .. 12.
const LOD: (u32, u32) = (0, 12 * 256);

/// D3D12 filter codes without the reduction bits: bit 4 minification, bit 2 magnification, bit 0 mip, all linear.
const FILTERS: [u32; 8] = [0x00, 0x01, 0x04, 0x05, 0x10, 0x11, 0x14, 0x15];
const ANISOTROPIC: u32 = 0x55;
const ANISO_RATIOS: [u32; 4] = [2, 4, 8, 16];

const REDUCTION_COMPARE: u32 = 0x80;
/// D3D12_COMPARISON_FUNC: LESS, GREATER, LESS_EQUAL, GREATER_EQUAL.
const COMPARE_FUNCS: [u32; 4] = [2, 5, 4, 7];
const COMPARE_MODES: [u32; 3] = [WRAP, CLAMP, BORDER];
const COMPARE_FILTERS: [u32; 3] = [0x00, 0x14, 0x15];

fn entry(filter: u32, u: u32, v: u32, aniso: u32, cmp: u32) -> StaticSampler {
    StaticSampler {
        filter,
        address_u: u,
        address_v: v,
        address_w: v,
        max_anisotropy: aniso,
        comparison_func: cmp,
        border_color: 0,
        min_lod_bits: (LOD.0 as f32 / 256.0).to_bits(),
        max_lod_bits: (LOD.1 as f32 / 256.0).to_bits(),
        register: 0,
        space: TB_LITSAMP_SPACE,
        visibility: 0,
    }
}

fn build() -> Vec<StaticSampler> {
    let mut v = Vec::new();
    for &m in &MODES {
        for &f in &FILTERS {
            v.push(entry(f, m, m, 1, 4));
        }
        for &r in &ANISO_RATIOS {
            v.push(entry(ANISOTROPIC, m, m, r, 4));
        }
    }
    // mixed address modes: the default trilinear filter only
    for &u in &MODES {
        for &m in &MODES {
            if u != m {
                v.push(entry(0x15, u, m, 1, 4));
            }
        }
    }
    for &m in &COMPARE_MODES {
        for &f in &COMPARE_FILTERS {
            for &c in &COMPARE_FUNCS {
                v.push(entry(REDUCTION_COMPARE | f, m, m, 1, c));
            }
        }
    }
    for (i, s) in v.iter_mut().enumerate() {
        s.register = i as u32;
    }
    v
}

/// The samplers every title root signature carries; entry `i` is register `i` of `TB_LITSAMP_SPACE`.
pub fn bank() -> &'static [StaticSampler] {
    static BANK: OnceLock<Vec<StaticSampler>> = OnceLock::new();
    BANK.get_or_init(build)
}

fn lod_of(s: &StaticSampler) -> (u32, u32) {
    ((f32::from_bits(s.min_lod_bits) * 256.0) as u32, (f32::from_bits(s.max_lod_bits) * 256.0) as u32)
}

/// The S# of four dwords (the fourth, the border colour, is not modelled) as the translator reads it.
pub fn decode(w0: u32, w1: u32, w2: u32, compare: bool) -> SSharp {
    super::durango::decode_ssharp(w0, Some(w1), Some(w2), compare)
}

/// The bank entry that behaves like the decoded S#, if there is one.
pub fn pick(d: &SSharp) -> Option<StaticSampler> {
    let filter = d.filter?;
    let lod = d.lod?;
    if d.reduction > 1 {
        return None;
    }
    bank().iter().copied().find(|s| {
        s.address_u == d.addr[0]
            && s.address_v == d.addr[1]
            && (s.filter >> 7) & 3 == d.reduction
            && (d.reduction != 1 || s.comparison_func == d.cmp_func)
            && s.filter & 0x7f == filter
            && d.max_aniso.is_none_or(|a| a == s.max_anisotropy)
            && lod_of(s) == lod
    })
}

/// The bank as the `D3D12_STATIC_SAMPLER_DESC` array of a serialized root signature of `version` (1, 2 = 1.1, 3 = 1.2).
pub fn rts0_bytes(version: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for s in bank() {
        let words = [
            s.filter,
            s.address_u,
            s.address_v,
            s.address_w,
            0f32.to_bits(), // MipLODBias
            s.max_anisotropy,
            s.comparison_func,
            s.border_color,
            s.min_lod_bits,
            s.max_lod_bits,
            s.register,
            s.space,
            s.visibility,
        ];
        for w in words {
            out.extend_from_slice(&w.to_le_bytes());
        }
        if version >= 3 {
            out.extend_from_slice(&0u32.to_le_bytes()); // Flags
        }
    }
    out
}

/// Register of `Binding` that stands for "the S# is read from SGPRs at run time" (see `hlsl_decls`).
pub const DYNAMIC: u32 = u32::MAX;

pub fn compare_start() -> usize {
    bank().iter().position(|s| (s.filter >> 7) & 3 == 1).unwrap_or(bank().len())
}

/// HLSL that picks a bank sampler from S# words held in SGPRs at run time, for samplers a shader builds on different
/// control-flow paths (so no single constant exists at the sample). `tb_lsb` holds the plain bank entries, `tb_lsc`
/// the depth-compare ones; both are views of the root signature's static samplers in `TB_LITSAMP_SPACE`.
/// `tb_lit_sampler_index` decodes the S# the way `decode_ssharp` does and finds the entry with the same address modes,
/// filter, anisotropy (and compare function); a combination outside the bank uses the same address modes with the
/// bilinear / trilinear filter, and the first entry if even those do not exist.
pub fn hlsl_decls(plain: bool, compare: bool) -> String {
    let b = bank();
    let cs = compare_start();
    let mut out = String::new();
    // D3D12 validates a shader sampler *array* against a sampler descriptor range; the title
    // roots carry the bank as static samplers, which can only satisfy individual registers.
    // Declare each entry as its own static sampler; run-time indexes use the heap instead.
    if plain {
        for i in 0..cs {
            out.push_str(&format!("SamplerState tb_lsb_{i} : register(s{i}, space{TB_LITSAMP_SPACE});
"));
        }
    }
    if compare {
        for i in cs..b.len() {
            out.push_str(&format!("SamplerComparisonState tb_lsc_{} : register(s{i}, space{TB_LITSAMP_SPACE});
", i - cs));
        }
    }
    out.push_str(&format!("static const uint4 tb_lskey[{}] = {{
", b.len()));
    for s in b {
        out.push_str(&format!("    uint4({}u, {}u, {}u, {}u),
", s.address_u, s.address_v, (s.filter & 0x7f) | (s.max_anisotropy << 8), s.comparison_func));
    }
    out.push_str("};
");
    out.push_str(&format!(
        "uint tb_lit_addr(uint c) {{ return c == 0u ? 1u : (c == 1u ? 2u : (c == 2u ? 3u : ((c == 3u || c == 5u || c == 7u) ? 5u : 4u))); }}
uint tb_lit_sampler_index(uint w0, uint w1, uint w2, bool compare) {{
    uint mag = (w2 >> 20) & 3u, mn = (w2 >> 22) & 3u, mip = (w2 >> 26) & 3u, ratio = (w0 >> 9) & 7u;
    uint f, an;
    if ((mag >= 2u || mn >= 2u) && ratio > 0u) {{ f = 0x55u; an = 1u << min(ratio, 4u); }}
    else {{ f = ((mn & 1u) ? 0x10u : 0u) | ((mag & 1u) ? 0x4u : 0u) | (mip == 2u ? 1u : 0u); an = 1u; }}
    uint u = tb_lit_addr(w0 & 7u), v = tb_lit_addr((w0 >> 3) & 7u), cf = ((w0 >> 12) & 7u) + 1u;
    uint lo = compare ? {cs}u : 0u, hi = compare ? {}u : {cs}u;
    uint near_i = lo;
    bool near_f = false;
    [loop] for (uint i = lo; i < hi; ++i) {{
        uint4 k = tb_lskey[i];
        bool same_mode = k.x == u && k.y == v && (!compare || k.w == cf);
        if (same_mode && (k.z & 0xffu) == f && (k.z >> 8) == an) {{ return i - lo; }}
        if (same_mode && (k.z & 0xffu) == (compare ? 0x14u : 0x15u) && (k.z >> 8) == 1u && !near_f) {{ near_i = i; near_f = true; }}
    }}
    return near_i - lo;
}}
",
        b.len()
    ));
    out
}

/// `D3D12_SAMPLER_DESC` (52 bytes) for a bank entry, so the same state can be written as a
/// descriptor in the shader-visible sampler heap.
pub fn sampler_desc(s: &StaticSampler) -> [u32; 13] {
    let border = match s.border_color {
        1 => [0.0f32, 0.0, 0.0, 1.0],
        2 => [1.0f32, 1.0, 1.0, 1.0],
        _ => [0.0f32; 4],
    };
    [
        s.filter,
        s.address_u,
        s.address_v,
        s.address_w,
        0.0f32.to_bits(),
        s.max_anisotropy,
        s.comparison_func,
        border[0].to_bits(),
        border[1].to_bits(),
        border[2].to_bits(),
        border[3].to_bits(),
        s.min_lod_bits,
        s.max_lod_bits,
    ]
}

/// Size of one serialized static sampler in a root signature of `version`.
pub fn rts0_stride(version: u32) -> usize {
    if version >= 3 { 56 } else { 52 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hlsl::durango::decode_ssharp;

    /// S# words a shipped title builds with `s_movk_i32` / `s_mov_b32` literals.
    fn decode(w0: u32, w1: u32, w2: u32, compare: bool) -> SSharp {
        decode_ssharp(w0, Some(w1), Some(w2), compare)
    }

    #[test]
    fn the_heap_range_fits_the_shader_visible_sampler_heap() {
        assert!(TB_LITSAMP_HEAP_BASE + bank().len() as u32 <= 2048);
    }

    #[test]
    fn a_bank_sampler_serializes_as_a_52_byte_sampler_desc() {
        let s = &bank()[30];
        let d = sampler_desc(s);
        assert_eq!(d.len() * 4, 52);
        assert_eq!((d[0], d[1], d[2], d[3]), (s.filter, s.address_u, s.address_v, s.address_w));
        assert_eq!(d[5], s.max_anisotropy);
        assert_eq!(d[6], s.comparison_func);
        assert_eq!(d[11], s.min_lod_bits);
        assert_eq!(d[12], s.max_lod_bits);
    }

    #[test]
    fn registers_are_the_bank_indexes_and_entries_are_distinct() {
        let b = bank();
        assert!(b.len() > 100 && b.len() < 400, "{}", b.len());
        for (i, s) in b.iter().enumerate() {
            assert_eq!(s.register as usize, i);
            assert_eq!(s.space, TB_LITSAMP_SPACE);
        }
        for i in 0..b.len() {
            for j in i + 1..b.len() {
                let (a, c) = (b[i], b[j]);
                assert!(StaticSampler { register: 0, ..a } != StaticSampler { register: 0, ..c }, "entries {i} and {j} are the same sampler");
            }
        }
    }

    #[test]
    fn a_clamped_bilinear_sampler_built_from_constants_gets_a_clamped_bilinear_static_sampler() {
        // the most common one in the retail title: clamp, linear min / mag, point mips, LOD 0..12
        let s = pick(&decode(0x92, 0x00c0_0000, 0x1450_0000, false)).expect("in the bank");
        assert_eq!((s.address_u, s.address_v), (CLAMP, CLAMP));
        assert_eq!(s.filter, 0x14, "min and mag linear, mip point");
        assert_eq!(lod_of(&s), LOD);
        assert_eq!(s.max_anisotropy, 1);
        let t = pick(&decode(0x92, 0x00c0_0000, 0x1850_0000, false)).expect("in the bank");
        assert_eq!(t.filter, 0x15, "linear mips: trilinear");
    }

    #[test]
    fn wrap_with_a_border_z_mode_ignores_the_unused_third_axis() {
        let s = pick(&decode(0x180, 0x00c0_0000, 0x1450_0000, false)).expect("in the bank");
        assert_eq!((s.address_u, s.address_v), (WRAP, WRAP));
    }

    #[test]
    fn anisotropic_sampler_keeps_its_ratio() {
        let s = pick(&decode(0x400, 0x00c0_0000, 0x18f0_0000, false)).expect("in the bank");
        assert_eq!(s.filter, ANISOTROPIC);
        assert_eq!(s.max_anisotropy, 4);
        assert_eq!((s.address_u, s.address_v), (WRAP, WRAP));
    }

    #[test]
    fn depth_compare_samplers_keep_their_function() {
        // clamp, depth-compare GREATER_EQUAL (GCN function 6), bilinear
        let d = decode(0x92 | (6 << 12), 0x00c0_0000, 0x0050_0000, true);
        let s = pick(&d).expect("in the bank");
        assert_eq!(s.filter >> 7 & 3, 1);
        assert_eq!(s.comparison_func, 7);
        assert_eq!(s.filter & 0x7f, 0x14);
    }

    #[test]
    fn what_the_bank_does_not_model_is_not_guessed() {
        assert!(pick(&decode(0x92, 0x00c0_0000, 0x1450_0000 | 0x3, false)).is_some(), "the LOD bias is not part of the key");
        assert!(pick(&decode(0x92, 0x0000_0500, 0x1450_0000, false)).is_none(), "a LOD range of 0..5/256 is not in the bank");
        assert!(pick(&decode(0x92 | (1 << 29), 0x00c0_0000, 0x1450_0000, false)).is_none(), "min / max reduction filters are not in the bank");
    }

    #[test]
    fn serialized_samplers_are_52_bytes_each_and_56_in_root_signature_1_2() {
        assert_eq!(rts0_bytes(2).len(), bank().len() * 52);
        assert_eq!(rts0_bytes(1).len(), bank().len() * 52);
        assert_eq!(rts0_bytes(3).len(), bank().len() * 56);
        let b = rts0_bytes(2);
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 0, "register 0");
        assert_eq!(u32::from_le_bytes(b[44..48].try_into().unwrap()), TB_LITSAMP_SPACE);
        assert_eq!(u32::from_le_bytes(b[52 + 40..52 + 44].try_into().unwrap()), 1, "register 1");
    }
}
