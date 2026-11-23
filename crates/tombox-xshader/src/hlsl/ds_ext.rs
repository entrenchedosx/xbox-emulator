use super::{r, Emitter};
use crate::rdna2::{Insn, Opnd};

pub(super) fn is_wide_lds(m: &str) -> bool {
    matches!(m, "ds_write_b64" | "ds_write2_b64" | "ds_write2st64_b64" | "ds_read_b64" | "ds_read2_b64" | "ds_read2st64_b64")
}

fn vg(o: &Opnd, k: u8) -> Result<Opnd, String> {
    match o {
        Opnd::Vgpr(n) => Ok(Opnd::Vgpr(n + k)),
        _ => Err("64-bit LDS access with a non-VGPR operand".into()),
    }
}

fn part(ins: &Insn, m: &str, op: u32, off: u32, src: Vec<Opnd>, dst: Vec<Opnd>) -> Insn {
    let mut p = ins.clone();
    p.mnemonic = m.to_string();
    p.op = op;
    p.aux.imm = off as i64;
    p.src = src;
    p.dst = dst;
    p
}

pub(super) fn split_wide_lds(ins: &Insn) -> Result<Vec<Insn>, String> {
    let m = ins.mnemonic.as_str();
    let off = ins.aux.imm as u32 & 0xffff;
    let (o0, o1) = (off & 0xff, (off >> 8) & 0xff);
    let scale = if m.contains("st64") { 512 } else { 8 };
    let bytes: Vec<u32> = match m {
        "ds_write_b64" | "ds_read_b64" => vec![off, off + 4],
        _ => vec![o0 * scale, o0 * scale + 4, o1 * scale, o1 * scale + 4],
    };
    if bytes.iter().any(|&b| b > 0xffff) {
        return Err(format!("{m}: offset {off:#x} reaches past the 16-bit DS offset"));
    }
    let addr = ins.src[0];
    if m.starts_with("ds_write") {
        let data: Vec<Opnd> = match m {
            "ds_write_b64" => vec![ins.src[1], vg(&ins.src[1], 1)?],
            _ => vec![ins.src[1], vg(&ins.src[1], 1)?, ins.src[2], vg(&ins.src[2], 1)?],
        };
        return Ok(bytes.iter().zip(data).map(|(&b, d)| part(ins, "ds_write_b32", 0x0d, b, vec![addr, d, Opnd::Vgpr(0)], vec![Opnd::Vgpr(0)])).collect());
    }
    let vd = match ins.dst.first() {
        Some(Opnd::Vgpr(v)) => *v,
        _ => return Err(format!("{m}: bad vdst")),
    };
    let mut order: Vec<usize> = (0..bytes.len()).collect();
    if let Opnd::Vgpr(a) = addr {
        if let Some(k) = order.iter().position(|&k| vd + k as u8 == a) {
            let x = order.remove(k);
            order.push(x);
        }
    }
    Ok(order
        .into_iter()
        .map(|k| part(ins, "ds_read_b32", 0x36, bytes[k], vec![addr, Opnd::Vgpr(0), Opnd::Vgpr(0)], vec![Opnd::Vgpr(vd + k as u8)]))
        .collect())
}

