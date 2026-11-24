use tombox_xshader::hlsl::{bl_alias_space, BL_ALIASES, TB_TABLE_FALLBACK_SPACE};

const PARAM_TABLE: u32 = 0;
const RANGE_SRV: u32 = 0;
const RANGE_UAV: u32 = 1;
const RANGE_CBV: u32 = 2;
const RANGE_SAMPLER: u32 = 3;

fn rd(b: &[u8], o: usize) -> Result<u32, String> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| format!("RTS0 read at {o:#x} out of bounds ({} bytes)", b.len()))
}

fn wr(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn add_aliases(body: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let version = rd(body, 0)?;
    let rs = match version {
        1 => 20,
        2 | 3 => 24,
        v => return Err(format!("RTS0 version {v} is not supported")),
    };
    let (np, po) = (rd(body, 4)? as usize, rd(body, 8)? as usize);
    let mut out = body.to_vec();
    let mut added = 0;
    for i in 0..np {
        let at = po + i * 12;
        if rd(body, at)? != PARAM_TABLE {
            continue;
        }
        let payload = rd(body, at + 8)? as usize;
        let (nr, ro) = (rd(body, payload)? as usize, rd(body, payload + 4)? as usize);
        let mut ranges: Vec<Vec<u8>> = Vec::with_capacity(nr);
        let mut extra: Vec<Vec<u8>> = Vec::new();
        let mut have: Vec<(u32, u32, u32)> = Vec::with_capacity(nr);
        let mut next = 0u32;
        for r in 0..nr {
            let ra = ro + r * rs;
            let d = body.get(ra..ra + rs).ok_or("RTS0 range out of bounds")?;
            let (ty, num, space, off) = (rd(d, 0)?, rd(d, 4)?, rd(d, 12)?, rd(d, rs - 4)?);
            let start = if off == u32::MAX { next } else { off };
            next = start.saturating_add(num);
            have.push((ty, space, start));
        }
        next = 0;
        for r in 0..nr {
            let ra = ro + r * rs;
            let d = body.get(ra..ra + rs).ok_or("RTS0 range out of bounds")?.to_vec();
            let (ty, num, space, off) = (rd(&d, 0)?, rd(&d, 4)?, rd(&d, 12)?, rd(&d, rs - 4)?);
            let start = if off == u32::MAX { next } else { off };
            next = start.saturating_add(num);
            let positional = (TB_TABLE_FALLBACK_SPACE..TB_TABLE_FALLBACK_SPACE + 4096).contains(&space);
            if (ty == RANGE_SRV || ty == RANGE_UAV) && space < 100_000 && !positional && start != u32::MAX {
                for k in 1..=BL_ALIASES {
                    if have.contains(&(ty, bl_alias_space(k, space), start)) {
                        continue;
                    }
                    let mut a = d.clone();
                    wr(&mut a, 12, bl_alias_space(k, space));
                    wr(&mut a, rs - 4, start);
                    extra.push(a);
                }
            }
            ranges.push(d);
        }
        if extra.is_empty() {
            continue;
        }
        let new_ro = out.len();
        for r in ranges.iter().chain(extra.iter()) {
            out.extend_from_slice(r);
        }
        added += extra.len();
        wr(&mut out, payload, (ranges.len() + extra.len()) as u32);
        wr(&mut out, payload + 4, new_ro as u32);
    }
    Ok((added > 0).then_some(out))
}

/// Give every descriptor table positional ranges (see `TB_TABLE_FALLBACK_SPACE`): for table parameter `p`, in register
/// space `TB_TABLE_FALLBACK_SPACE + p`, an unbounded SRV range, an unbounded UAV range and a constant-buffer range
/// (`cbv_as_raw`: the SRV the constant-buffer rewrite turns CBVs into, in the space it maps that space to), each
/// starting at the table's first descriptor with register 0, so that register N of those ranges is slot N.
/// Returns `None` when the tables already have them (or there are no tables).
pub fn add_positional(body: &[u8], cbv_as_raw: bool) -> Result<Option<Vec<u8>>, String> {
    let version = rd(body, 0)?;
    let rs = match version {
        1 => 20,
        2 | 3 => 24,
        v => return Err(format!("RTS0 version {v} is not supported")),
    };
    let (np, po) = (rd(body, 4)? as usize, rd(body, 8)? as usize);
    let mut out = body.to_vec();
    let mut added = 0;
    for i in 0..np {
        let at = po + i * 12;
        if rd(body, at)? != PARAM_TABLE {
            continue;
        }
        let space = TB_TABLE_FALLBACK_SPACE + i as u32;
        let cbv_space = if cbv_as_raw { crate::cbv_raw::map_space(space)? } else { space };
        let payload = rd(body, at + 8)? as usize;
        let (nr, ro) = (rd(body, payload)? as usize, rd(body, payload + 4)? as usize);
        if nr == 0 {
            continue;
        }
        let mut ranges: Vec<Vec<u8>> = Vec::with_capacity(nr + 3);
        for r in 0..nr {
            let ra = ro + r * rs;
            ranges.push(body.get(ra..ra + rs).ok_or("RTS0 range out of bounds")?.to_vec());
        }
        let have = |ty: u32, sp: u32| ranges.iter().any(|d| rd(d, 0).ok() == Some(ty) && rd(d, 12).ok() == Some(sp));
        // A table that holds samplers holds nothing else (the sampler heap is a different heap).
        if ranges.iter().any(|d| rd(d, 0).ok() == Some(RANGE_SAMPLER)) {
            continue;
        }
        let template = ranges[0].clone();
        let mut extra = Vec::new();
        for (ty, sp) in [(RANGE_SRV, space), (RANGE_UAV, space), (if cbv_as_raw { RANGE_SRV } else { RANGE_CBV }, cbv_space)] {
            if have(ty, sp) {
                continue;
            }
            let mut a = template.clone();
            wr(&mut a, 0, ty);
            wr(&mut a, 4, u32::MAX);
            wr(&mut a, 8, 0);
            wr(&mut a, 12, sp);
            wr(&mut a, rs - 4, 0);
            extra.push(a);
        }
        if extra.is_empty() {
            continue;
        }
        let new_ro = out.len();
        for r in ranges.iter().chain(extra.iter()) {
            out.extend_from_slice(r);
        }
        added += extra.len();
        wr(&mut out, payload, (ranges.len() + extra.len()) as u32);
        wr(&mut out, payload + 4, new_ro as u32);
    }
    Ok((added > 0).then_some(out))
}

/// Give the root signature the bank of static samplers (`tombox_xshader::hlsl::litsamp`) the translator binds a sampler
/// to when the console shader builds that sampler itself from constants. Static samplers the signature already has are
/// kept. Returns `None` when the bank is already there.
pub fn add_literal_samplers(body: &[u8]) -> Result<Option<Vec<u8>>, String> {
    use tombox_xshader::hlsl::litsamp;
    let version = rd(body, 0)?;
    if !(1..=3).contains(&version) {
        return Err(format!("RTS0 version {version} is not supported"));
    }
    let stride = litsamp::rts0_stride(version);
    let (n, off) = (rd(body, 12)? as usize, rd(body, 16)? as usize);
    let existing: &[u8] = if n == 0 { &[] } else { body.get(off..off + n * stride).ok_or("RTS0 static samplers out of bounds")? };
    for i in 0..n {
        if rd(existing, i * stride + 44)? == litsamp::TB_LITSAMP_SPACE {
            return Ok(None);
        }
    }
    let mut out = body.to_vec();
    let new_off = out.len();
    out.extend_from_slice(existing);
    out.extend_from_slice(&litsamp::rts0_bytes(version));
    wr(&mut out, 12, (n + litsamp::bank().len()) as u32);
    wr(&mut out, 16, new_off as u32);
    Ok(Some(out))
}

pub fn replace_rts0(blob: &[u8], body: &[u8]) -> Result<Vec<u8>, String> {
    if blob.get(0..4) != Some(b"DXBC") {
        return Ok(body.to_vec());
    }
    let n = rd(blob, 28)? as usize;
    let mut chunks: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(n);
    for i in 0..n {
        let o = rd(blob, 32 + i * 4)? as usize;
        let size = rd(blob, o + 4)? as usize;
        let cc: [u8; 4] = blob.get(o..o + 4).ok_or("DXBC chunk out of bounds")?.try_into().unwrap();
        let data = if &cc == b"RTS0" { body.to_vec() } else { blob.get(o + 8..o + 8 + size).ok_or("DXBC chunk out of bounds")?.to_vec() };
        chunks.push((cc, data));
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"DXBC");
    out.extend_from_slice(&[0u8; 16]);
    out.extend_from_slice(&blob[20..24]);
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&(n as u32).to_le_bytes());
    let mut o = 32 + n * 4;
    for (_, d) in &chunks {
        out.extend_from_slice(&(o as u32).to_le_bytes());
        o += 8 + d.len();
    }
    for (cc, d) in &chunks {
        out.extend_from_slice(cc);
        out.extend_from_slice(&(d.len() as u32).to_le_bytes());
        out.extend_from_slice(d);
    }
    let total = out.len() as u32;
    wr(&mut out, 24, total);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbounded_srv_ranges_get_alias_spaces_at_the_same_offsets() {
        let rs = 24;
        let mut b = vec![0u8; 24 + 12 + 8 + 2 * rs];
        let w = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        w(&mut b, 0, 2);
        w(&mut b, 4, 1);
        w(&mut b, 8, 24);
        w(&mut b, 32, 36);
        w(&mut b, 36, 2);
        w(&mut b, 40, 44);
        for (k, r) in [[1u32, 4, 0, 0, 0, 0], [0, u32::MAX, 0, 5, 0, u32::MAX]].iter().enumerate() {
            for (i, v) in r.iter().enumerate() {
                w(&mut b, 44 + k * rs + i * 4, *v);
            }
        }
        let o = add_aliases(&b).unwrap().unwrap();
        let p = tombox_xshader::meta::parse_rts0(&o).unwrap();
        let tombox_xshader::meta::RootParamKind::Table(r) = &p.params[0].kind else { panic!() };
        assert_eq!(r.len(), 2 + 2 * BL_ALIASES as usize);
        assert_eq!((r[1].offset_in_table, r[1].space), (u32::MAX, 5));
        for k in 1..=BL_ALIASES {
            let a = &r[1 + k as usize];
            assert_eq!((a.offset_in_table, a.space, a.num, a.range_type), (0, bl_alias_space(k, 0), 4, 1));
            let a = &r[1 + (BL_ALIASES + k) as usize];
            assert_eq!((a.offset_in_table, a.space, a.num, a.range_type), (4, bl_alias_space(k, 5), u32::MAX, 0));
        }
        let mut no = b.clone();
        w(&mut no, 44, 2);
        w(&mut no, 44 + rs, 3);
        assert!(add_aliases(&no).unwrap().is_none());
    }

    #[test]
    fn aliasing_an_already_aliased_root_signature_changes_nothing() {
        let rs = 24;
        let mut b = vec![0u8; 24 + 12 + 8 + 2 * rs];
        let w = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        w(&mut b, 0, 2);
        w(&mut b, 4, 1);
        w(&mut b, 8, 24);
        w(&mut b, 32, 36);
        w(&mut b, 36, 2);
        w(&mut b, 40, 44);
        for (k, r) in [[1u32, 4, 0, 0, 0, 0], [0, u32::MAX, 0, 5, 0, u32::MAX]].iter().enumerate() {
            for (i, v) in r.iter().enumerate() {
                w(&mut b, 44 + k * rs + i * 4, *v);
            }
        }
        let once = add_aliases(&b).unwrap().unwrap();
        assert!(add_aliases(&once).unwrap().is_none(), "a second pass must not add duplicate alias ranges");
        let p = tombox_xshader::meta::parse_rts0(&once).unwrap();
        let tombox_xshader::meta::RootParamKind::Table(r) = &p.params[0].kind else { panic!() };
        assert_eq!(r.len(), 2 + 2 * BL_ALIASES as usize);
    }

    #[test]
    fn dxbc_container_is_rebuilt_around_the_new_rts0() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"DXBC");
        blob.extend_from_slice(&[7u8; 16]);
        blob.extend_from_slice(&1u32.to_le_bytes());
        blob.extend_from_slice(&0u32.to_le_bytes());
        blob.extend_from_slice(&2u32.to_le_bytes());
        blob.extend_from_slice(&40u32.to_le_bytes());
        blob.extend_from_slice(&52u32.to_le_bytes());
        blob.extend_from_slice(b"RTS0");
        blob.extend_from_slice(&4u32.to_le_bytes());
        blob.extend_from_slice(&[1, 2, 3, 4]);
        blob.extend_from_slice(b"XTRA");
        blob.extend_from_slice(&2u32.to_le_bytes());
        blob.extend_from_slice(&[9, 9]);
        let o = replace_rts0(&blob, &[5u8; 12]).unwrap();
        let c = tombox_xshader::Dxbc::parse(&o).unwrap();
        assert_eq!(c.chunk(b"RTS0").unwrap().size, 12);
        assert_eq!(c.chunk(b"XTRA").unwrap().size, 2);
        assert_eq!(rd(&o, 24).unwrap() as usize, o.len());
    }
}

