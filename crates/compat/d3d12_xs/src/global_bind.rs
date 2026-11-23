//! Binding for translated shaders that read or write global memory through a 64-bit
//! GPU virtual address (`flat_*`/`global_*` loads and stores), the `TbGlobal` ABI of
//! `tombox_xshader::global_abi`.
//!
//! D3D12 has no raw GPU pointers in HLSL, so the translator lowers each access to a
//! lookup in a small *window table*: up to `TB_GLOBAL_WINDOWS` byte-address buffers,
//! each covering one VA range `[base, base + size)`. An address outside every window
//! reads zero / drops the write and bumps a fault record.
//!
//! Four root descriptors in space `TB_GLOBAL_SPACE` carry it: a CBV with the window
//! table, one UAV per window, and a UAV for the fault record. They are appended to every
//! root signature that has room (like GDS) and filled when the signature is set on a
//! list. Which resources become windows is a policy, not knowledge of what the shader
//! will touch: the largest registered buffers, plus whichever buffer contained the first
//! address a previous run faulted on. Faults are read back and logged, never silent.

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use compat_common::record;
use tombox_xshader::global_abi::{fault_of, window_cb, Fault, Window, TB_GLOBAL_CBV, TB_GLOBAL_FAULT_BYTES, TB_GLOBAL_FAULT_UAV, TB_GLOBAL_ROOT_DWORDS, TB_GLOBAL_SPACE, TB_GLOBAL_WINDOWS};

use crate::device_wrap::Guid;
use crate::gpu_va::Entry;

const PARAM_SIZE: usize = 32;
const PARAM_CBV: u32 = 2;
const PARAM_UAV: u32 = 4;
/// Root parameters appended per signature: the window-table CBV, one UAV per window, the fault UAV.
pub const PARAM_COUNT: u32 = TB_GLOBAL_WINDOWS as u32 + 2;

const PC_COPY_BUFFER_REGION: usize = 15;
const PC_SET_COMPUTE_ROOT_CBV: usize = 37;
const PC_SET_GRAPHICS_ROOT_CBV: usize = 38;
const PC_SET_COMPUTE_ROOT_UAV: usize = 41;
const PC_SET_GRAPHICS_ROOT_UAV: usize = 42;
const XBOX_SET_COMPUTE_ROOT_SIGNATURE: usize = 33;
const XBOX_SET_GRAPHICS_ROOT_SIGNATURE: usize = 34;
const DEV_CREATE_COMMITTED: usize = 27;
const RES_MAP: usize = 8;
const RES_GET_GPU_VA: usize = 11;
const HEAP_DEFAULT: u32 = 1;
const HEAP_UPLOAD: u32 = 2;
const HEAP_READBACK: u32 = 3;
const STATE_COMMON: u32 = 0;
const STATE_UAV: u32 = 0x8;
const STATE_COPY_DEST: u32 = 0x400;
const STATE_COPY_SOURCE: u32 = 0x800;
const STATE_GENERIC_READ: u32 = 0xac3;
const FLAG_ALLOW_UAV: u32 = 0x4;
const LIST_TYPE_BUNDLE: u32 = 1;
const SMALL_BUFFER: u64 = 256;
const RECOMPUTE_EVERY: Duration = Duration::from_millis(250);
const MAX_HINTS: usize = 8;
const MAX_CBV_BUFFERS: usize = 4096;
const IID_RESOURCE: Guid = Guid { d1: 0x6964_42be, d2: 0xa72e, d3: 0x4059, d4: [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad] };
const IID_DEVICE: Guid = Guid { d1: 0x1898_19f1, d2: 0x1db6, d3: 0x4b57, d4: [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7] };

/// `TOMBOX_GLOBAL_BIND=0` turns the binder off; shaders that need it are then refused.
fn disabled_by(value: Option<&str>) -> bool {
    value.map_or(false, |v| v.trim() == "0")
}

pub(crate) fn refusal(needs_global: bool) -> Option<String> {
    (needs_global && disabled_by(std::env::var("TOMBOX_GLOBAL_BIND").ok().as_deref()))
        .then(|| format!("shader reads or writes global memory through a 64-bit GPU VA and the global-memory binder is turned off (TOMBOX_GLOBAL_BIND=0; space {TB_GLOBAL_SPACE})"))
}

