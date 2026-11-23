//! Which scalar registers hold a value the translated HLSL really has.
//!
//! The translator resolves descriptors and root-table reads *statically*: an SGPR that holds
//! a root-table pointer, or a word of a descriptor loaded from a table, exists only as a
//! symbol (`Sym::Table` / `Sym::Desc`); the HLSL variable of that name is never assigned.
//! That is fine while the register is only used to name a resource. It is wrong the moment
//! the register is used as a *value*: a 64-bit address for a scalar load, say. Reading
//! global memory through such an address would silently read from address zero.
//!
//! This is a forward dataflow over the control-flow graph. State = the set of registers whose
//! value is *not* available (SGPRs, VGPRs under `VGPR + n`, and SCC); join = union.
//!
//! * At entry the user-data SGPRs (`0..user_base`) hold root pointers: unavailable.
//! * A load of root *constants*, of global memory, of code-embedded tables or of constant
//!   buffer data yields real values. A load of a descriptor (root descriptor, descriptor
//!   table, bindless) yields words that exist only symbolically.
//! * Everything else is a function of its inputs: unavailable if any input is.
//!
//! `check_smem_global` uses it to accept a scalar load through an SGPR-held address only
//! when both halves of the address (and a register offset) are real values.

use super::Emitter;
use crate::meta::RootParamKind;
use crate::rdna2::{Enc, Insn, Opnd};
use std::collections::BTreeSet;
use std::ops::Range;

const SCC: u16 = 900;
const VGPR: u16 = 3000;

/// Basic blocks and their successors (same shape as the resource dataflow's).
pub(super) fn cfg_of(insns: &[Insn]) -> (Vec<Range<usize>>, Vec<Vec<usize>>) {
    let n = insns.len();
    let off2idx: std::collections::HashMap<usize, usize> = insns.iter().enumerate().map(|(i, x)| (x.offset, i)).collect();
    let mut leaders: BTreeSet<usize> = BTreeSet::new();
    leaders.insert(0);
    let mut tgt: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (i, ins) in insns.iter().enumerate() {
        if ins.enc != Enc::Sopp {
            continue;
        }
        if Emitter::branch_cond(ins.op).is_some() {
            let t = ins.offset as i64 + 4 + ins.aux.imm * 4;
            if let Some(&ti) = off2idx.get(&(t as usize)) {
                tgt.insert(i, ti);
                leaders.insert(ti);
            }
            leaders.insert(i + 1);
        } else if ins.op == 1 {
            leaders.insert(i + 1);
        }
    }
    let starts: Vec<usize> = leaders.into_iter().filter(|&s| s < n).collect();
    let block_of = |i: usize| starts.partition_point(|&s| s <= i) - 1;
    let ranges: Vec<Range<usize>> = (0..starts.len()).map(|b| starts[b]..starts.get(b + 1).copied().unwrap_or(n)).collect();
    let succs = ranges
        .iter()
        .map(|r| {
            let last = r.end - 1;
            let ins = &insns[last];
            let mut v = Vec::new();
            if let Some(&t) = tgt.get(&last) {
                v.push(block_of(t));
            }
            let falls = !(ins.enc == Enc::Sopp && (ins.op == 1 || ins.op == 2));
            if falls && r.end < n {
                v.push(block_of(r.end));
            }
            v
        })
        .collect();
    (ranges, succs)
}

fn key(o: &Opnd) -> Option<u16> {
    match o {
        Opnd::Sgpr(n) => Some(*n as u16),
        Opnd::Vgpr(v) => Some(VGPR + *v as u16),
        Opnd::Scc => Some(SCC),
        _ => None,
    }
}

/// How many consecutive SGPRs source operand `i` covers.
fn src_span(ins: &Insn, i: usize) -> u16 {
    match ins.enc {
        Enc::Smem => {
            if i == 0 {
                2
            } else {
                1
            }
        }
        Enc::Mubuf | Enc::Mtbuf => {
            if i == 1 {
                4
            } else {
                1
            }
        }
        Enc::Mimg => 8,
        _ => {
            if ins.mnemonic.contains("64") {
                2
            } else {
                1
            }
        }
    }
}

fn dst_span(ins: &Insn) -> u16 {
    if ins.enc == Enc::Smem && ins.op <= 4 {
        1 << ins.op
    } else if ins.mnemonic.ends_with("_b64") || ins.mnemonic.ends_with("_u64") || ins.mnemonic.ends_with("_i64") {
        2
    } else {
        1
    }
}

fn reads_scc(m: &str) -> bool {
    m.starts_with("s_cselect") || m.starts_with("s_cmov") || m == "s_addc_u32" || m == "s_subb_u32"
}

/// Scalar ALU instructions that do not write SCC.
fn keeps_scc(m: &str) -> bool {
    ["s_mov", "s_movk", "s_cmov", "s_cselect", "s_brev", "s_getpc", "s_setpc", "s_swappc", "s_mul", "s_bitset", "s_pack", "s_bfm", "s_sext", "s_ff", "s_nop"]
        .iter()
        .any(|p| m.starts_with(p))
}

