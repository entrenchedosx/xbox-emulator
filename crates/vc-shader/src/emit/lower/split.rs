//! Wave64 compute programs on a host of 32-lane waves ([`Lower::split`]):
//! deciding whether the program runs exactly as host waves of 32, each
//! holding one half of the console's wave ([`split_plan`]), what that takes
//! (group barriers for LDS, lane exchange through group-shared memory), and
//! the analyses behind it ([`uniform_blocks`], [`uniform_values`],
//! [`lds_plan`]).

use std::collections::{HashMap, HashSet};

use super::super::bitcode::{Attr, Bin, V, pred};
use super::super::shader::flags;
use super::{Lower, Parts, Why, masks_read_as_data};
use crate::emit::wave::{Op, Shape};
use crate::gfx10::{Fields, Operand};
use crate::ir::{EXEC, Inst, Program, Reg, Value};
use crate::native::{ResourceType, RootType, UsedResource};
use crate::resource::{Address, Role, Site, Source};
use crate::structure::{Atom, Bool, LaneExit, Structured};

/// The used-resource entry a descriptor loaded from a table falls in.
pub(super) fn used_kind_of<'a>(
    x: &'a Parts<'a>,
    scalar: &HashMap<Site, Address>,
    src: &Source,
) -> Option<&'a UsedResource> {
    if let Source::Phi { args, .. } | Source::Select { args, .. } | Source::Lanes { args, .. } = src
    {
        return used_kind_of(x, scalar, args.first()?);
    }
    if let Source::LanePhi { args, .. } = src {
        return args
            .iter()
            .flatten()
            .find_map(|a| used_kind_of(x, scalar, a));
    }
    if let Source::Built { address, .. } = src {
        let rp = x.native.layout.get(address.param)?;
        return x
            .native
            .used
            .iter()
            .find(|u| u.root == RootType::Descriptor && u.user_data == rp.user_data);
    }
    let (Source::Loaded { load, first } | Source::Fetched { load, first }) = src else {
        return None;
    };
    let a = scalar.get(load)?;
    let rp = x.native.layout.get(a.param)?;
    if rp.root != RootType::Table {
        return None;
    }
    let dword = (a.offset / 4) + u32::from(*first);
    x.native
        .used
        .iter()
        .filter(|u| {
            u.root == RootType::Table
                && u.user_data == rp.user_data
                && u.table_offset_dwords <= dword
        })
        .max_by_key(|u| u.table_offset_dwords)
}

/// Whether `inst` names `reg` among its source operands (not only as the old
/// value its result merges with).
fn operand_reads(inst: &Inst, reg: Reg) -> bool {
    let Reg::V(n) = reg else { return true };
    match &inst.fields {
        Fields::Valu(v) => {
            v.sdwa.is_some()
                || v.src.iter().any(|o| {
                    matches!(*o, Operand::Vector { reg: r, dwords } if (r..r + dwords.max(1)).contains(&n))
                })
        }
        // Memory and LDS operations name their registers in fields; their
        // writes are whole.
        _ => true,
    }
}