fn root_param(ty: u32, register: u32) -> [u8; PARAM_SIZE] {
    let mut p = [0u8; PARAM_SIZE];
    p[0..4].copy_from_slice(&ty.to_le_bytes());
    p[8..12].copy_from_slice(&register.to_le_bytes());
    p[12..16].copy_from_slice(&TB_GLOBAL_SPACE.to_le_bytes());
    p
}

/// The appended parameters, in index order: window-table CBV, window UAVs, fault UAV.
pub fn params() -> Vec<u8> {
    let mut v = root_param(PARAM_CBV, TB_GLOBAL_CBV).to_vec();
    for i in 0..TB_GLOBAL_WINDOWS as u32 {
        v.extend_from_slice(&root_param(PARAM_UAV, i));
    }
    v.extend_from_slice(&root_param(PARAM_UAV, TB_GLOBAL_FAULT_UAV));
    v
}

/// Append the global-memory parameters if the signature has room. Returns the new
/// parameter bytes and the index of the window-table CBV (the others follow it).
pub fn append_plan(params_in: &[u8], flags: u32) -> Option<(Vec<u8>, u32)> {
    if flags & crate::gds::ROOT_FLAG_LOCAL != 0 || crate::gds::root_cost(params_in) + TB_GLOBAL_ROOT_DWORDS > crate::gds::ROOT_DWORD_LIMIT {
        return None;
    }
    let idx = (params_in.len() / PARAM_SIZE) as u32;
    let mut v = params_in.to_vec();
    v.extend_from_slice(&params());
    Some((v, idx))
}

pub unsafe fn patch_root_signature(blob: *const u8, len: usize) -> Option<(Vec<u8>, u32)> {
    if disabled_by(std::env::var("TOMBOX_GLOBAL_BIND").ok().as_deref()) {
        return None;
    }
    unsafe { crate::gds::patch_with(blob, len, PARAM_COUNT, "global", &append_plan) }
}

static ROOTS: Mutex<Option<HashMap<usize, u32>>> = Mutex::new(None);

pub fn note_root_signature(rs: *mut c_void, idx: u32) {
    static LOGGED: AtomicUsize = AtomicUsize::new(0);
    if LOGGED.fetch_add(1, Ordering::Relaxed) < 8 {
        record("d3d12_xs", "global", &format!("root signature {rs:p}: global-memory parameters at root indices {idx}..{} (CBV, {TB_GLOBAL_WINDOWS} window UAVs, fault UAV; space {TB_GLOBAL_SPACE})", idx + PARAM_COUNT - 1));
    }
    ROOTS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(rs as usize, idx);
}

/// A new signature can reuse the address of a released one; drop any stale index.
pub fn forget(rs: *mut c_void) {
    if let Some(m) = ROOTS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.remove(&(rs as usize));
    }
}

pub fn root_index(rs: *mut c_void) -> Option<u32> {
    ROOTS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&(rs as usize)).copied())
}

fn contains(e: &Entry, va: u64) -> bool {
    va >= e.start && va - e.start < e.len
}

fn usable(e: &Entry) -> bool {
    e.buffer && e.len >= 4 && e.len <= u32::MAX as u64
}

/// Pick the windows. `buffers` is every registered buffer range, largest first; `hints`
/// are addresses a shader faulted on. A buffer containing a hint comes first (largest
/// one if several), the rest are filled with the largest buffers. At most
/// `TB_GLOBAL_WINDOWS`, one per resource, ordered by address. A window's size is a
/// 32-bit offset in the ABI, so a larger buffer cannot be a window.
pub fn choose_windows(buffers: &[Entry], hints: &[u64]) -> Vec<Entry> {
    let mut sorted = buffers.to_vec();
    sorted.sort_by(|a, b| b.len.cmp(&a.len).then(a.start.cmp(&b.start)));
    let buffers = &sorted[..];
    let mut chosen: Vec<Entry> = Vec::new();
    for &h in hints {
        if chosen.len() >= TB_GLOBAL_WINDOWS {
            break;
        }
        if chosen.iter().any(|c| contains(c, h)) {
            continue;
        }
        if let Some(e) = buffers.iter().filter(|e| usable(e) && contains(e, h)).max_by_key(|e| e.len) {
            chosen.push(*e);
        }
    }
    for e in buffers.iter().filter(|e| usable(e)) {
        if chosen.len() >= TB_GLOBAL_WINDOWS {
            break;
        }
        if !chosen.iter().any(|c| c.res == e.res) {
            chosen.push(*e);
        }
    }
    chosen.sort_by_key(|e| e.start);
    chosen
}

