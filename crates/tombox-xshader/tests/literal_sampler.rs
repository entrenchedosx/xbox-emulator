//! Samplers a console shader builds in SGPRs, and descriptors it fetches straight from the descriptor heaps.
use tombox_xshader::hlsl::{litsamp, translate, TB_LITSAMP_SPACE};
use tombox_xshader::meta::{RootParam, RootParamKind, RootRange, RootSig, SigElement};
use tombox_xshader::{ShaderExtract, Stage};

fn shader(words: &[u32], root: RootSig) -> ShaderExtract {
    let isa: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    ShaderExtract {
        stage: Stage::Pixel,
        shader_model: (6, 0),
        bitcode_stripped: true,
        fssm: Vec::new(),
        entry: Some("main".into()),
        isa_data: isa.clone(),
        isa,
        isa_offset_in_fssm: 0,
        isa_preamble: [0; 4],
        psv_numthreads: None,
        has_input_sig: true,
        has_output_sig: true,
        has_psv0: false,
        has_root_sig: true,
        input_sig: vec![],
        output_sig: vec![SigElement { name: "SV_Target".into(), index: 0, sysval: 64, comp_type: 3, register: 0, mask: 0xf, rw_mask: 0 }],
        prim_sig: Vec::new(),
        resources: Vec::new(),
        gs_info: None,
        root_sig: Some(root),
    }
}

fn compile(label: &str, hlsl: &str, profile: &str) {
    #[cfg(windows)]
    {
        assert!(tombox_xshader::dxc::available(), "{label}: DXC (dxcompiler.dll + dxil.dll) not found");
        let profile = tombox_xshader::dxc::profile_for(profile);
        let code = tombox_xshader::dxc::compile(hlsl, &profile).unwrap_or_else(|e| panic!("{label}: dxc: {e}\n{hlsl}"));
        assert!(code.len() > 4 && &code[..4] == b"DXBC", "{label}: DXC returned no container");
    }
    #[cfg(not(windows))]
    let _ = (label, hlsl, profile);
}

fn table(range_type: u32) -> RootParam {
    RootParam { visibility: 0, kind: RootParamKind::Table(vec![RootRange { range_type, num: 4, base_register: 0, space: 0, offset_in_table: u32::MAX }]) }
}

/// Three tables, one root descriptor and two root constants: seven user SGPRs, s8..s14.
fn packed_root(flags: u32) -> RootSig {
    RootSig {
        version: 2,
        flags,
        params: vec![
            table(0),
            table(0),
            table(2),
            RootParam { visibility: 0, kind: RootParamKind::Descriptor { kind: 3, register: 4, space: 3 } },
            RootParam { visibility: 0, kind: RootParamKind::Constants { register: 500, space: 0, num32: 2 } },
        ],
        num_static_samplers: 0,
        static_samplers: Vec::new(),
    }
}

const TAIL: [u32; 4] = [0x7e00_0304, 0xf800_180f, 0x0302_0100, 0xbf81_0000];

/// `s_load_dwordx8 s20, s8` (the texture descriptor of table 0), the S# {0x92, 0xc00000, 0x14500000, 0} built in
/// s28..s31 from constants, then `image_sample_lz v0, v3, s20, s28`.
fn literal_sampler_program() -> Vec<u32> {
    let mut w = vec![0xf40c_0504, 0xfa00_0000, 0xb01c_0092, 0xbe9d_03ff, 0x00c0_0000, 0xbe9e_03ff, 0x1450_0000, 0xbe9f_0380, 0xf09c_0708, 0x00e5_0003];
    w.extend_from_slice(&TAIL);
    w
}

#[test]
fn a_sampler_built_from_constants_binds_to_the_static_sampler_bank() {
    let t = translate(&shader(&literal_sampler_program(), packed_root(0))).unwrap_or_else(|e| panic!("translate failed: {e}"));
    let want = litsamp::pick(&litsamp::decode(0x92, 0x00c0_0000, 0x1450_0000, false)).expect("in the bank");
    assert!(
        t.hlsl.contains(&format!("SamplerState samp{} : register(s{}, space{TB_LITSAMP_SPACE});", (u64::from(TB_LITSAMP_SPACE) << 16) | u64::from(want.register), want.register)),
        "{}",
        t.hlsl
    );
    compile("literal sampler", &t.hlsl, t.profile);
}

#[test]
fn heap_directly_indexed_textures_and_samplers_come_from_the_descriptor_heaps() {
    // s_mov_b32 s24, s15 / s_mov_b32 s25, 0: the resource heap pointer (the user SGPR after the packed root arguments);
    // s_mov_b32 s26, s16 / s_mov_b32 s27, 0: the sampler heap pointer; s0 = 64 / s1 = 48 are byte offsets (slot 2 of 32
    // bytes, slot 3 of 16); s_load_dwordx8 s32, s24 [s0]; s_load_dwordx4 s40, s26 [s1]; image_sample_lz v0, v3, s32, s40.
    let mut w = vec![
        0xbe98_030f, 0xbe99_0380, 0xbe9a_0310, 0xbe9b_0380, 0xbe80_03c0, 0xbe81_03b0,
        0xf40c_080c, 0x0000_0000, 0xf408_0a0d, 0x0200_0000,
        0xf09c_0708, 0x0148_0003,
    ];
    w.extend_from_slice(&TAIL);
    let t = translate(&shader(&w, packed_root(0xc00))).unwrap_or_else(|e| panic!("translate failed: {e}"));
    assert!(t.hlsl.contains("ResourceDescriptorHeap[NonUniformResourceIndex(tb_bi0 / 32u)]"), "{}", t.hlsl);
    assert!(t.hlsl.contains("SamplerDescriptorHeap[NonUniformResourceIndex(tb_bi1 / 16u)]"), "{}", t.hlsl);
    assert!(!t.hlsl.contains("not a root parameter"));
    compile("heap indexed", &t.hlsl, t.profile);
}