/// The VGPR values that hold the same number in every lane of the wave
/// wherever they are read: made from scalars, constants and memory read at
/// uniform addresses, by instructions every lane runs and writes alike. (A
/// thread id, a result a lane mask or a lane exchange selects, anything
/// computed in lanes that part ways, a loop-carried value that does, are not.)
fn uniform_values(st: &Structured, p: &Program, entered: &[bool]) -> HashSet<Value> {
    let mut varying: HashSet<Value> = p
        .inputs
        .iter()
        .filter(|(r, _)| matches!(r, Reg::V(_)))
        .map(|&(_, v)| v)
        .collect();
    // A join the lanes enter along different edges (per-lane branches into it).
    let joined: Vec<bool> = p
        .blocks
        .iter()
        .map(|blk| {
            blk.preds.iter().all(|&q| match st.exits[q] {
                LaneExit::Branch {
                    when,
                    taken,
                    not_taken,
                } if taken != not_taken => st
                    .bools
                    .support(when)
                    .iter()
                    .all(|a| matches!(a, Atom::Uniform(_))),
                _ => true,
            })
        })
        .collect();
    let mask = |v: &Value| {
        st.masks.contains_key(v) || st.highs.contains_key(v) || st.other.contains_key(v)
    };
    loop {
        let mut changed = false;
        for (b, blk) in p.blocks.iter().enumerate() {
            for phi in &blk.phis {
                if !varying.contains(&phi.value)
                    && (!joined[b] || phi.args.iter().any(|a| varying.contains(a) || mask(a)))
                {
                    changed |= varying.insert(phi.value);
                }
            }
            for (i, inst) in blk.insts.iter().enumerate() {
                let n = inst.name();
                let lane_dependent = n.starts_with("ds_swizzle")
                    || n.contains("permute")
                    || n.starts_with("v_mbcnt")
                    || n == "v_writelane_b32"
                    || matches!(&inst.fields, Fields::Valu(v) if v.dpp.is_some())
                    || st.guards[b][i].is_some()
                    || !entered[b]
                    || inst.reads.iter().any(|(r, v)| {
                        // Every vector instruction reads EXEC (what it narrows
                        // is the guard's) and, to merge a partial write, the
                        // old value of what it writes: no operand's, unless
                        // written (SDWA) or guarded.
                        let kept = inst.writes.iter().any(|w| w.0 == *r)
                            && !operand_reads(inst, *r)
                            && st.guards[b][i].is_none();
                        !kept
                            && (varying.contains(v) || !matches!(r, Reg::S(EXEC | 127)) && mask(v))
                    });
                if lane_dependent {
                    for &(r, v) in &inst.writes {
                        if matches!(r, Reg::V(_)) {
                            changed |= varying.insert(v);
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut all = HashSet::new();
    for blk in &p.blocks {
        all.extend(blk.phis.iter().map(|ph| ph.value));
        for i in &blk.insts {
            all.extend(i.writes.iter().map(|w| w.1));
        }
    }
    all.retain(|v| !varying.contains(v));
    all
}

/// Where a split program needs group barriers for the console's lockstep
/// order of LDS accesses between lanes of both halves.
#[derive(Debug, Default)]
pub(super) struct LdsPlan {
    /// Before the instruction (IR block, index).
    pub(super) before: HashSet<(usize, usize)>,
    /// At the end of the IR block, before its branch.
    pub(super) at_end: HashSet<usize>,
}

/// The barriers the LDS accesses of a wave64 program need when its wave runs
/// as two host waves: one lane's access must follow every access of another
/// lane the console runs first (a write before a read or write, a read before
/// a write), and nothing orders waves but a barrier, which every thread of
/// the group must reach. They go where a hazard is pending (writes since the
/// last barrier meet a read or write; reads meet a write) in a block every
/// thread enters, and at the end of such a block before lanes part ways into a
/// branch. `None`: a hazard inside code only some lanes run, where no barrier
/// can go (two accesses of lanes that may be in different halves, with no
/// point where all lanes meet between them).
/// Diagnostics, `VC_SPLIT_TRUST_PRIVATE_LDS`: [`lds_plan`] takes an LDS
/// hazard in code only some lanes run as no hazard, the LDS being each
/// thread's own (private arrays the compiler keeps there). Unsound where
/// lanes share it; an experiment for programs the plan refuses. Off in
/// builds without diagnostics.
pub fn trust_private_lds() -> bool {
    #[cfg(feature = "diagnostics")]
    {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("VC_SPLIT_TRUST_PRIVATE_LDS").is_some_and(|v| v != "0"))
    }
    #[cfg(not(feature = "diagnostics"))]
    false
}

fn lds_plan(p: &Program, entered: &[bool]) -> Option<LdsPlan> {
    const R: u8 = 1;
    const W: u8 = 2;
    let class = |i: &Inst| -> Option<u8> {
        let n = i.name();
        if !matches!(i.fields, Fields::Ds { .. })
            || n.starts_with("ds_swizzle")
            || n.contains("permute")
        {
            return None;
        }
        Some(if n.starts_with("ds_read") { R } else { W })
    };
    let order = crate::ir::reverse_postorder(&p.blocks);
    let n = p.blocks.len();
    let mut pending = vec![0u8; n];
    for _round in 0..16 {
        let mut plan = LdsPlan::default();
        let mut changed = false;
        for &b in &order {
            let mut s = pending[b];
            for (i, inst) in p.blocks[b].insts.iter().enumerate() {
                if inst.name() == "s_barrier" {
                    s = 0;
                    continue;
                }
                let Some(c) = class(inst) else { continue };
                if (c == W && s != 0) || (c == R && s & W != 0) {
                    if !entered[b] {
                        // Diagnostics: take the program's LDS as private to
                        // each thread (no barrier where none can go).
                        if trust_private_lds() {
                            s |= c;
                            continue;
                        }
                        return None;
                    }
                    plan.before.insert((b, i));
                    s = 0;
                }
                s |= c;
            }
            let parts = p.blocks[b]
                .exit
                .successors()
                .into_iter()
                .any(|t| !entered[t]);
            if entered[b] && s != 0 && parts {
                plan.at_end.insert(b);
                s = 0;
            }
            for t in p.blocks[b].exit.successors() {
                if pending[t] | s != pending[t] {
                    pending[t] |= s;
                    changed = true;
                }
            }
        }
        if !changed {
            return Some(plan);
        }
    }
    None
}

/// The IR blocks every thread of the group enters together (a group barrier
/// is legal only there): what holds in the lanes that enter one (their path
/// from the program's start) comes from scalar conditions alone, which the
/// console's wave, and so the whole group of a split wave64 program, takes
/// alike; and it is in no loop whose lanes leave at different times (a
/// per-lane exit or back edge), where a lane that has left would not arrive.
pub(super) fn uniform_blocks(st: &Structured, p: &Program) -> Vec<bool> {
    let n = p.blocks.len();
    let scalar = |when: Bool| {
        st.bools
            .support(when)
            .iter()
            .all(|a| matches!(a, Atom::Uniform(_)))
    };
    let mut uniform: Vec<bool> = (0..n).map(|b| scalar(st.reach[b])).collect();
    // The blocks each block reaches.
    let reaches: Vec<Vec<bool>> = (0..n)
        .map(|b| {
            let mut seen = vec![false; n];
            let mut stack = p.blocks[b].exit.successors();
            while let Some(t) = stack.pop() {
                if !std::mem::replace(&mut seen[t], true) {
                    stack.extend(p.blocks[t].exit.successors());
                }
            }
            seen
        })
        .collect();
    for (b, from) in reaches.iter().enumerate() {
        // The loop `b` is in: the blocks that reach it and it reaches.
        if !from[b] {
            continue;
        }
        let members: Vec<usize> = (0..n).filter(|&c| from[c] && reaches[c][b]).collect();
        let divergent = members.iter().any(|&u| match st.exits[u] {
            LaneExit::Branch {
                when,
                taken,
                not_taken,
            } if taken != not_taken => {
                members.contains(&taken) != members.contains(&not_taken) && !scalar(when)
            }
            _ => false,
        });
        if divergent {
            for &c in &members {
                uniform[c] = false;
            }
        }
    }
    uniform
}

/// Whether the wave64 compute program `p` runs exactly as host waves of 32,
/// each holding one 32-lane half of the console's wave ([`Lower::split`]).
/// What the two halves could do differently makes it refuse:
///
/// * Lanes read each other only through `ds_swizzle` (groups of 32 lanes),
///   `ds_bpermute` (the RDNA 2 ISA, section 10.4.4: lanes 0..31 and 32..63
///   independently) and DPP (rows of 16 lanes), so each half exchanges
///   inside its own host wave. `v_readlane` of a constant lane (where the
///   whole group arrives) goes through group-shared memory and a barrier
///   ([`Lower::split_readlane`]); `v_writelane` and `v_mbcnt` name lanes of
///   the other half, and a lane mask read or held as a number covers both.
/// * A scalar register holds the same value in both halves: none is a lane's
///   value (`v_readfirstlane` outside a waterfall loop is the first lane of
///   the host wave, the second half's own first lane in one of them).
/// * No lane reads memory another lane wrote: the halves are separate waves,
///   which the console's lockstep order does not hold apart. A program that
///   writes global memory (UAVs, title memory) and also loads from it is
///   refused; reading sampled resources and constants is no hazard. LDS is
///   refused too ([`Lower::split`] is made only without it).
pub(super) fn split_plan(
    x: &Parts,
    st: &Structured,
    p: &Program,
    lds_dwords: u32,
) -> Result<LdsPlan, &'static str> {
    let plan = split_plan_inner(x, st, p, lds_dwords);
    if let Err(why) = plan {
        LAST_STOP.with(|l| l.set(Some(why)));
        *STOPS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(why)
            .or_default() += 1;
    }
    plan
}

/// Wave64 programs that keep 64 lanes, by what stopped the split
/// ([`split_stops`]).
static STOPS: std::sync::Mutex<std::collections::BTreeMap<&'static str, u32>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

thread_local! {
    /// What stopped the split of the program this thread translated last.
    static LAST_STOP: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

/// What stopped the split of the program this thread translated since the
/// last call, if anything did (`vc translate --list` names it per program).
pub fn take_split_stop() -> Option<&'static str> {
    LAST_STOP.with(|l| l.take())
}

/// How many programs [`split_plan`] refused, by reason (`vc translate`
/// lists them with `VC_SPLIT_REPORT`).
pub fn split_stops() -> Vec<(&'static str, u32)> {
    let stops = STOPS.lock().unwrap_or_else(|e| e.into_inner());
    stops.iter().map(|(why, n)| (*why, *n)).collect()
}

fn split_plan_inner(
    x: &Parts,
    st: &Structured,
    p: &Program,
    lds_dwords: u32,
) -> Result<LdsPlan, &'static str> {
    let entered = uniform_blocks(st, p);
    let uniform = uniform_values(st, p, &entered);
    // The block that makes each value: a lane mask is read as a number
    // through a ballot there.
    let def_block: HashMap<Value, usize> = p
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(b, block)| {
            block
                .insts
                .iter()
                .flat_map(move |i| i.writes.iter().map(move |w| (w.1, b)))
                .chain(block.phis.iter().map(move |phi| (phi.value, b)))
        })
        .collect();
    // `v_lshrrev_b32 v, 6 or more, v0` of a 1-D group's thread id: the console
    // wave's index, which the two halves of a wave hold alike (a
    // `v_readfirstlane` of it is the same in both).
    let wave_index: HashSet<Value> = {
        let one_d = matches!(x.native.io, crate::native::StageIo::Cs(cs) if cs.threads[1] == 1 && cs.threads[2] == 1);
        let tid = p
            .inputs
            .iter()
            .find(|(r, _)| *r == Reg::V(0))
            .map(|&(_, v)| v);
        p.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter(|i| i.name() == "v_lshrrev_b32" && one_d)
            .filter(|i| match &i.fields {
                Fields::Valu(v) => matches!(
                    (v.src.first(), v.src.get(1)),
                    (Some(&Operand::Const(k)), Some(&Operand::Vector { reg: 0, .. })) if (134..=191).contains(&k)
                ),
                _ => false,
            })
            .filter(|i| tid.is_some() && i.read(Reg::V(0)) == tid)
            .filter_map(|i| i.writes.first().map(|w| w.1))
            .collect()
    };
    // The instructions inside whole-wave sections: not lowered one by one
    // (their results are what the section left, [`Lower::split_halves`]).
    let mut inside: HashSet<(usize, usize)> = HashSet::new();
    for (b, block) in p.blocks.iter().enumerate() {
        // Reductions and prefix sums the compiler expands over the whole
        // wave: each half runs its host wave operation, and the halves'
        // results are combined through group-shared memory and a barrier
        // where every thread of the group arrives.
        let found = super::super::wave::sections(p, b);
        if !found.is_empty() && !entered[b] {
            return Err(
                "a reduction or prefix sum over the whole wave where the lanes of the group part ways",
            );
        }
        for (&start, sec) in &found {
            if sec.shapes.values().any(|&sh| !halves_combine(sh)) {
                return Err(
                    "a reduction or prefix sum over the whole wave that the halves cannot combine",
                );
            }
            inside.extend((start + 1..sec.end).map(|i| (b, i)));
        }
        for (k, i) in block.insts.iter().enumerate() {
            if inside.contains(&(b, k)) {
                continue;
            }
            let n = i.name();
            // The first lane of a waterfall loop's read is the lane's own;
            // elsewhere a value every lane holds alike is the first lane's.
            let first_ok = n == "v_readfirstlane_b32"
                && (i
                    .writes
                    .first()
                    .is_some_and(|w| x.res.waterfall.reads.contains(&w.1))
                    || match &i.fields {
                        Fields::Valu(v) => match v.src.first() {
                            Some(&Operand::Vector { reg, .. }) => i
                                .read(Reg::V(reg))
                                .is_some_and(|s| uniform.contains(&s) || wave_index.contains(&s)),
                            _ => true,
                        },
                        _ => false,
                    });
            // A lane of a constant number goes through group-shared memory
            // and a barrier, where the whole group arrives.
            let exchange = n == "v_readlane_b32"
                && matches!(&i.fields, Fields::Valu(v)
                    if v.src.get(1).copied().and_then(crate::resource::lane_of).is_some())
                && entered[b];
            if n == "v_readlane_b32" && !exchange {
                return Err(
                    "v_readlane_b32 of a lane that is not constant, or where lanes part ways",
                );
            }
            if n == "v_writelane_b32" {
                return Err("v_writelane_b32");
            }
            if n == "v_readfirstlane_b32" && !first_ok {
                return Err(
                    "v_readfirstlane_b32 of a value not shown to be the same in every lane",
                );
            }
            // A mask read as a number is the ballots of both halves
            // ([`Lower::split_ballot`]): taken where the mask is made, by
            // every thread of the group.
            for m in masks_read_as_data(st, i) {
                let lo = st.highs.get(&m).copied().unwrap_or(m);
                if !st.active.contains(&lo) || def_block.get(&lo).is_some_and(|&d| !entered[d]) {
                    return Err("a lane mask read as a number");
                }
            }
        }
    }
    // Masks held as numbers or constant lane sets name lanes of both halves.
    let whens = st.exits.iter().filter_map(|e| match *e {
        LaneExit::Branch { when, .. } => Some(when),
        _ => None,
    });
    let mut all: Vec<Bool> = st
        .guards
        .iter()
        .enumerate()
        .flat_map(|(b, g)| {
            let inside = &inside;
            g.iter()
                .enumerate()
                .filter(move |(i, _)| !inside.contains(&(b, *i)))
                .filter_map(|(_, g)| *g)
        })
        .chain(whens)
        .collect();
    all.sort_unstable();
    all.dedup();
    if all.into_iter().any(|g| {
        st.bools
            .support(g)
            .iter()
            .any(|a| matches!(a, Atom::Opaque(_) | Atom::Constant(_)))
    }) {
        return Err("a lane mask held as a number or a constant lane set");
    }
    // Global memory: a store or atomic and a load that may read what it
    // wrote (an unordered access view or memory of unknown kind; shader
    // resource views and constants cannot).
    let descriptors: HashMap<(Site, bool), &Source> = x
        .res
        .descriptors
        .iter()
        .map(|d| ((d.site, d.role == Role::Sampler), &d.source))
        .collect();
    let scalar: HashMap<Site, Address> = x.res.scalar.iter().copied().collect();
    let global = |i: &Inst| {
        matches!(
            i.fields,
            Fields::Image { .. } | Fields::Buffer { .. } | Fields::Flat { .. }
        )
    };
    let mut writes: Vec<(usize, usize)> = Vec::new();
    let mut reads: Vec<(usize, usize)> = Vec::new();
    for (b, block) in p.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            let n = inst.name();
            if !global(inst) {
                continue;
            }
            if n.contains("store") || n.contains("atomic") {
                writes.push((b, i));
            }
            if n.contains("atomic")
                || matches!(inst.fields, Fields::Flat { .. }) && !n.contains("store")
            {
                reads.push((b, i));
                continue;
            }
            let load = ["buffer_load", "tbuffer_load", "image_load"]
                .iter()
                .any(|l| n.starts_with(l));
            if load
                && !descriptors
                    .get(&((b, i), false))
                    .and_then(|src| used_kind_of(x, &scalar, src))
                    .is_some_and(|u| matches!(u.kind, ResourceType::Srv | ResourceType::Cbv))
            {
                reads.push((b, i));
            }
        }
    }
    // The console runs a wave's instructions in order, so what a load can
    // see of the other lanes' writes is what earlier instructions wrote: the
    // halves, separate waves, agree only where no load runs after a write
    // (a loop brings a block's own writes back before its loads).
    let mut reach: HashMap<usize, Vec<bool>> = HashMap::new();
    let mut hazard = false;
    for &(wb, wi) in &writes {
        let after = reach.entry(wb).or_insert_with(|| {
            let mut seen = vec![false; p.blocks.len()];
            let mut stack = p.blocks[wb].exit.successors();
            while let Some(y) = stack.pop() {
                if !std::mem::replace(&mut seen[y], true) {
                    stack.extend(p.blocks[y].exit.successors());
                }
            }
            seen
        });
        if reads
            .iter()
            .any(|&(rb, ri)| after[rb] || rb == wb && ri > wi)
        {
            hazard = true;
            break;
        }
    }
    if hazard {
        return Err("global memory written and also read by lanes (halves are separate waves)");
    }
    if lds_dwords > 0 {
        lds_plan(p, &entered).ok_or("an LDS hazard inside code only some lanes run")
    } else {
        Ok(LdsPlan::default())
    }
}