pub fn to_windows(chosen: &[Entry]) -> Vec<Window> {
    chosen.iter().map(|e| Window { base: e.start, size: e.len as u32 }).collect()
}

/// A readable description of a fault record, with the windows it missed.
pub fn describe_fault(f: &Fault, windows: &[Window]) -> String {
    let w: Vec<String> = windows.iter().map(|w| format!("{:#x}+{:#x}", w.base, w.size)).collect();
    format!("{} global-memory access(es) outside the windows [{}]; first address {:#x}", f.count, w.join(", "), f.first_va)
}

struct FaultBufs {
    res: usize,
    va: u64,
    readback: usize,
    readback_ptr: usize,
}

struct Bound {
    key: Vec<(u64, u32, usize)>,
    uav_va: Vec<u64>,
    cbv_va: u64,
}

struct Shared {
    fault: Option<FaultBufs>,
    bound: Option<Bound>,
    hints: Vec<u64>,
    generation: u64,
    checked: Instant,
    seen_faults: u32,
    cbv_buffers: usize,
    logs: usize,
}

static S: Mutex<Option<Shared>> = Mutex::new(None);
static USED: Mutex<Option<HashSet<usize>>> = Mutex::new(None);
static PREV_CROOT: AtomicUsize = AtomicUsize::new(0);
static PREV_GROOT: AtomicUsize = AtomicUsize::new(0);

fn shared<R>(f: impl FnOnce(&mut Shared) -> R) -> R {
    let mut g = S.lock().unwrap_or_else(|e| e.into_inner());
    let s = g.get_or_insert_with(|| Shared { fault: None, bound: None, hints: Vec::new(), generation: u64::MAX, checked: Instant::now(), seen_faults: 0, cbv_buffers: 0, logs: 0 });
    f(s)
}

unsafe fn vslot(obj: *mut c_void, i: usize) -> usize {
    unsafe { *(*(obj as *const *const usize)).add(i) }
}

unsafe fn device_of(list: *mut c_void) -> Option<*mut c_void> {
    let get: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32 = unsafe { core::mem::transmute(vslot(list, 7)) };
    let mut dev: *mut c_void = core::ptr::null_mut();
    (unsafe { get(list, &IID_DEVICE, &mut dev) } >= 0 && !dev.is_null()).then_some(dev)
}

unsafe fn release(o: *mut c_void) {
    let r: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { core::mem::transmute(vslot(o, 2)) };
    unsafe { r(o) };
}

/// A 1-D buffer of `size` bytes in `heap_type`, in `state`.
unsafe fn make_buffer(dev: *mut c_void, heap_type: u32, size: u64, flags: u32, state: u32) -> Option<usize> {
    let heap: [u32; 5] = [heap_type, 0, 0, 1, 1];
    let mut desc = [0u8; 56];
    desc[0..4].copy_from_slice(&1u32.to_le_bytes());
    desc[0x10..0x18].copy_from_slice(&size.to_le_bytes());
    desc[0x18..0x1c].copy_from_slice(&1u32.to_le_bytes());
    desc[0x1c..0x1e].copy_from_slice(&1u16.to_le_bytes());
    desc[0x1e..0x20].copy_from_slice(&1u16.to_le_bytes());
    desc[0x24..0x28].copy_from_slice(&1u32.to_le_bytes());
    desc[0x2c..0x30].copy_from_slice(&1u32.to_le_bytes());
    desc[0x30..0x34].copy_from_slice(&flags.to_le_bytes());
    let create: unsafe extern "system" fn(*mut c_void, *const u32, u32, *const u8, u32, *const c_void, *const Guid, *mut *mut c_void) -> i32 =
        unsafe { core::mem::transmute(vslot(dev, DEV_CREATE_COMMITTED)) };
    let mut res: *mut c_void = core::ptr::null_mut();
    let hr = unsafe { create(dev, heap.as_ptr(), 0, desc.as_ptr(), state, core::ptr::null(), &IID_RESOURCE, &mut res) };
    if hr < 0 || res.is_null() {
        record("d3d12_xs", "global", &format!("buffer ({size:#x} bytes, heap {heap_type}) create FAILED hr={hr:#010x}"));
        return None;
    }
    Some(res as usize)
}