impl Emitter<'_> {
    fn input_unavailable(&self, ins: &Insn, st: &BTreeSet<u16>) -> bool {
        let mut bad = ins.src.iter().enumerate().any(|(i, o)| match o {
            Opnd::Sgpr(n) => (0..src_span(ins, i)).any(|k| st.contains(&(*n as u16 + k))),
            other => key(other).is_some_and(|k| st.contains(&k)),
        });
        if reads_scc(&ins.mnemonic) {
            bad |= st.contains(&SCC);
        }
        bad
    }

    /// Whether dword `k` of an `s_load` through the root table is a value: only root constants
    /// are materialised (as `tb_rc<param>_<i>`); descriptors and descriptor tables are not.
    fn smem_dword_available(&self, idx: usize, ins: &Insn, k: u32) -> bool {
        if self.smem_global.contains(&idx) {
            return true;
        }
        if matches!(self.res.get(&idx).and_then(|v| v.first()), Some(super::Res::CodeTbl)) {
            return true;
        }
        let Some(&off) = self.rm_loads.get(&idx) else { return false };
        let _ = ins;
        matches!(
            self.rm_param(off / 4 + k).and_then(|(p, _)| self.rm_kind_of(p)),
            Some(RootParamKind::Constants { .. })
        )
    }

    fn avail_step(&self, idx: usize, ins: &Insn, st: &mut BTreeSet<u16>) {
        let bad_in = self.input_unavailable(ins, st);
        // Destination registers.
        let span = dst_span(ins);
        let mut dsts: Vec<u16> = Vec::new();
        for o in &ins.dst {
            match o {
                Opnd::Sgpr(n) => dsts.extend((0..span).map(|k| *n as u16 + k)),
                other => dsts.extend(key(other)),
            }
        }
        match ins.enc {
            Enc::Smem if ins.op <= 4 => {
                if let Some(Opnd::Sgpr(d)) = ins.dst.first() {
                    for k in 0..span {
                        let r = *d as u16 + k;
                        if self.smem_dword_available(idx, ins, k as u32) {
                            st.remove(&r);
                        } else {
                            st.insert(r);
                        }
                    }
                }
            }
            // Memory reads and constant-buffer loads produce data, not symbols.
            Enc::Smem | Enc::Mubuf | Enc::Mtbuf | Enc::Mimg | Enc::Flat | Enc::Ds => {
                for r in dsts {
                    st.remove(&r);
                }
            }
            _ => {
                // v_readfirstlane copies a lane of a VGPR: as available as that VGPR.
                for r in dsts {
                    if bad_in {
                        st.insert(r);
                    } else {
                        st.remove(&r);
                    }
                }
                if matches!(ins.enc, Enc::Sopc) || (matches!(ins.enc, Enc::Sop1 | Enc::Sop2 | Enc::Sopk) && !keeps_scc(&ins.mnemonic)) {
                    if bad_in {
                        st.insert(SCC);
                    } else {
                        st.remove(&SCC);
                    }
                }
            }
        }
    }

    /// Every `s_load` classified as a global read must have an address (both halves, and a
    /// register offset if any) made of real values.
    pub(super) fn check_smem_global(&self) -> Result<(), String> {
        if self.smem_global.is_empty() {
            return Ok(());
        }
        let (ranges, succs) = cfg_of(&self.insns);
        let mut entry: BTreeSet<u16> = BTreeSet::new();
        entry.extend(0..self.user_base() as u16);
        let mut states: Vec<Option<BTreeSet<u16>>> = vec![None; ranges.len()];
        states[0] = Some(entry);
        let mut work = vec![0usize];
        let mut budget = 20_000usize;
        while let Some(b) = work.pop() {
            if budget == 0 {
                return Err("scalar value availability did not converge".into());
            }
            budget -= 1;
            let mut st = states[b].clone().unwrap();
            for i in ranges[b].clone() {
                self.avail_step(i, &self.insns[i], &mut st);
            }
            for &s in &succs[b] {
                let changed = match &mut states[s] {
                    slot @ None => {
                        *slot = Some(st.clone());
                        true
                    }
                    Some(o) => {
                        let n = o.len();
                        o.extend(st.iter().copied());
                        o.len() != n
                    }
                };
                if changed && !work.contains(&s) {
                    work.push(s);
                }
            }
        }
        for (b, r) in ranges.iter().enumerate() {
            let Some(mut st) = states[b].clone() else { continue };
            for i in r.clone() {
                let ins = &self.insns[i];
                if self.smem_global.contains(&i) {
                    if let Some(Opnd::Sgpr(base)) = ins.src.first() {
                        let mut regs = vec![*base as u16, *base as u16 + 1];
                        let soff = ins.raw.get(1).copied().unwrap_or(0) >> 25;
                        if soff != 0x7d && soff <= 107 {
                            regs.push(soff as u16);
                        }
                        if let Some(r) = regs.into_iter().find(|r| st.contains(r)) {
                            return Err(format!(
                                "s_load at {:#x}: the address in s{base}:s{} is not a value the shader holds at run time (s{r} derives from a root-table pointer or a descriptor word, which exist only symbolically); reading memory through it would read the wrong address",
                                ins.offset,
                                base + 1
                            ));
                        }
                    }
                }
                self.avail_step(i, ins, &mut st);
            }
        }
        Ok(())
    }
}