/// Whether the halves of a console wave can combine what a whole-wave
/// section leaves: integer reductions of an associative operation, and
/// sums for prefixes.
fn halves_combine(shape: Shape) -> bool {
    match shape {
        Shape::K(_) | Shape::Value(_) | Shape::Masked(..) => true,
        Shape::Active(op, ..) => identity_of(op).is_some(),
        Shape::Prefix(op, ..) => op == Op::AddU,
        Shape::Half(..) => false,
    }
}

/// What a lane that contributes nothing holds, for the integer operations
/// the halves combine.
fn identity_of(op: Op) -> Option<u32> {
    match op {
        Op::AddU | Op::Or | Op::Xor | Op::MaxU => Some(0),
        Op::And | Op::MinU => Some(u32::MAX),
        Op::MinI => Some(i32::MAX as u32),
        Op::MaxI => Some(i32::MIN as u32),
        _ => None,
    }
}

impl<'a> Lower<'a> {
    /// A group barrier with a shared-memory fence: every thread of the group
    /// must reach it ([`uniform_blocks`]).
    pub(super) fn group_barrier(&mut self) {
        let mode = self.s.m.i32(1 | 8);
        let void = self.s.t.void;
        self.s
            .op(self.cur, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    }

    /// Whether every thread of the group enters IR block `b` together:
    /// what holds in the lanes that enter it comes from scalar conditions
    /// alone ([`uniform_blocks`]), so a group barrier there is legal.
    pub(super) fn uniform_entry(&self, b: usize) -> bool {
        self.uniform.get(b).copied().unwrap_or(false)
    }

    /// Split: `v_readlane_b32` of a constant lane. The lane lives in one of
    /// the two host waves of its console wave: its thread stores the value in
    /// group-shared memory, a group barrier orders the store, and every
    /// thread of the console wave reads it. The barrier needs every thread of
    /// the group to reach it: [`split_ok`] allows the instruction only where
    /// they do. (A lane the group has no thread for reads 0.)
    pub(super) fn split_readlane(&mut self, inst: &Inst, x: V) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let k = v
            .src
            .get(1)
            .copied()
            .and_then(crate::resource::lane_of)
            .ok_or("a lane read at a lane that is not constant")?
            & 63;
        if !self.uniform_entry(self.cur_ir) {
            return Err("a lane read where the lanes of the group part ways");
        }
        self.cross_lane(false)?;
        let slot = self.xchg_used;
        self.xchg_used += 1;
        let threads: u32 = self.s.threads.expect("compute").iter().product();
        let waves = threads.div_ceil(64);
        let xchg = self.s.xchg(self.xchg_slots * waves);
        let f = self.console_flat();
        let c = self.cur;
        let k_lane = self.s.m.i32(k);
        let (k63, k6) = (self.s.m.i32(63), self.s.m.i32(6));
        let lane = self.s.m.bin(c, Bin::And, f, k63);
        let wave = self.s.m.bin(c, Bin::LShr, f, k6);
        let owner = self.s.m.cmp(c, pred::EQ, lane, k_lane);
        let (per_wave, at_slot) = (self.s.m.i32(self.xchg_slots), self.s.m.i32(slot));
        let row = self.s.m.bin(c, Bin::Mul, wave, per_wave);
        let index = self.s.m.bin(c, Bin::Add, row, at_slot);
        let at = self.begin_guarded(Some(owner));
        let p = self.s.m.element(self.cur, xchg, index);
        self.s.m.store(self.cur, p, x);
        self.end_guarded(at, &[]);
        let c = self.cur;
        self.group_barrier();
        let p = self.s.m.element(c, xchg, index);
        let got = self.s.m.load(c, p);
        // Its thread exists: the wave's lane k is thread 64 w + k of the group.
        let first = self.s.m.bin(c, Bin::Shl, wave, k6);
        let thread = self.s.m.bin(c, Bin::Add, first, k_lane);
        let total = self.s.m.i32(threads);
        let exists = self.s.m.cmp(c, pred::IULT, thread, total);
        let zero = self.s.m.i32(0);
        let r = self.s.m.select(c, exists, got, zero);
        // A later pass over this instruction stores again: after every
        // thread has read.
        if self.on_cycle(self.cur_ir) {
            self.group_barrier();
        }
        Ok(r)
    }