unsafe fn gpu_va(res: usize) -> u64 {
    let f: unsafe extern "system" fn(*mut c_void) -> u64 = unsafe { core::mem::transmute(vslot(res as *mut c_void, RES_GET_GPU_VA)) };
    unsafe { f(res as *mut c_void) }
}

unsafe fn map(res: usize) -> Option<usize> {
    let f: unsafe extern "system" fn(*mut c_void, u32, *const c_void, *mut *mut c_void) -> i32 = unsafe { core::mem::transmute(vslot(res as *mut c_void, RES_MAP)) };
    let mut p: *mut c_void = core::ptr::null_mut();
    (unsafe { f(res as *mut c_void, 0, core::ptr::null(), &mut p) } >= 0 && !p.is_null()).then_some(p as usize)
}

/// The fault record (a device UAV) and its persistently mapped readback twin.
unsafe fn fault_bufs(list: *mut c_void) -> Option<(usize, u64)> {
    if let Some(f) = shared(|s| s.fault.as_ref().map(|f| (f.res, f.va))) {
        return Some(f);
    }
    let dev = unsafe { device_of(list) }?;
    let made = (|| unsafe {
        let res = make_buffer(dev, HEAP_DEFAULT, SMALL_BUFFER, FLAG_ALLOW_UAV, STATE_COMMON)?;
        let readback = make_buffer(dev, HEAP_READBACK, SMALL_BUFFER, 0, STATE_COPY_DEST)?;
        let readback_ptr = map(readback)?;
        Some(FaultBufs { res, va: gpu_va(res), readback, readback_ptr })
    })();
    unsafe { release(dev) };
    let f = made?;
    let out = (f.res, f.va);
    record("d3d12_xs", "global", &format!("fault record: device UAV {:#x} va={:#x}, readback {:#x} (u{TB_GLOBAL_FAULT_UAV}, space{TB_GLOBAL_SPACE})", f.res, f.va, f.readback));
    shared(|s| s.fault = Some(f));
    Some(out)
}

/// A fresh upload buffer holding the window table. Windows change rarely, and a buffer a
/// recorded list still points at must not change, so a changed table gets a new buffer.
unsafe fn table_buffer(list: *mut c_void, table: &[u32]) -> Option<u64> {
    if shared(|s| s.cbv_buffers) >= MAX_CBV_BUFFERS {
        return None;
    }
    let dev = unsafe { device_of(list) }?;
    let made = (|| unsafe {
        let res = make_buffer(dev, HEAP_UPLOAD, SMALL_BUFFER, 0, STATE_GENERIC_READ)?;
        let p = map(res)? as *mut u32;
        core::ptr::copy_nonoverlapping(table.as_ptr(), p, table.len());
        Some(gpu_va(res))
    })();
    unsafe { release(dev) };
    shared(|s| s.cbv_buffers += 1);
    made
}