#[cfg(test)]
mod positional_tests {
    use super::*;

    fn table_rts0(range_types: &[u32]) -> Vec<u8> {
        let rs = 24;
        let mut b = vec![0u8; 24 + 12 + 8 + range_types.len() * rs];
        let w = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        w(&mut b, 0, 2);
        w(&mut b, 4, 1);
        w(&mut b, 8, 24);
        w(&mut b, 32, 36);
        w(&mut b, 36, range_types.len() as u32);
        w(&mut b, 40, 44);
        for (k, ty) in range_types.iter().enumerate() {
            let at = 44 + k * rs;
            w(&mut b, at, *ty);
            w(&mut b, at + 4, 1);
            w(&mut b, at + 12, 1);
            w(&mut b, at + 20, u32::MAX);
        }
        b
    }

    #[test]
    fn every_descriptor_table_gets_positional_ranges_once() {
        let b = table_rts0(&[0, 2]);
        let o = add_positional(&b, true).unwrap().expect("positional ranges are added");
        let p = tombox_xshader::meta::parse_rts0(&o).unwrap();
        let tombox_xshader::meta::RootParamKind::Table(r) = &p.params[0].kind else { panic!() };
        assert_eq!(r.len(), 2 + 3);
        let space = TB_TABLE_FALLBACK_SPACE;
        let raw_space = crate::cbv_raw::map_space(space).unwrap();
        for (ty, sp) in [(0, space), (1, space), (0, raw_space)] {
            let x = r.iter().find(|x| x.range_type == ty && x.space == sp).unwrap_or_else(|| panic!("no range of type {ty} in space {sp}"));
            assert_eq!((x.num, x.base_register, x.offset_in_table), (u32::MAX, 0, 0), "unbounded, register N = slot N");
        }
        assert!(add_positional(&o, true).unwrap().is_none(), "idempotent");
        let aliased = add_aliases(&o).unwrap().expect("the declared SRV range is aliased");
        let p = tombox_xshader::meta::parse_rts0(&aliased).unwrap();
        let tombox_xshader::meta::RootParamKind::Table(r) = &p.params[0].kind else { panic!() };
        assert!(!r.iter().any(|x| (1..=BL_ALIASES).any(|k| x.space == bl_alias_space(k, space))), "positional ranges are not aliased");
        // constant buffers that stay constant buffers get a real CBV range
        let plain = add_positional(&b, false).unwrap().unwrap();
        let p = tombox_xshader::meta::parse_rts0(&plain).unwrap();
        let tombox_xshader::meta::RootParamKind::Table(r) = &p.params[0].kind else { panic!() };
        assert!(r.iter().any(|x| x.range_type == 2 && x.space == space && x.num == u32::MAX));
    }