    /// The thread's flat index in its group, relabelled to its console lane
    /// (see [`Self::thread_id`]): made in the entry block, once.
    pub(super) fn console_flat(&mut self) -> V {
        if let Some(f) = self.flat_id {
            return f;
        }
        let e = self.s.entry;
        let i32t = self.s.t.i32;
        let flat = self.s.op(
            e,
            "flattenedThreadIdInGroup.i32",
            Attr::ReadNone,
            i32t,
            96,
            &[],
        );
        let first = self.s.op(
            e,
            "waveReadLaneFirst.i32",
            Attr::NoUnwind,
            i32t,
            118,
            &[flat],
        );
        let wave = self.s.wave_size.expect("set with console lanes");
        let mask = self.s.m.i32(!(wave - 1));
        let base = self.s.m.bin(e, Bin::And, first, mask);
        let lane = self
            .s
            .op(e, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let f = self.s.m.bin(e, Bin::Add, base, lane);
        self.s.flags |= flags::WAVE_OPS;
        self.flat_id = Some(f);
        f
    }

    /// The thread's lane in its console wave of 64 (`v_mbcnt` counts the
    /// lanes of both halves below it): not the host lane, which is the
    /// lane in its half.
    pub(super) fn console_lane(&mut self) -> V {
        let f = self.console_flat();
        let k63 = self.s.m.i32(63);
        self.s.m.bin(self.cur, Bin::And, f, k63)
    }

    /// Split: what each half of the thread's console wave holds of `mine`
    /// (`ident` for a half the group has no threads for). The halves store
    /// theirs in group-shared memory, a group barrier orders the stores, and
    /// every thread reads both; a second barrier keeps the next exchange's
    /// stores behind the reads. Every thread of the group must reach `at`:
    /// callers check [`Lower::uniform_entry`] of the block.
    pub(super) fn exchange_halves(
        &mut self,
        at: usize,
        mine: V,
        ident: u32,
    ) -> Result<(V, V), Why> {
        self.cross_lane(false)?;
        let threads: u32 = self.s.threads.expect("compute").iter().product();
        let waves = threads.div_ceil(64);
        let pair = self
            .xchg_slots
            .checked_sub(2)
            .ok_or("no group-shared memory for the halves")?;
        let xchg = self.s.xchg(self.xchg_slots * waves);
        let f = self.console_flat();
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        let (k1, k5, k6) = (k(self, 1), k(self, 5), k(self, 6));
        let wave = self.s.m.bin(at, Bin::LShr, f, k6);
        let upper = self.s.m.bin(at, Bin::LShr, f, k5);
        let half = self.s.m.bin(at, Bin::And, upper, k1);
        let (per_wave, first) = (k(self, self.xchg_slots), k(self, pair));
        let row = self.s.m.bin(at, Bin::Mul, wave, per_wave);
        let row = self.s.m.bin(at, Bin::Add, row, first);
        let own = self.s.m.bin(at, Bin::Add, row, half);
        let p = self.s.m.element(at, xchg, own);
        self.s.m.store(at, p, mine);
        self.barrier_at(at);
        let next = self.s.m.bin(at, Bin::Add, row, k1);
        let p_lo = self.s.m.element(at, xchg, row);
        let p_hi = self.s.m.element(at, xchg, next);
        let lo = self.s.m.load(at, p_lo);
        let hi = self.s.m.load(at, p_hi);
        self.barrier_at(at);
        // The upper half's threads exist where the group has any past 64 w + 32.
        let start = self.s.m.bin(at, Bin::Shl, wave, k6);
        let k32 = k(self, 32);
        let start = self.s.m.bin(at, Bin::Add, start, k32);
        let total = k(self, threads);
        let exists = self.s.m.cmp(at, pred::IULT, start, total);
        let id = k(self, ident);
        let hi = self.s.m.select(at, exists, hi, id);
        Ok((lo, hi))
    }

    /// A group barrier with a shared-memory fence in host block `at`.
    fn barrier_at(&mut self, at: usize) {
        let mode = self.s.m.i32(1 | 8);
        let void = self.s.t.void;
        self.s
            .op(at, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    }

    /// Split: a whole-wave section's reduction or prefix `r` over this host
    /// wave's half, as the console computes it over both: the halves'
    /// totals are exchanged and combined (a prefix: the upper half also
    /// adds what the lower half holds). `arg` is the lanes' masked values.
    pub(super) fn split_halves(
        &mut self,
        op: Op,
        prefix: Option<bool>,
        arg: V,
        r: V,
        float: bool,
    ) -> Result<V, Why> {
        if !self.split {
            return Ok(r);
        }
        let ident = identity_of(op)
            .filter(|_| !float)
            .ok_or("a float reduction over both halves")?;
        if !self.uniform_entry(self.cur_ir) {
            return Err("a whole-wave section where the lanes of the group part ways");
        }
        let c = self.cur;
        let total = match prefix {
            None => r,
            Some(_) => {
                if op != Op::AddU {
                    return Err("a prefix over both halves of an operation other than a sum");
                }
                let (sum, unsigned) = (self.s.m.i8(0), self.s.m.i8(1));
                let i32t = self.s.t.i32;
                self.s.op(
                    c,
                    "waveActiveOp.i32",
                    Attr::NoUnwind,
                    i32t,
                    119,
                    &[arg, sum, unsigned],
                )
            }
        };
        let (lo, hi) = self.exchange_halves(c, total, ident)?;
        if prefix.is_some() {
            // The upper half's lanes also have the lower half's behind them.
            let f = self.console_flat();
            let (k5, k1, zero) = (self.s.m.i32(5), self.s.m.i32(1), self.s.m.i32(0));
            let upper = self.s.m.bin(c, Bin::LShr, f, k5);
            let half = self.s.m.bin(c, Bin::And, upper, k1);
            let is_upper = self.s.m.cmp(c, pred::NE, half, zero);
            let below = self.s.m.select(c, is_upper, lo, zero);
            return Ok(self.s.m.bin(c, Bin::Add, r, below));
        }
        Ok(match op {
            Op::AddU => self.s.m.bin(c, Bin::Add, lo, hi),
            Op::Or => self.s.m.bin(c, Bin::Or, lo, hi),
            Op::And => self.s.m.bin(c, Bin::And, lo, hi),
            Op::Xor => self.s.m.bin(c, Bin::Xor, lo, hi),
            Op::MinU | Op::MaxU | Op::MinI | Op::MaxI => {
                let p = match op {
                    Op::MinU => pred::IULT,
                    Op::MaxU => pred::IUGT,
                    Op::MinI => pred::SLT,
                    _ => pred::SGT,
                };
                let first = self.s.m.cmp(c, p, lo, hi);
                self.s.m.select(c, first, lo, hi)
            }
            _ => return Err("a reduction over both halves of an operation they cannot combine"),
        })
    }

    /// Split: the console's mask as a number, from the ballot of each half
    /// (`xy` is this host wave's, in its first dword): the lower half's
    /// ballot is the mask's low dword, the upper half's its high dword.
    /// `at` is the host block that makes the mask.
    pub(super) fn split_ballot(&mut self, lo: Value, at: usize, xy: (V, V)) -> Result<(V, V), Why> {
        if !self.split {
            return Ok(xy);
        }
        if self
            .def_block
            .get(&lo)
            .is_some_and(|&b| !self.uniform_entry(b))
        {
            return Err("a lane mask read as a number where the lanes of the group part ways");
        }
        self.exchange_halves(at, xy.0, 0)
    }
}