/// Recompute the windows when the VA registry changed (at most every `RECOMPUTE_EVERY`)
/// or a fault produced a hint, and return `(table CBV address, window UAV addresses)`.
unsafe fn current_plan(list: *mut c_void, dummy: u64) -> Option<(u64, [u64; TB_GLOBAL_WINDOWS])> {
    let now = Instant::now();
    let gen = crate::gpu_va::generation();
    let (stale, hints) = shared(|s| (s.bound.is_none() || (s.generation != gen && now.duration_since(s.checked) >= RECOMPUTE_EVERY) || s.generation == u64::MAX, s.hints.clone()));
    if stale {
        let chosen: Vec<Entry> = choose_windows(&crate::gpu_va::buffers_by_size(), &hints)
            .into_iter()
            .map(|e| crate::placed_offset::true_entry(e, e.start).0)
            .collect();
        let key: Vec<(u64, u32, usize)> = chosen.iter().map(|e| (e.start, e.len as u32, e.res)).collect();
        let changed = shared(|s| s.bound.as_ref().map_or(true, |b| b.key != key));
        if changed {
            let windows = to_windows(&chosen);
            let table = window_cb(&windows, [0, 0]);
            let cbv_va = unsafe { table_buffer(list, &table) }?;
            let mut uav_va = Vec::new();
            for e in &chosen {
                uav_va.push(unsafe { gpu_va(e.res) });
            }
            let log = shared(|s| {
                s.logs += 1;
                s.logs <= 16
            });
            if log {
                let desc: Vec<String> = chosen.iter().map(|e| format!("{:#x}+{:#x} (resource {:#x})", e.start, e.len, e.res)).collect();
                record("d3d12_xs", "global", &format!("windows for 64-bit global memory ({} of {TB_GLOBAL_WINDOWS}): [{}]; hints {hints:x?}", chosen.len(), desc.join(", ")));
            }
            shared(|s| s.bound = Some(Bound { key, uav_va, cbv_va }));
        }
        shared(|s| {
            s.generation = gen;
            s.checked = now;
        });
    }
    shared(|s| {
        let b = s.bound.as_ref()?;
        let mut uavs = [dummy; TB_GLOBAL_WINDOWS];
        for (i, v) in b.uav_va.iter().enumerate().take(TB_GLOBAL_WINDOWS) {
            uavs[i] = *v;
        }
        Some((b.cbv_va, uavs))
    })
}

/// Read the mapped fault record; log new faults and remember the first address as a hint
/// so the next window choice covers it.
fn poll_faults() {
    shared(|s| {
        let Some(f) = s.fault.as_ref() else { return };
        let rec: [u32; 4] = unsafe { core::ptr::read_volatile(f.readback_ptr as *const [u32; 4]) };
        let fault = fault_of(&rec);
        if fault.count == s.seen_faults {
            return;
        }
        s.seen_faults = fault.count;
        let windows: Vec<Window> = s.bound.as_ref().map(|b| b.key.iter().map(|k| Window { base: k.0, size: k.1 }).collect()).unwrap_or_default();
        record("d3d12_xs", "global", &format!("GLOBAL MEMORY FAULT: {}", describe_fault(&fault, &windows)));
        if fault.first_va != 0 && !s.hints.contains(&fault.first_va) && s.hints.len() < MAX_HINTS {
            s.hints.push(fault.first_va);
            s.generation = u64::MAX;
        }
    });
}

fn barriers_allowed(list_type: u32) -> bool {
    list_type != LIST_TYPE_BUNDLE
}

unsafe fn bind(list: *mut c_void, rs: *mut c_void, cbv_slot: usize, uav_slot: usize) {
    let Some(idx) = root_index(rs) else { return };
    let Some((fault_res, fault_va)) = (unsafe { fault_bufs(list) }) else { return };
    let Some((cbv_va, uavs)) = (unsafe { current_plan(list, fault_va) }) else { return };
    poll_faults();
    let first_use = USED.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashSet::new).insert(list as usize);
    if first_use && barriers_allowed(unsafe { crate::fill::list_type(list) }) {
        unsafe { crate::fill::barrier_pub(list, fault_res, STATE_COMMON, STATE_UAV) };
    }
    let set_cbv: unsafe extern "system" fn(*mut c_void, u32, u64) = unsafe { core::mem::transmute(crate::cmdlist_wrap::pc_method(list, cbv_slot)) };
    let set_uav: unsafe extern "system" fn(*mut c_void, u32, u64) = unsafe { core::mem::transmute(crate::cmdlist_wrap::pc_method(list, uav_slot)) };
    unsafe {
        set_cbv(list, idx, cbv_va);
        for (i, va) in uavs.iter().enumerate() {
            set_uav(list, idx + 1 + i as u32, *va);
        }
        set_uav(list, idx + 1 + TB_GLOBAL_WINDOWS as u32, fault_va);
    }
}

