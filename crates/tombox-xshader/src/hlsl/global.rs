use super::{r, Emitter};
use crate::global_abi::{TB_GLOBAL_FAULT_UAV, TB_GLOBAL_SPACE, TB_GLOBAL_WINDOWS};
use crate::rdna2::{Enc, Insn, Opnd};

pub const HELPERS: &str = r#"cbuffer TbGlobal : register(bCBV, spaceSPACE) { uint4 tb_gwin[NWIN]; uint4 tb_msin; };
RWByteAddressBuffer tb_gw0 : register(u0, spaceSPACE);
RWByteAddressBuffer tb_gw1 : register(u1, spaceSPACE);
RWByteAddressBuffer tb_gfault : register(uFAULT, spaceSPACE);
uint2 tb_gva(uint lo, uint hi, uint add, int off) {
    uint a = lo + add;
    uint c = a < lo ? 1u : 0u;
    uint b = a + (uint)off;
    uint d = off < 0 ? (b <= a ? 0u : 0xffffffffu) : (b < a ? 1u : 0u);
    return uint2(b, hi + c + d);
}
uint tb_gwhich(uint2 va, out uint o) {
    [unroll] for (uint i = 0u; i < NWINu; i++) {
        uint2 b = tb_gwin[i].xy;
        uint lo = va.x - b.x;
        uint hi = va.y - b.y - (va.x < b.x ? 1u : 0u);
        if (tb_gwin[i].z >= 4u && hi == 0u && lo <= tb_gwin[i].z - 4u) { o = lo; return i; }
    }
    o = 0u;
    return 0xffffffffu;
}
void tb_gmiss(uint2 va) {
    uint p;
    tb_gfault.InterlockedAdd(0, 1u, p);
    if (p == 0u) tb_gfault.Store2(4, va);
}
uint tb_gload_al(uint w, uint o) {
    if (w == 0u) return tb_gw0.Load(o);
    return tb_gw1.Load(o);
}
uint tb_gmem_load(uint2 va, uint nbytes) {
    uint2 a = tb_gva(va.x & ~3u, va.y, 0u, 0);
    uint o;
    uint w = tb_gwhich(a, o);
    if (w == 0xffffffffu) { tb_gmiss(va); return 0u; }
    uint sh = (va.x & 3u) * 8u;
    uint lo = tb_gload_al(w, o);
    if ((va.x & 3u) + nbytes <= 4u) return lo >> sh;
    uint2 n = tb_gva(a.x, a.y, 4u, 0);
    uint o2;
    uint w2 = tb_gwhich(n, o2);
    if (w2 == 0xffffffffu) { tb_gmiss(n); return lo >> sh; }
    return (lo >> sh) | (tb_gload_al(w2, o2) << (32u - sh));
}
void tb_gstore_bits(uint2 va, uint v, uint nbytes) {
    uint2 a = tb_gva(va.x & ~3u, va.y, 0u, 0);
    uint o;
    uint w = tb_gwhich(a, o);
    if (w == 0xffffffffu) { tb_gmiss(va); return; }
    uint sh = (va.x & 3u) * 8u;
    uint m = (nbytes >= 4u ? 0xffffffffu : ((1u << (nbytes * 8u)) - 1u)) << sh;
    uint p;
    if (m == 0xffffffffu) {
        if (w == 0u) tb_gw0.Store(o, v); else tb_gw1.Store(o, v);
        return;
    }
    uint keep = ~m;
    uint put = (v << sh) & m;
    if (w == 0u) { tb_gw0.InterlockedAnd(o, keep, p); tb_gw0.InterlockedOr(o, put, p); }
    else { tb_gw1.InterlockedAnd(o, keep, p); tb_gw1.InterlockedOr(o, put, p); }
}
void tb_gmem_store_n(uint2 va, uint v, uint nbytes) {
    uint sh = (va.x & 3u) * 8u;
    uint first = min(nbytes, 4u - (va.x & 3u));
    tb_gstore_bits(va, v, first);
    if (first < nbytes) tb_gstore_bits(tb_gva(va.x, va.y, first, 0), v >> (first * 8u), nbytes - first);
}
void tb_gmem_store(uint2 va, uint v) { tb_gmem_store_n(va, v, 4u); }
#define TB_GMEM 1
#ifndef TB_AMP_GMEM
#define tb_gload tb_gmem_load
#define tb_gstore tb_gmem_store
#define tb_gstore_n tb_gmem_store_n
#endif
"#;