    #[test]
    fn sampler_tables_stay_sampler_only() {
        assert!(add_positional(&table_rts0(&[3]), true).unwrap().is_none());
        assert!(add_positional(&table_rts0(&[]), true).unwrap().is_none());
    }

    #[test]
    fn the_literal_sampler_bank_is_appended_once_and_the_titles_own_static_samplers_are_kept() {
        use tombox_xshader::hlsl::litsamp;
        let w = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        // root signature 1.1 with no parameters and one static sampler of the title's at s7
        let mut b = vec![0u8; 24 + 52];
        w(&mut b, 0, 2);
        w(&mut b, 8, 24);
        w(&mut b, 12, 1);
        w(&mut b, 16, 24);
        w(&mut b, 24 + 40, 7);
        let out = add_literal_samplers(&b).unwrap().expect("bank added");
        let bank = litsamp::bank().len();
        assert_eq!(rd(&out, 12).unwrap() as usize, 1 + bank);
        let off = rd(&out, 16).unwrap() as usize;
        assert_eq!(rd(&out, off + 40).unwrap(), 7, "the title's own sampler comes first, untouched");
        let last = off + bank * 52;
        assert_eq!((rd(&out, last + 40).unwrap() as usize, rd(&out, last + 44).unwrap()), (bank - 1, litsamp::TB_LITSAMP_SPACE));
        assert_eq!(out.len(), b.len() + (1 + bank) * 52);
        assert!(add_literal_samplers(&out).unwrap().is_none(), "already there");
        let mut v3 = vec![0u8; 24];
        w(&mut v3, 0, 3);
        w(&mut v3, 8, 24);
        w(&mut v3, 16, 24);
        let out = add_literal_samplers(&v3).unwrap().expect("bank added to a 1.2 signature");
        assert_eq!(out.len(), 24 + bank * 56);
        assert!(add_literal_samplers(&[0u8; 8]).is_err());
    }
}