/// Before a list that bound the fault record closes: copy it to the readback buffer and
/// return it to COMMON so the next list starts from a known state.
pub unsafe fn on_close(list: *mut c_void) {
    let used = USED.lock().unwrap_or_else(|e| e.into_inner()).as_mut().map_or(false, |s| s.remove(&(list as usize)));
    if !used || !barriers_allowed(unsafe { crate::fill::list_type(list) }) {
        return;
    }
    let Some((res, readback)) = shared(|s| s.fault.as_ref().map(|f| (f.res, f.readback))) else { return };
    unsafe {
        crate::fill::barrier_pub(list, res, STATE_UAV, STATE_COPY_SOURCE);
        let copy: unsafe extern "system" fn(*mut c_void, *mut c_void, u64, *mut c_void, u64, u64) = core::mem::transmute(crate::cmdlist_wrap::pc_method(list, PC_COPY_BUFFER_REGION));
        copy(list, readback as *mut c_void, 0, res as *mut c_void, 0, TB_GLOBAL_FAULT_BYTES as u64);
        crate::fill::barrier_pub(list, res, STATE_COPY_SOURCE, STATE_COMMON);
    }
}

unsafe extern "system" fn h_croot(this: *mut c_void, rs: *mut c_void) {
    let f: unsafe extern "system" fn(*mut c_void, *mut c_void) = unsafe { core::mem::transmute(PREV_CROOT.load(Ordering::Relaxed)) };
    unsafe { f(this, rs) };
    unsafe { bind(this, rs, PC_SET_COMPUTE_ROOT_CBV, PC_SET_COMPUTE_ROOT_UAV) };
}

unsafe extern "system" fn h_groot(this: *mut c_void, rs: *mut c_void) {
    let f: unsafe extern "system" fn(*mut c_void, *mut c_void) = unsafe { core::mem::transmute(PREV_GROOT.load(Ordering::Relaxed)) };
    unsafe { f(this, rs) };
    unsafe { bind(this, rs, PC_SET_GRAPHICS_ROOT_CBV, PC_SET_GRAPHICS_ROOT_UAV) };
}