/// Atomic read-modify-write on a dword of a window, for the `flat_atomic_*` family. Added to
/// [`HELPERS`] only when the shader has such an instruction, so every other shader's text is
/// unchanged.
///
/// `op` is the instruction's operation (see [`atomic_op`]); `v` is DATA, `c` the compare value
/// of a compare-and-swap. The old value is returned. Operations with a native `Interlocked*`
/// form use it; the others are a compare-and-swap loop on the dword. An address that is not
/// dword aligned, or that lies outside every window, is a fault: it is counted in the fault
/// record (which the binder logs) and no memory is touched.
pub const ATOMIC_HELPERS: &str = r#"uint tb_gatom_on(RWByteAddressBuffer b, uint o, uint op, uint v, uint c) {
    uint old = 0u;
    if (op == 0u) { b.InterlockedExchange(o, v, old); return old; }
    if (op == 1u) { b.InterlockedCompareExchange(o, c, v, old); return old; }
    if (op == 2u) { b.InterlockedAdd(o, v, old); return old; }
    if (op == 3u) { b.InterlockedAdd(o, 0u - v, old); return old; }
    if (op == 6u) { b.InterlockedMin(o, v, old); return old; }
    if (op == 8u) { b.InterlockedMax(o, v, old); return old; }
    if (op == 9u) { b.InterlockedAnd(o, v, old); return old; }
    if (op == 10u) { b.InterlockedOr(o, v, old); return old; }
    if (op == 11u) { b.InterlockedXor(o, v, old); return old; }
    uint cur = b.Load(o);
    [allow_uav_condition] for (uint i = 0u; i < 65536u; i++) {
        uint nv = cur;
        if (op == 5u) nv = asuint(min(asint(cur), asint(v)));
        else if (op == 7u) nv = asuint(max(asint(cur), asint(v)));
        else if (op == 12u) nv = (cur >= v) ? 0u : (cur + 1u);
        else if (op == 13u) nv = ((cur == 0u) || (cur > v)) ? v : (cur - 1u);
        else if (op == 14u) nv = (asfloat(cur) == asfloat(c)) ? v : cur;
        else if (op == 15u) nv = (asfloat(v) < asfloat(cur)) ? v : cur;
        else if (op == 16u) nv = (asfloat(v) > asfloat(cur)) ? v : cur;
        if (nv == cur) return cur;
        uint prev = 0u;
        b.InterlockedCompareExchange(o, cur, nv, prev);
        if (prev == cur) return cur;
        cur = prev;
    }
    return cur;
}
uint tb_gatom(uint2 va, uint op, uint v, uint c) {
    if ((va.x & 3u) != 0u) { tb_gmiss(va); return 0u; }
    uint o;
    uint w = tb_gwhich(va, o);
    if (w == 0xffffffffu) { tb_gmiss(va); return 0u; }
    if (w == 0u) return tb_gatom_on(tb_gw0, o, op, v, c);
    return tb_gatom_on(tb_gw1, o, op, v, c);
}
"#;

pub fn helpers() -> String {
    HELPERS
        .replace("bCBV", &format!("b{}", crate::global_abi::TB_GLOBAL_CBV))
        .replace("uFAULT", &format!("u{TB_GLOBAL_FAULT_UAV}"))
        .replace("NWINu", &format!("{TB_GLOBAL_WINDOWS}u"))
        .replace("NWIN", &TB_GLOBAL_WINDOWS.to_string())
        .replace("SPACE", &TB_GLOBAL_SPACE.to_string())
}