impl Emitter<'_> {
    pub(super) fn cs_threads(&self) -> (u32, u32, u32) {
        let p = self.ex.isa_preamble;
        if p[1..].iter().all(|&n| n == 0) {
            return self.ex.psv_numthreads.map(|[x, y, z]| (x, y, z)).unwrap_or((1, 1, 1));
        }
        (p[1].max(1), p[2].max(1), p[3].max(1))
    }

    fn xlane_threads(&self) -> Result<(u32, u32, u32), String> {
        let (x, y, z) = self.cs_threads();
        let plane = x.checked_mul(y).ok_or("invalid group dimensions for DS lane exchange")?;
        let n = plane.checked_mul(z).filter(|n| (1..=1024).contains(n)).ok_or("invalid group size for DS lane exchange")?;
        Ok((x, plane, n))
    }

    pub(super) fn one_wave_lds(&self) -> bool {
        if self.ex.stage != crate::Stage::Compute {
            return false;
        }
        if self.insns.iter().any(|i| i.enc == crate::rdna2::Enc::Sopp && i.op == 0x0a) {
            return false;
        }
        let p = self.ex.isa_preamble;
        if p[1] == 0 && p[2] == 0 && p[3] == 0 {
            return false;
        }
        let n = (p[1].max(1) as u64).saturating_mul(p[2].max(1) as u64).saturating_mul(p[3].max(1) as u64);
        n > 0 && n <= 64 && self.insns.iter().any(|i| i.enc == crate::rdna2::Enc::Ds && i.mnemonic != "ds_swizzle_b32")
    }

    pub(super) fn xlane_header(&self) -> Result<Option<String>, String> {
        if !self.is_cs || !self.body.contains("tb_xl[") {
            return Ok(None);
        }
        if self.uses_lds {
            if self.ex.stage != crate::Stage::Compute {
                return Err("DS lane exchange with LDS requires a compute workgroup".into());
            }
            // The existing LDS mirror already uses the full 32 KiB groupshared budget.
            // Borrow its first three words per thread for the exchange and restore them
            // before any lane resumes translating LDS accesses.
            return Ok(Some("#define TB_XL_GS 1\n#define TB_XL_SAVE_LDS 1\n#define tb_xl tb_lds".into()));
        }
        let (_, _, n) = self.xlane_threads()?;
        Ok(Some(format!("#define TB_XL_GS 1\ngroupshared uint tb_xl[{}];", 3 * n)))
    }

    pub(super) fn emit_lds_wrxchg2(&mut self, ins: &Insn) -> Result<(), String> {
        let m = ins.mnemonic.as_str();
        let (lo, hi) = match ins.dst.first() {
            Some(Opnd::Vgpr(v)) if *v < 255 => (Opnd::Vgpr(*v), Opnd::Vgpr(*v + 1)),
            _ => return Err(format!("{m}: bad vdst")),
        };
        self.uses_lds = true;
        self.lds_order(true, true);
        let off = ins.aux.imm as u32;
        let step = if m.contains("st64") { 256 } else { 4 };
        for o in &ins.src[..3] {
            self.touch(o);
        }
        let addr = r(&ins.src[0])?;
        let at = |o: u32| format!("tb_lds[(({addr} + {:#x}u) >> 2) & 8191u]", o * step);
        let (x0, x1) = (r(&ins.src[1])?, r(&ins.src[2])?);
        let t = self.tmp;
        self.tmp += 1;
        self.line(&format!(
            "uint t{t}a = 0u; uint t{t}b = 0u; if (exec_lo & 1u) {{ InterlockedExchange({}, {x0}, t{t}a); InterlockedExchange({}, {x1}, t{t}b); }}",
            at(off & 0xff),
            at((off >> 8) & 0xff)
        ));
        self.wv(&lo, &format!("t{t}a"))?;
        self.wv(&hi, &format!("t{t}b"))
    }

    pub(super) fn emit_ds_permute(&mut self, ins: &Insn) -> Result<(), String> {
        self.uses_wave = true;
        let d = *ins.dst.first().ok_or("permute: no vdst")?;
        let swizzle = ins.mnemonic == "ds_swizzle_b32";
        let off = ins.aux.imm as u32 & 0xffff;
        let (addr, data) = if swizzle { (ins.src[0], ins.src[0]) } else { (ins.src[0], ins.src[1]) };
        self.touch(&addr);
        self.touch(&data);
        let back = swizzle || ins.mnemonic == "ds_bpermute_b32";
        let t = self.tmp;
        self.tmp += 1;
        let source_lane = |lane: String| {
            let mask = off & 0x1f;
            if off & 0x8000 == 0 {
                format!("((({lane} & {mask:#x}u) | {:#x}u) ^ {:#x}u)", (off >> 5) & 0x1f, (off >> 10) & 0x1f)
            } else if off < 0xc000 {
                format!("(({lane} & ~3u) | (({:#x}u >> (({lane} & 3u) * 2u)) & 3u))", off & 0xff)
            } else if off < 0xe000 {
                let n = (off >> 5) & 0x1f;
                let amount = if off & 0x400 == 0 { n } else { (32 - n) & 31 };
                format!("(({lane} & {mask:#x}u) | ((({lane} + {amount}u) & 31u) & {:#x}u))", !mask & 0x1f)
            } else {
                let shift = 27 + mask.count_ones();
                let reversed = if shift >= 32 { "0u".to_string() } else { format!("(reversebits({lane}) >> {shift}u)") };
                format!("({reversed} | ({lane} & {mask:#x}u))")
            }
        };
        if swizzle {
            self.line(&format!("uint t{t}m = 0u; uint t{t}d = {}; uint t{t};", r(&data)?));
        } else {
            self.line(&format!("uint t{t}m = ((({}) + {off:#x}u) >> 2) & 31u; uint t{t}d = {}; uint t{t};", r(&addr)?, r(&data)?));
        }
        let shared = self.is_cs && self.brace_depth == 0 && self.loop_depth == 0;
        if shared {
            let (x, plane, n) = self.xlane_threads()?;
            let active = 2 * n;
            self.line("#ifdef TB_XL_GS");
            self.line(&format!("{{ uint t{t}f = gtid.x + gtid.y * {x}u + gtid.z * {plane}u; uint t{t}g = t{t}f & ~31u; uint t{t}j = t{t}f & 31u;"));
            if swizzle {
                self.line(&format!("t{t}m = {};", source_lane(format!("t{t}j"))));
            }
            self.line("GroupMemoryBarrierWithGroupSync();");
            self.line("#ifdef TB_XL_SAVE_LDS");
            self.line(&format!("uint t{t}sd = tb_xl[t{t}f]; uint t{t}sm = tb_xl[t{t}f + {n}u]; uint t{t}sa = tb_xl[t{t}f + {active}u];"));
            self.line("#endif");
            self.line(&format!("tb_xl[t{t}f] = t{t}d; tb_xl[t{t}f + {n}u] = t{t}m; tb_xl[t{t}f + {active}u] = exec_lo & 1u;"));
            self.line("GroupMemoryBarrierWithGroupSync();");
            if back {
                self.line(&format!("uint t{t}i = t{t}g + t{t}m; uint t{t}q = min(t{t}i, {}u); t{t} = (t{t}i < {n}u && tb_xl[t{t}q + {active}u] != 0u) ? tb_xl[t{t}q] : 0u;", n - 1));
            } else {
                self.line(&format!(
                    "t{t} = 0u; for (uint t{t}k = 0u; t{t}k < 32u; t{t}k++) {{ uint t{t}i = min(t{t}g + t{t}k, {}u); if (t{t}g + t{t}k < {n}u && tb_xl[t{t}i + {active}u] != 0u && tb_xl[t{t}i + {n}u] == t{t}j) t{t} = tb_xl[t{t}i]; }}",
                    n - 1
                ));
            }
            // Retire the exchange reads before restoring LDS, then make the original
            // values visible to every lane that can access those addresses.
            self.line("#ifdef TB_XL_SAVE_LDS");
            self.line("GroupMemoryBarrierWithGroupSync();");
            self.line(&format!("tb_xl[t{t}f] = t{t}sd; tb_xl[t{t}f + {n}u] = t{t}sm; tb_xl[t{t}f + {active}u] = t{t}sa;"));
            self.line("GroupMemoryBarrierWithGroupSync();");
            self.line("#endif");
            self.line("}");
            self.line("#else");
        }
        self.line(&format!("{{ uint t{t}l = WaveGetLaneIndex(); uint t{t}g = t{t}l & ~31u;"));
        if swizzle {
            self.line(&format!("t{t}m = {};", source_lane(format!("(t{t}l & 31u)"))));
        }
        if back {
            self.line(&format!("uint t{t}v = WaveReadLaneAt(t{t}d, t{t}g + t{t}m); t{t} = (WaveReadLaneAt(exec_lo & 1u, t{t}g + t{t}m) != 0u) ? t{t}v : 0u; }}"));
        } else {
            self.line(&format!(
                "t{t} = 0u; for (uint t{t}k = 0u; t{t}k < 32u; t{t}k++) {{ uint t{t}x = WaveReadLaneAt(t{t}m, t{t}g + t{t}k); uint t{t}y = WaveReadLaneAt(t{t}d, t{t}g + t{t}k); bool t{t}a = WaveReadLaneAt(exec_lo & 1u, t{t}g + t{t}k) != 0u; if (t{t}a && t{t}x == (t{t}l & 31u)) t{t} = t{t}y; }} }}"
            ));
        }
        if shared {
            self.line("#endif");
        }
        self.wv(&d, &format!("t{t}"))
    }
}