pub fn install(vt: &[AtomicUsize]) {
    PREV_CROOT.store(vt[XBOX_SET_COMPUTE_ROOT_SIGNATURE].load(Ordering::Relaxed), Ordering::Relaxed);
    PREV_GROOT.store(vt[XBOX_SET_GRAPHICS_ROOT_SIGNATURE].load(Ordering::Relaxed), Ordering::Relaxed);
    vt[XBOX_SET_COMPUTE_ROOT_SIGNATURE].store(h_croot as usize, Ordering::Relaxed);
    vt[XBOX_SET_GRAPHICS_ROOT_SIGNATURE].store(h_groot as usize, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(start: u64, len: u64, res: usize) -> Entry {
        Entry { start, len, res, buffer: true }
    }

    fn u32_at(p: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(p[o..o + 4].try_into().unwrap())
    }

    #[test]
    fn root_parameters_match_the_translator_abi() {
        let p = params();
        assert_eq!(p.len(), PARAM_SIZE * PARAM_COUNT as usize);
        let at = |i: usize| (u32_at(&p, i * PARAM_SIZE), u32_at(&p, i * PARAM_SIZE + 8), u32_at(&p, i * PARAM_SIZE + 12));
        assert_eq!(at(0), (PARAM_CBV, TB_GLOBAL_CBV, TB_GLOBAL_SPACE), "window table is b0");
        assert_eq!(at(1), (PARAM_UAV, 0, TB_GLOBAL_SPACE), "window 0 is u0");
        assert_eq!(at(2), (PARAM_UAV, 1, TB_GLOBAL_SPACE), "window 1 is u1");
        assert_eq!(at(3), (PARAM_UAV, TB_GLOBAL_FAULT_UAV, TB_GLOBAL_SPACE), "fault record is u2");
        // root descriptors cost two DWORDs each: this is what the ABI reserves
        assert_eq!(crate::gds::root_cost(&p), TB_GLOBAL_ROOT_DWORDS);
        assert_eq!((TB_GLOBAL_CBV, TB_GLOBAL_FAULT_UAV, TB_GLOBAL_SPACE), (0, 2, 4_000_003), "the HLSL declares b0, u0..u1 and u2 in this space");
    }

    #[test]
    fn appended_after_existing_parameters_when_there_is_room() {
        let existing = [root_param(PARAM_UAV, 7), root_param(PARAM_CBV, 3)].concat();
        let (v, idx) = append_plan(&existing, 0).expect("room");
        assert_eq!(idx, 2);
        assert_eq!(v.len(), existing.len() + params().len());
        assert_eq!(&v[..existing.len()], &existing[..], "existing parameters keep their indices");
    }

    #[test]
    fn refused_for_local_signatures_and_when_full() {
        assert!(append_plan(&[], crate::gds::ROOT_FLAG_LOCAL).is_none());
        // 28 root descriptors cost 56 DWORDs; 8 more would pass the limit of 64
        let full: Vec<u8> = (0..28).flat_map(|i| root_param(PARAM_UAV, i)).collect();
        assert_eq!(crate::gds::root_cost(&full), 56);
        assert!(append_plan(&full, 0).is_some(), "56 + 8 = 64 still fits");
        let over: Vec<u8> = (0..29).flat_map(|i| root_param(PARAM_UAV, i)).collect();
        assert!(append_plan(&over, 0).is_none(), "58 + 8 > 64");
    }

    #[test]
    fn windows_are_the_largest_buffers_one_per_resource_ordered_by_address() {
        let b = [buf(0x9000, 0x4000, 1), buf(0x1000, 0x100, 2), buf(0x5000, 0x2000, 3), buf(0x7000, 0x2000, 3)];
        let w = choose_windows(&b, &[]);
        assert_eq!(w.iter().map(|e| e.res).collect::<Vec<_>>(), vec![3, 1], "two windows, ordered by start: res 3 @0x5000, res 1 @0x9000");
        assert_eq!(w[0].start, 0x5000);
    }

    #[test]
    fn a_faulting_address_pulls_its_buffer_into_a_window() {
        let b = [buf(0x10_0000, 0x8000, 1), buf(0x20_0000, 0x4000, 2), buf(0x30_0000, 0x100, 3)];
        // without a hint the two largest win; a fault inside the small buffer replaces the smaller of them
        assert_eq!(choose_windows(&b, &[]).iter().map(|e| e.res).collect::<Vec<_>>(), vec![1, 2]);
        let w = choose_windows(&b, &[0x30_0040]);
        assert_eq!(w.iter().map(|e| e.res).collect::<Vec<_>>(), vec![1, 3], "the hinted buffer takes a window, the largest keeps the other; ordered by address");
        // a hint already covered adds nothing
        assert_eq!(choose_windows(&b, &[0x10_0010]).iter().map(|e| e.res).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn buffers_that_cannot_be_windows_are_skipped() {
        let huge = buf(0, u32::MAX as u64 + 1, 1);
        let tiny = buf(0x100, 2, 2);
        let tex = Entry { start: 0x4000, len: 0x1000, res: 3, buffer: false };
        let ok = buf(0x8000, 16, 4);
        let w = choose_windows(&[huge, tiny, tex, ok], &[]);
        assert_eq!(w.iter().map(|e| e.res).collect::<Vec<_>>(), vec![4], "32-bit window size, 4-byte minimum, buffers only");
    }

    #[test]
    fn window_table_is_what_the_shader_reads() {
        let w = to_windows(&[buf(0x7f10_0000_0000, 0x100, 1)]);
        let cb = window_cb(&w, [0, 0]);
        assert_eq!(&cb[..4], &[0, 0x7f10, 0x100, 0], "base lo/hi, size");
        assert_eq!(&cb[4..8], &[0, 0, 0, 0], "an unused window has size 0, which the shader skips");
    }

    #[test]
    fn faults_are_described_with_the_windows_they_missed() {
        let s = describe_fault(&Fault { count: 3, first_va: 0xdead_0000 }, &[Window { base: 0x1000, size: 0x40 }]);
        assert!(s.contains("3 global-memory access") && s.contains("0xdead0000") && s.contains("0x1000+0x40"), "{s}");
    }

    #[test]
    fn the_binder_can_be_turned_off_and_is_on_by_default() {
        assert!(!disabled_by(None));
        assert!(!disabled_by(Some("1")));
        assert!(disabled_by(Some("0")));
        assert!(disabled_by(Some(" 0 ")));
    }

    #[test]
    fn a_released_signature_does_not_keep_its_index() {
        let rs = 0x6700_0001usize as *mut c_void;
        assert_eq!(root_index(rs), None);
        note_root_signature(rs, 5);
        assert_eq!(root_index(rs), Some(5));
        forget(rs);
        assert_eq!(root_index(rs), None);
    }
}