/// Helper text for the program: the window helpers, plus the atomic ones when used.
pub fn helpers_for(insns: &[Insn]) -> String {
    let mut h = helpers();
    if insns.iter().any(|i| i.enc == Enc::Flat && i.mnemonic.starts_with("flat_atomic_")) {
        h.push_str(ATOMIC_HELPERS);
    }
    h
}

/// The operation number `tb_gatom_on` takes for a 32-bit `flat_atomic_*` mnemonic, and whether
/// the instruction has a second DATA dword (the compare value).
fn atomic_op(m: &str) -> Result<(u32, bool), String> {
    let name = m.strip_prefix("flat_atomic_").ok_or_else(|| format!("{m}: not an atomic"))?;
    if name.ends_with("_x2") {
        return Err(format!("{m}: 64-bit global atomics are not lowered (they need Int64 atomics on the window buffers)"));
    }
    Ok(match name {
        "swap" => (0, false),
        "cmpswap" => (1, true),
        "add" => (2, false),
        "sub" => (3, false),
        "smin" => (5, false),
        "umin" => (6, false),
        "smax" => (7, false),
        "umax" => (8, false),
        "and" => (9, false),
        "or" => (10, false),
        "xor" => (11, false),
        "inc" => (12, false),
        "dec" => (13, false),
        "fcmpswap" => (14, true),
        "fmin" => (15, false),
        "fmax" => (16, false),
        other => return Err(format!("{m}: global atomic `{other}` is not lowered (its memory semantics are not modelled)")),
    })
}

fn width(m: &str) -> Option<(bool, u8, u8, bool)> {
    Some(match m {
        "flat_load_ubyte" => (false, 1, 1, false),
        "flat_load_sbyte" => (false, 1, 1, true),
        "flat_load_ushort" => (false, 1, 2, false),
        "flat_load_sshort" => (false, 1, 2, true),
        "flat_load_dword" => (false, 1, 4, false),
        "flat_load_dwordx2" => (false, 2, 4, false),
        "flat_load_dwordx3" => (false, 3, 4, false),
        "flat_load_dwordx4" => (false, 4, 4, false),
        "flat_store_byte" | "flat_store_byte_d16_hi" => (true, 1, 1, false),
        "flat_store_short" | "flat_store_short_d16_hi" => (true, 1, 2, false),
        "flat_store_dword" => (true, 1, 4, false),
        "flat_store_dwordx2" => (true, 2, 4, false),
        "flat_store_dwordx3" => (true, 3, 4, false),
        "flat_store_dwordx4" => (true, 4, 4, false),
        _ => return None,
    })
}

pub fn routes(ins: &Insn, insns: &[Insn]) -> bool {
    if ins.enc != Enc::Flat {
        return false;
    }
    let m = ins.mnemonic.as_str();
    if m.starts_with("flat_load_dword") && ins.aux.mask != 1 && insns.iter().any(|i| i.mnemonic == "s_getpc_b64") {
        return false;
    }
    // Scratch-segment atomics are routed too, so that they fail with the scratch message.
    ins.aux.mask != 1 || width(m).is_some_and(|w| w.0) || m.starts_with("flat_atomic_")
}

impl<'a> Emitter<'a> {
    fn global_va(&mut self, ins: &Insn) -> Result<String, String> {
        let off = ins.aux.imm as i32;
        let v = match ins.src.first() {
            Some(Opnd::Vgpr(v)) => *v,
            _ => return Err("global access without a VGPR address".into()),
        };
        match ins.src.get(2) {
            Some(Opnd::Sgpr(s)) => {
                let (s0, s1) = (Opnd::Sgpr(*s), Opnd::Sgpr(s + 1));
                self.touch(&s1);
                Ok(format!("tb_gva({}, {}, {}, {off})", r(&s0)?, r(&s1)?, r(&Opnd::Vgpr(v))?))
            }
            Some(Opnd::Null) | None => {
                let hi = Opnd::Vgpr(v + 1);
                self.touch(&hi);
                Ok(format!("tb_gva({}, {}, 0u, {off})", r(&Opnd::Vgpr(v))?, r(&hi)?))
            }
            Some(o) => Err(format!("global access with scalar address {o:?} unsupported")),
        }
    }

    pub(super) fn global_load_expr(&self, va: &str, k: u8, bytes: u8, signed: bool) -> String {
        let a = if k == 0 { va.to_string() } else { format!("tb_gva({va}.x, {va}.y, {}u, 0)", 4 * k as u32) };
        match (bytes, signed) {
            (4, _) => format!("tb_gload({a}, {bytes}u)"),
            (1, false) => format!("(tb_gload({a}, {bytes}u) & 0xffu)"),
            (2, false) => format!("(tb_gload({a}, {bytes}u) & 0xffffu)"),
            (1, true) => format!("(uint)(((int)(tb_gload({a}, {bytes}u) << 24u)) >> 24u)"),
            _ => format!("(uint)(((int)(tb_gload({a}, {bytes}u) << 16u)) >> 16u)"),
        }
    }

    pub(super) fn global_store_line(&self, va: &str, k: u8, val: &str, bytes: u8) -> String {
        let a = if k == 0 { va.to_string() } else { format!("tb_gva({va}.x, {va}.y, {}u, 0)", 4 * k as u32) };
        if bytes == 4 { format!("tb_gstore({a}, {val});") } else { format!("tb_gstore_n({a}, {val}, {bytes}u);") }
    }

    /// `s_load_dword{,x2,x4,x8,x16} sD, sBase[, soffset] [imm]` where `sBase:sBase+1` is a
    /// 64-bit address held in SGPRs: a read of global memory by scalar unit, lowered to
    /// the same window-table lookups as vector flat loads. Every lane takes the same
    /// address, so there is no exec guard (scalar memory ignores exec). The hardware
    /// ignores the low two address bits of a scalar load, so the address is aligned down.
    /// An address outside every window reads zero and bumps the fault record, exactly like
    /// the vector path; the binder logs those faults.
    pub(super) fn emit_smem_global(&mut self, ins: &Insn) -> Result<(), String> {
        let n = 1u32 << ins.op;
        let b = match ins.src.first() {
            Some(Opnd::Sgpr(b)) => *b,
            other => return Err(format!("s_load at {:#x}: global base {other:?} is not an SGPR pair", ins.offset)),
        };
        let d = match ins.dst.first() {
            Some(Opnd::Sgpr(d)) => *d,
            other => return Err(format!("s_load at {:#x}: bad destination {other:?}", ins.offset)),
        };
        let (lo, hi) = (Opnd::Sgpr(b), Opnd::Sgpr(b + 1));
        self.touch(&lo);
        self.touch(&hi);
        let soff = ins.raw.get(1).copied().unwrap_or(0) >> 25;
        let add = if soff == 0x7d {
            "0u".to_string()
        } else if soff <= 107 {
            self.touch(&Opnd::Sgpr(soff as u8));
            r(&Opnd::Sgpr(soff as u8))?
        } else {
            return Err(format!("s_load at {:#x}: soffset {soff} unsupported", ins.offset));
        };
        let off = ins.aux.imm as i32;
        let t = self.tmp;
        self.tmp += 1;
        self.line(&format!("uint2 g{t} = tb_gva({}, {}, {add}, {off}); g{t}.x &= 0xfffffffcu;", r(&lo)?, r(&hi)?));
        let g = format!("g{t}");
        for k in 0..n as u8 {
            self.line(&format!("uint g{t}_{k} = {};", self.global_load_expr(&g, k, 4, false)));
        }
        for k in 0..n as u8 {
            let dst = Opnd::Sgpr(d + k);
            self.touch(&dst);
            self.ws(&dst, &format!("g{t}_{k}"))?;
        }
        Ok(())
    }

    /// `global_atomic_*` / `flat_atomic_*` (32-bit): an atomic read-modify-write of one dword at
    /// the instruction's 64-bit address. DATA is `v[data]` (and `v[data+1]` for the compare
    /// operand of a compare-and-swap); with GLC set the pre-operation value is returned in VDST.
    fn emit_global_atomic(&mut self, ins: &Insn) -> Result<(), String> {
        let m = ins.mnemonic.as_str();
        if self.amp_flat(ins) {
            return Err(format!("{m}: global atomics in amplification/mesh shaders are not lowered"));
        }
        let (op, has_cmp) = atomic_op(m)?;
        let data = match ins.src.get(1) {
            Some(Opnd::Vgpr(d)) => *d,
            _ => return Err(format!("{m}: data operand is not a VGPR")),
        };
        let va = self.global_va(ins)?;
        let (val, cmp) = (Opnd::Vgpr(data), Opnd::Vgpr(data.wrapping_add(1)));
        self.touch(&val);
        let v = r(&val)?;
        let c = if has_cmp {
            self.touch(&cmp);
            r(&cmp)?
        } else {
            "0u".to_string()
        };
        let t = self.tmp;
        self.tmp += 1;
        self.line(&format!("uint2 g{t} = {va}; uint g{t}_old = 0u;"));
        self.line(&format!("if (exec_lo & 1u) {{ g{t}_old = tb_gatom(g{t}, {op}u, {v}, {c}); }}"));
        let returns = ins.raw.first().is_some_and(|w| (w >> 16) & 1 != 0);
        if returns {
            let vd = match ins.dst.first() {
                Some(Opnd::Vgpr(v)) => *v,
                _ => return Err(format!("{m}: bad vdst")),
            };
            self.wv(&Opnd::Vgpr(vd), &format!("g{t}_old"))?;
        }
        Ok(())
    }

    pub(super) fn emit_global(&mut self, ins: &Insn) -> Result<(), String> {
        let m = ins.mnemonic.as_str();
        if ins.aux.mask == 1 {
            return Err(format!("{m}: scratch-segment access unsupported"));
        }
        if ins.raw.first().is_some_and(|w| (w >> 13) & 1 != 0) {
            return Err(format!("{m}: LDS=1 (memory <-> LDS transfer) is not lowered"));
        }
        if m.starts_with("flat_atomic_") {
            return self.emit_global_atomic(ins);
        }
        let (store, n, bytes, signed) = width(m).ok_or_else(|| format!("{m}: global/flat operation unsupported"))?;
        let va = self.global_va(ins)?;
        let t = self.tmp;
        self.tmp += 1;
        self.line(&format!("uint2 g{t} = {va};"));
        let g = format!("g{t}");
        if store {
            let data = match ins.src.get(1) {
                Some(Opnd::Vgpr(d)) => *d,
                _ => return Err(format!("{m}: data operand is not a VGPR")),
            };
            let mut body = Vec::new();
            for k in 0..n {
                let o = Opnd::Vgpr(data + k);
                self.touch(&o);
                let v = if m.ends_with("_d16_hi") { format!("({} >> 16u)", r(&o)?) } else { r(&o)? };
                body.push(self.global_store_line(&g, k, &v, bytes));
            }
            self.line(&format!("if (exec_lo & 1u) {{ {} }}", body.join(" ")));
            return Ok(());
        }
        let vd = match ins.dst.first() {
            Some(Opnd::Vgpr(v)) => *v,
            _ => return Err(format!("{m}: bad vdst")),
        };
        let decl: Vec<String> = (0..n).map(|k| format!("g{t}_{k} = 0u")).collect();
        self.line(&format!("uint {};", decl.join(", ")));
        let loads: Vec<String> = (0..n).map(|k| format!("g{t}_{k} = {};", self.global_load_expr(&g, k, bytes, signed))).collect();
        self.line(&format!("if (exec_lo & 1u) {{ {} }}", loads.join(" ")));
        for k in 0..n {
            self.wv(&Opnd::Vgpr(vd + k), &format!("g{t}_{k}"))?;
        }
        Ok(())
    }
}
