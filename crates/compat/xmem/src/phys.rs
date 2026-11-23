use core::ffi::c_void;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use crate::sys::*;
use compat_common::record;

#[path = "phys_runs.rs"]
mod runs;

const M: &str = "xmem";
pub use compat_common::physical_pages::PAGE_BYTES as PAGE;

const MEM_PRIVATE: u32 = 0x2_0000;
pub(crate) const MEM_RESERVE_PLACEHOLDER: u32 = 0x4_0000;
pub(crate) const MEM_REPLACE_PLACEHOLDER: u32 = 0x4000;
pub(crate) const MEM_PRESERVE_PLACEHOLDER: u32 = 0x2;
const MEM_COALESCE_PLACEHOLDERS: u32 = 0x1;
const SEC_RESERVE: u32 = 0x400_0000;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const MEM_DECOMMIT: u32 = 0x4000;
const ERROR_INVALID_ADDRESS: u32 = 487;

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileMappingW(file: usize, attrs: *const c_void, protect: u32, hi: u32, lo: u32, name: *const u16) -> usize;
    fn CloseHandle(h: usize) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> usize;
    fn Thread32First(snap: usize, entry: *mut ThreadEntry) -> i32;
    fn Thread32Next(snap: usize, entry: *mut ThreadEntry) -> i32;
    fn OpenThread(access: u32, inherit: i32, tid: u32) -> usize;
    fn SuspendThread(h: usize) -> u32;
    fn ResumeThread(h: usize) -> u32;
    fn GetThreadContext(h: usize, ctx: *mut u8) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn GetCurrentProcessId() -> u32;
}

#[repr(C)]
#[derive(Default)]
struct ThreadEntry {
    size: u32,
    usage: u32,
    tid: u32,
    pid: u32,
    base_pri: i32,
    delta_pri: i32,
    flags: u32,
}

#[repr(C, align(16))]
struct Context([u8; 0x4d0]);

struct Frozen {
    handles: Vec<usize>,
    suspended: Vec<bool>,
    _ctx: Box<Context>,
}

/// `TOMBOX_XMEM_FREEZE_TRACE=1`: report every freeze and thaw through OutputDebugString.
/// Written from a stack buffer on purpose: while the world is frozen the thread that
/// owns the process heap lock may be one of the suspended ones, so anything that
/// allocates (the normal logger, `format!`) can deadlock right where it is needed.
fn freeze_trace(what: &str, a: usize, b: usize) {
    static ON: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
    let mut on = ON.load(core::sync::atomic::Ordering::Relaxed);
    if on == 0 {
        on = if crate::sys::env_trimmed_is(b"TOMBOX_XMEM_FREEZE_TRACE\0", b"1") == Some(true) { 2 } else { 1 };
        ON.store(on, core::sync::atomic::Ordering::Relaxed);
    }
    if on != 2 {
        return;
    }
    use core::fmt::Write;
    struct Buf([u8; 200], usize);
    impl Write for Buf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let n = s.len().min(self.0.len() - 1 - self.1);
            self.0[self.1..self.1 + n].copy_from_slice(&s.as_bytes()[..n]);
            self.1 += n;
            Ok(())
        }
    }
    let mut buf = Buf([0u8; 200], 0);
    let _ = write!(buf, "[xmem freeze] tid={} {what} {a} {b}", unsafe { GetCurrentThreadId() });
    unsafe { OutputDebugStringA(buf.0.as_ptr()) };
}

thread_local! {
    static FREEZES: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// How many times this thread has suspended every other thread (see `Frozen`).
pub fn freezes_on_this_thread() -> usize {
    FREEZES.with(|c| c.get())
}

impl Frozen {
    fn new() -> Self {
        let mut f = Self::prepare();
        f.freeze();
        f
    }

    /// Everything that needs the heap: the thread list, the handles, the buffers. Nothing is suspended yet.
    ///
    /// All of it has to be done, and every temporary dropped, before the first `SuspendThread`: once the world
    /// is frozen a thread that owns the process heap lock may be one of the frozen ones, so a single `HeapAlloc`
    /// or `HeapFree` by the freezing thread never returns. The title hit exactly that: every other thread
    /// suspended and this one parked in RtlFreeHeap directly under `Frozen::new`, which freed its thread-id
    /// list when the function returned.
    fn prepare() -> Self {
        FREEZES.with(|c| c.set(c.get() + 1));
        freeze_trace("new: enter", 0, 0);
        let mut ids = Vec::with_capacity(256);
        unsafe {
            let snap = CreateToolhelp32Snapshot(4, 0);
            if snap != usize::MAX {
                let (me, pid) = (GetCurrentThreadId(), GetCurrentProcessId());
                let mut e = ThreadEntry { size: core::mem::size_of::<ThreadEntry>() as u32, ..Default::default() };
                let mut ok = Thread32First(snap, &mut e);
                while ok != 0 {
                    if e.pid == pid && e.tid != me {
                        ids.push(e.tid);
                    }
                    ok = Thread32Next(snap, &mut e);
                }
                CloseHandle(snap);
            }
            freeze_trace("new: threads listed", ids.len(), 0);
            let handles: Vec<usize> = ids.iter().map(|&t| OpenThread(0x2 | 0x8, 0, t)).filter(|&h| h != 0).collect();
            drop(ids);
            Frozen { suspended: vec![false; handles.len()], handles, _ctx: Box::new(Context([0u8; 0x4d0])) }
        }
    }

    /// Suspend every other thread. Must not allocate or free: see `prepare`.
    fn freeze(&mut self) {
        let ctx = self._ctx.0.as_mut_ptr();
        freeze_trace("new: suspending", self.handles.len(), 0);
        #[cfg(test)]
        frozen_guard::enter();
        for i in 0..self.handles.len() {
            unsafe {
                if SuspendThread(self.handles[i]) != u32::MAX {
                    self.suspended[i] = true;
                    core::ptr::write_unaligned(ctx.add(0x30) as *mut u32, 0x10_0001);
                    GetThreadContext(self.handles[i], ctx);
                }
            }
        }
        freeze_trace("new: frozen", self.suspended.iter().filter(|s| **s).count(), self.handles.len());
    }
}

impl Drop for Frozen {
    fn drop(&mut self) {
        freeze_trace("drop: thawing", self.suspended.iter().filter(|s| **s).count(), self.handles.len());
        for i in 0..self.handles.len() {
            unsafe {
                if self.suspended[i] {
                    ResumeThread(self.handles[i]);
                }
                CloseHandle(self.handles[i]);
            }
        }
        #[cfg(test)]
        frozen_guard::leave();
    }
}

#[link(name = "kernelbase", kind = "raw-dylib")]
extern "system" {
    fn VirtualAlloc2(process: Handle, base: *mut c_void, size: usize, kind: u32, protect: u32, params: *mut c_void, count: u32) -> *mut c_void;
    fn MapViewOfFile3(file: usize, process: Handle, base: *mut c_void, offset: u64, size: usize, kind: u32, protect: u32, params: *mut c_void, count: u32) -> *mut c_void;
    fn UnmapViewOfFile2(process: Handle, base: *mut c_void, flags: u32) -> i32;
}

struct Section {
    handle: usize,
    live: usize,
}

struct Region {
    size: usize,
    protect: u32,
    split: bool,
}

struct State {
    next_pfn: u64,
    next_section: u64,
    sections: HashMap<u64, Section>,
    pfns: HashMap<u64, (u64, u64)>,
    views: BTreeMap<usize, u64>,
    private: BTreeSet<usize>,
    regions: BTreeMap<usize, Region>,
}

static S: Mutex<Option<State>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut g = S.lock().unwrap_or_else(|e| e.into_inner());
    let s = g.get_or_insert_with(|| State {
        next_pfn: compat_common::physical_pages::FIRST_TITLE_PAGE,
        next_section: 1,
        sections: HashMap::new(),
        pfns: HashMap::new(),
        views: BTreeMap::new(),
        private: BTreeSet::new(),
        regions: BTreeMap::new(),
    });
    f(s)
}

pub fn serialized<R>(f: impl FnOnce() -> R) -> R {
    with(|_| f())
}

pub fn live_bytes() -> u64 {
    with(|s| s.pfns.len() as u64 * PAGE as u64)
}

pub fn owned_extent(va: usize) -> Option<(usize, usize)> {
    with(|s| region_of(s, va).map(|b| (b, b + s.regions[&b].size)))
}

#[cfg(test)]
static MAP_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub fn map_test_guard() -> std::sync::MutexGuard<'static, ()> {
    let g = MAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    runs::reset_breaker();
    g
}

#[cfg(test)]
pub fn released(va: usize) -> Result<(), String> {
    with(|s| {
        let m = query(va).ok_or_else(|| format!("{va:#x}: VirtualQuery failed"))?;
        let placeholder = m.state == MEM_RESERVE && m.kind == 0x2_0000 && m.alloc_protect == PAGE_NOACCESS;
        if m.state == 0x1_0000 || !placeholder || region_of(s, va).is_some() {
            Ok(())
        } else {
            Err(format!("{va:#x}: placeholder left behind (base {:#x} alloc_base {:#x} size {:#x}) and no live region owns it", m.base, m.alloc_base, m.region_size))
        }
    })
}

#[cfg(test)]
pub fn counted_bytes_of(pfns: &[u64]) -> u64 {
    with(|s| pfns.iter().filter(|p| s.pfns.contains_key(p)).count() as u64 * PAGE as u64)
}

fn query(addr: usize) -> Option<MemoryBasicInformation> {
    let mut m = MemoryBasicInformation::default();
    let n = unsafe { VirtualQueryEx(GetCurrentProcess(), addr as *const c_void, &mut m, core::mem::size_of::<MemoryBasicInformation>()) };
    if n == 0 { None } else { Some(m) }
}

pub fn allocate(n: usize) -> Option<Vec<u64>> {
    if n == 0 {
        return Some(Vec::new());
    }
    let bytes = (n as u64).checked_mul(PAGE as u64)?;
    // PFNs name offsets in one shared section. Reserve the backing here and
    // commit it when a view first makes those offsets accessible to the title.
    let h = unsafe { CreateFileMappingW(usize::MAX, core::ptr::null(), PAGE_EXECUTE_READWRITE | SEC_RESERVE, (bytes >> 32) as u32, bytes as u32, core::ptr::null()) };
    if h == 0 {
        return None;
    }
    Some(with(|s| {
        let sid = s.next_section;
        s.next_section += 1;
        s.sections.insert(sid, Section { handle: h, live: n });
        let base = s.next_pfn;
        s.next_pfn += n as u64;
        (0..n as u64).map(|i| {
            s.pfns.insert(base + i, (sid, i * PAGE as u64));
            base + i
        }).collect()
    }))
}

pub fn free(pfns: &[u64]) -> bool {
    with(|s| {
        let mut seen = std::collections::HashSet::with_capacity(pfns.len());
        if pfns.iter().any(|p| !s.pfns.contains_key(p) || !seen.insert(*p)) {
            return false;
        }
        for p in pfns {
            if let Some((sid, _)) = s.pfns.remove(p) {
                let gone = match s.sections.get_mut(&sid) {
                    Some(sec) => {
                        sec.live -= 1;
                        sec.live == 0
                    }
                    None => false,
                };
                if gone {
                    if let Some(sec) = s.sections.remove(&sid) {
                        unsafe { CloseHandle(sec.handle) };
                    }
                }
            }
        }
        true
    })
}

fn region_of(s: &State, va: usize) -> Option<usize> {
    s.regions.range(..=va).next_back().filter(|(b, r)| va < **b + r.size).map(|(b, _)| *b)
}

unsafe fn split(base: usize, size: usize) -> bool {
    let mut p = base;
    while p + PAGE < base + size {
        if VirtualFree(p as *mut c_void, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) == 0 {
            return false;
        }
        p += PAGE;
    }
    true
}

unsafe fn coalesce(base: usize, size: usize) {
    VirtualFree(base as *mut c_void, size, MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS);
}

unsafe fn adopt(s: &mut State, va: usize) -> Option<usize> {
    if let Some(b) = region_of(s, va) {
        return Some(b);
    }
    let m = query(va)?;
    if m.state != MEM_RESERVE || m.kind != MEM_PRIVATE {
        return None;
    }
    let base = m.alloc_base;
    let mut size = 0usize;
    let mut p = base;
    loop {
        let r = match query(p) {
            Some(r) if r.alloc_base == base && r.state != 0x1_0000 => r,
            _ => break,
        };
        if r.state != MEM_RESERVE {
            return None;
        }
        size += r.region_size;
        p = r.base + r.region_size;
    }
    let protect = match m.alloc_protect & 0xff {
        0 | 0x01 => PAGE_READWRITE,
        v => v,
    };
    let (got, err) = {
        let _world = Frozen::new();
        if VirtualFree(base as *mut c_void, 0, MEM_RELEASE) == 0 {
            return None;
        }
        let got = VirtualAlloc2(GetCurrentProcess(), base as *mut c_void, size, MEM_RESERVE | MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, core::ptr::null_mut(), 0);
        let err = GetLastError();
        if got as usize != base {
            if !got.is_null() {
                VirtualFree(got, 0, MEM_RELEASE);
            }
            VirtualAlloc(base as *mut c_void, size, MEM_RESERVE, m.alloc_protect.max(PAGE_NOACCESS));
        }
        (got, err)
    };
    if got as usize != base {
        record(M, "XMemMapPhysicalPages", &format!("could not turn reservation {base:#x}+{size:#x} into a placeholder (err={err})"));
        return None;
    }
    s.regions.insert(base, Region { size, protect, split: false });
    crate::hooks::install_vfree_hook();
    Some(base)
}

fn idle(s: &State, base: usize, size: usize) -> bool {
    !s.views.range(base..base + size).any(|_| true) && !s.private.range(base..base + size).any(|_| true)
}

unsafe fn ensure_split(s: &mut State, base: usize) -> bool {
    let (size, was) = (s.regions[&base].size, s.regions[&base].split);
    if was {
        return true;
    }
    if !split(base, size) {
        return false;
    }
    if let Some(r) = s.regions.get_mut(&base) {
        r.split = true;
    }
    true
}

unsafe fn to_placeholder(s: &mut State, piece: usize) -> bool {
    if s.private.remove(&piece) {
        let len = region_of(s, piece).map(|b| PAGE.min(b + s.regions[&b].size - piece)).unwrap_or(PAGE);
        VirtualFree(piece as *mut c_void, len, MEM_DECOMMIT);
        if VirtualFree(piece as *mut c_void, len, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) == 0 {
            record(M, "XMemMapPhysicalPages", &format!("piece {piece:#x} back to placeholder failed (err={})", GetLastError()));
            return false;
        }
    }
    true
}

unsafe fn unmap_view(s: &mut State, va: usize) -> bool {
    if runs::run_range(va).is_some() && !runs::break_run(s, va) {
        return false;
    }
    if s.views.remove(&va).is_some() {
        return UnmapViewOfFile2(GetCurrentProcess(), va as *mut c_void, MEM_PRESERVE_PLACEHOLDER) != 0;
    }
    true
}

unsafe fn map_committed_view(section: usize, va: usize, offset: u64, size: usize, protect: u32) -> bool {
    let process = GetCurrentProcess();
    let view = MapViewOfFile3(section, process, va as *mut c_void, offset, size, MEM_REPLACE_PLACEHOLDER, protect, core::ptr::null_mut(), 0);
    if view.is_null() {
        return false;
    }
    if !VirtualAlloc(view, size, MEM_COMMIT, protect).is_null() {
        return true;
    }
    let error = GetLastError();
    UnmapViewOfFile2(process, view, MEM_PRESERVE_PLACEHOLDER);
    SetLastError(error);
    false
}

unsafe fn map_page(s: &mut State, va: usize, pfn: Option<u64>) -> bool {
    if try_map_page(s, va, pfn) {
        return true;
    }
    let stale = region_of(s, va).filter(|b| idle(s, *b, s.regions[b].size));
    match stale {
        Some(b) => {
            s.regions.remove(&b);
            try_map_page(s, va, pfn)
        }
        None => false,
    }
}

unsafe fn try_map_page(s: &mut State, va: usize, pfn: Option<u64>) -> bool {
    let base = match adopt(s, va) {
        Some(b) => b,
        None => return false,
    };
    if !unmap_view(s, va) {
        return false;
    }
    let (size, protect, was_split) = {
        let r = &s.regions[&base];
        (r.size, r.protect, r.split)
    };
    let pfn = match pfn {
        Some(p) => p,
        None => {
            if was_split && idle(s, base, size) {
                coalesce(base, size);
                if let Some(r) = s.regions.get_mut(&base) {
                    r.split = false;
                }
            }
            return true;
        }
    };
    let (sid, off) = match s.pfns.get(&pfn) {
        Some(v) => *v,
        None => {
            SetLastError(ERROR_INVALID_PARAMETER);
            return false;
        }
    };
    if !was_split && !ensure_split(s, base) {
        return false;
    }
    if !to_placeholder(s, va) {
        return false;
    }
    let h = s.sections[&sid].handle;
    if !map_committed_view(h, va, off, PAGE, protect) {
        return false;
    }
    s.views.insert(va, pfn);
    true
}

pub fn map(pages: &[(usize, Option<u64>)]) -> bool {
    if let (Some(lo), Some(hi)) = (pages.iter().map(|p| p.0 & !(PAGE - 1)).min(), pages.iter().map(|p| p.0 & !(PAGE - 1)).max()) {
        crate::epoch::bump_range(lo, hi + PAGE);
    }
    let ok = with(|s| {
        let mut ok = true;
        unsafe { runs::release_covered(s, pages) };
        for (i, j) in runs::groups(s, pages) {
            if j - i > 1 && unsafe { runs::try_map_run(s, &pages[i..j]) } {
                continue;
            }
            for &(va, pfn) in &pages[i..j] {
                if !unsafe { map_page(s, va & !(PAGE - 1), pfn) } {
                    ok = false;
                }
            }
        }
        ok
    });
    if let (Some(lo), Some(hi)) = (pages.iter().map(|p| p.0 & !(PAGE - 1)).min(), pages.iter().map(|p| p.0 & !(PAGE - 1)).max()) {
        crate::epoch::bump_range(lo, hi + PAGE);
    }
    for &(va, pfn) in pages {
        if pfn.is_some() {
            crate::write_note::committed(va & !(PAGE - 1), PAGE);
        }
    }
    ok
}

pub fn release_region(addr: usize, size: usize) -> Option<i32> {
    with(|s| {
        let r = s.regions.get(&addr)?;
        if size != 0 {
            unsafe { SetLastError(ERROR_INVALID_PARAMETER) };
            return Some(0);
        }
        let total = r.size;
        unsafe { runs::release_in(s, addr, addr + total) };
        let views: Vec<usize> = s.views.range(addr..addr + total).map(|(v, _)| *v).collect();
        for v in views {
            unsafe { unmap_view(s, v) };
        }
        let pieces: Vec<usize> = s.private.range(addr..addr + total).copied().collect();
        for p in pieces {
            unsafe { to_placeholder(s, p) };
        }
        unsafe {
            coalesce(addr, total);
            s.regions.remove(&addr);
            Some(VirtualFree(addr as *mut c_void, 0, MEM_RELEASE))
        }
    })
}


fn pieces(base: usize, size: usize, addr: usize, len: usize) -> Vec<(usize, usize, usize)> {
    let end = (addr + len).min(base + size);
    let mut out = Vec::new();
    let mut p = (addr.max(base) - base) / PAGE * PAGE + base;
    while p < end {
        let piece_end = (p + PAGE).min(base + size);
        let lo = addr.max(p);
        let hi = end.min(piece_end);
        out.push((p, lo, hi - lo));
        p = piece_end;
    }
    out
}

pub fn commit_owned(addr: usize, size: usize, protect: u32) -> Option<usize> {
    with(|s| {
        let base = region_of(s, addr)?;
        let (rsize, rprot) = (s.regions[&base].size, s.regions[&base].protect);
        let lo = addr & !0xfff;
        let len = ((addr + size.max(1) + 0xfff) & !0xfff) - lo;
        if lo + len > base + rsize {
            unsafe { SetLastError(ERROR_INVALID_ADDRESS) };
            return Some(0);
        }
        let ps = pieces(base, rsize, lo, len);
        if ps.iter().any(|(p, _, _)| s.views.contains_key(p)) {
            unsafe { SetLastError(ERROR_INVALID_ADDRESS) };
            return Some(0);
        }
        unsafe {
            if !ensure_split(s, base) {
                let err = GetLastError();
                let pieces_now: Vec<String> = (0..rsize / PAGE).filter_map(|i| query(base + i * PAGE)).map(|m| format!("[{:#x} ab={:#x} sz={:#x} st={:#x} ty={:#x} ap={:#x}]", m.base, m.alloc_base, m.region_size, m.state, m.kind, m.alloc_protect)).collect();
                commit_failed(&format!("split of region {base:#x}+{rsize:#x} err={err} split_flag={} views={} private={} pieces {}", s.regions[&base].split, s.views.range(base..base + rsize).count(), s.private.range(base..base + rsize).count(), pieces_now.join("")));
                return Some(0);
            }
            for (p, a, n) in ps {
                if !s.private.contains(&p) {
                    let plen = PAGE.min(base + rsize - p);
                    let got = VirtualAlloc2(GetCurrentProcess(), p as *mut c_void, plen, MEM_RESERVE | MEM_REPLACE_PLACEHOLDER, rprot, core::ptr::null_mut(), 0);
                    if got as usize != p {
                        commit_failed(&format!("piece {p:#x}+{plen:#x} replace placeholder -> {got:p} err={} state={:x?}", GetLastError(), query(p).map(|m| (m.base, m.alloc_base, m.region_size, m.state, m.kind, m.alloc_protect))));
                        return Some(0);
                    }
                    s.private.insert(p);
                }
                if VirtualAlloc(a as *mut c_void, n, MEM_COMMIT, protect).is_null() {
                    commit_failed(&format!("commit {a:#x}+{n:#x} err={}", GetLastError()));
                    return Some(0);
                }
            }
        }
        Some(lo)
    })
}

#[cfg(test)]
pub static LAST_COMMIT_FAIL: Mutex<String> = Mutex::new(String::new());

fn commit_failed(why: &str) {
    record(M, "XMemVirtualAlloc", &format!("commit in physical-page region failed: {why}"));
    #[cfg(test)]
    {
        *LAST_COMMIT_FAIL.lock().unwrap_or_else(|e| e.into_inner()) = why.to_string();
    }
}

pub fn decommit_owned(addr: usize, size: usize) -> Option<i32> {
    with(|s| {
        let base = region_of(s, addr)?;
        let rsize = s.regions[&base].size;
        let len = if size == 0 { base + rsize - addr } else { size };
        let lo = addr & !0xfff;
        let hi = (addr + len + 0xfff) & !0xfff;
        for (p, a, n) in pieces(base, rsize, lo, hi - lo) {
            if s.private.contains(&p) && unsafe { VirtualFree(a as *mut c_void, n, MEM_DECOMMIT) } == 0 {
                return Some(0);
            }
        }
        Some(1)
    })
}

/// Reserve address space as a placeholder (`MEM_RESERVE | MEM_RESERVE_PLACEHOLDER`,
/// `PAGE_NOACCESS`). The console kernel shares the Windows 10 placeholder
/// protocol, and a title that asks for a placeholder goes on to split it with
/// `VirtualFree(.., MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER)`, which the PC
/// kernel only accepts on a real placeholder - a plain reservation is refused
/// with ERROR_INVALID_PARAMETER.
pub fn reserve_placeholder(base: usize, len: usize) -> *mut c_void {
    serialized(|| unsafe {
        VirtualAlloc2(GetCurrentProcess(), base as *mut c_void, len, MEM_RESERVE | MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, core::ptr::null_mut(), 0)
    })
}

/// Take ownership of the title's placeholder `[base, base + len)` as it is. The console
/// calls this "replace the placeholder with a reservation"; on the PC the physical
/// pages are mapped straight into a placeholder, so there is nothing to replace.
/// Converting a plain reservation instead (`adopt`) means freeing it and re-reserving
/// it as a placeholder with every other thread suspended, which can deadlock against
/// whichever suspended thread holds the process heap lock (a title thread inside
/// `HeapAlloc`, a debugger-held exception dispatch); the title already hands over the
/// state `adopt` works to create, so none of that is needed.
///
/// The range must be exactly one placeholder (reserved, private, `PAGE_NOACCESS`).
pub fn own_placeholder(base: usize, len: usize, protect: u32) -> Result<(), u32> {
    with(|s| {
        if let Some(b) = region_of(s, base) {
            return if b == base && s.regions[&b].size == len { Ok(()) } else { Err(ERROR_INVALID_ADDRESS) };
        }
        let m = query(base).ok_or(ERROR_INVALID_ADDRESS)?;
        let exact = m.base == base && m.region_size == len;
        if m.state != MEM_RESERVE || m.kind != MEM_PRIVATE || m.alloc_protect != PAGE_NOACCESS || !exact {
            return Err(ERROR_INVALID_PARAMETER);
        }
        let protect = if protect & 0xff == PAGE_NOACCESS { PAGE_READWRITE } else { protect };
        s.regions.insert(base, Region { size: len, protect, split: false });
        crate::hooks::install_vfree_hook();
        Ok(())
    })
}

pub fn region_extent(addr: usize) -> Option<usize> {
    with(|s| s.regions.get(&addr).map(|r| r.size))
}

pub fn owns(addr: usize) -> bool {
    with(|s| region_of(s, addr).is_some())
}

/// Test hooks: when the world is frozen and thawed. `thawed()` is set once every thread has been resumed (the
/// point after which the freezing thread may use the heap again).
#[cfg(test)]
pub(crate) mod frozen_guard {
    use std::sync::atomic::{AtomicBool, Ordering};

    static THAWED: AtomicBool = AtomicBool::new(true);

    pub fn enter() {
        THAWED.store(false, Ordering::SeqCst);
    }

    /// A test that wants to observe the thaw clears the flag first.
    pub fn arm() {
        THAWED.store(false, Ordering::SeqCst);
    }

    pub fn leave() {
        THAWED.store(true, Ordering::SeqCst);
    }

    pub fn thawed() -> bool {
        THAWED.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod freeze_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[link(name = "kernel32")]
    extern "system" {
        fn HeapLock(heap: *mut c_void) -> i32;
        fn HeapUnlock(heap: *mut c_void) -> i32;
    }

    #[test]
    fn a_large_physical_pool_commits_only_mapped_offsets_and_preserves_aliases() {
        let _guard = map_test_guard();
        let count = 99_776;
        let mut n = count;
        let mut pfns = vec![0; count];
        assert_eq!(unsafe { crate::virt::XMemAllocatePhysicalPages(0x380, &mut n, pfns.as_mut_ptr()) }, 1);
        assert_eq!(n, count);
        let last = pfns.last_mut().unwrap();
        let (handle, offset) = with(|s| {
            let (sid, offset) = s.pfns[last];
            (s.sections[&sid].handle, offset)
        });
        unsafe {
            let process = GetCurrentProcess();
            let probe = MapViewOfFile3(handle, process, core::ptr::null_mut(), offset, PAGE, 0, PAGE_READWRITE, core::ptr::null_mut(), 0);
            assert!(!probe.is_null());
            let state = query(probe as usize).unwrap().state;
            assert_ne!(UnmapViewOfFile2(process, probe, 0), 0);
            assert_eq!(state, MEM_RESERVE);

            let a = reserve_placeholder(0, PAGE);
            let b = reserve_placeholder(0, PAGE);
            assert!(!a.is_null() && !b.is_null());
            assert_eq!(crate::virt::XMemMapPhysicalPages(a, 1, last), a);
            assert_eq!(query(a as usize).unwrap().state, MEM_COMMIT);
            a.cast::<u32>().write(0xabcd_ef01);
            assert_eq!(crate::virt::XMemMapPhysicalPages(b, 1, last), b);
            assert_eq!(b.cast::<u32>().read(), 0xabcd_ef01);
            assert_eq!(release_region(a as usize, 0), Some(1));
            assert_eq!(release_region(b as usize, 0), Some(1));
            assert_eq!(crate::virt::XMemFreePhysicalPages(count, pfns.as_mut_ptr()), 1);
        }
    }

    /// Nothing may allocate or free while the world is frozen: a suspended thread may own the lock of the
    /// heap the emulator DLLs allocate from (`SuspendThread` does not release it), and the freezing thread
    /// would wait for it forever. The title hit this as a hang with 71 threads suspended and the freezing
    /// thread parked in RtlFreeHeap directly under `Frozen::new`, which freed its thread-id list at the end
    /// of the function.
    ///
    /// Here a helper thread takes the heap's lock and keeps it until the world has been thawed, so the helper
    /// is suspended while it owns the lock. Any heap use by the freezing thread in between never returns.
    /// A regression therefore hangs this test (it suspends every thread of the process, the harness too)
    /// instead of failing an assertion.
    #[test]
    fn nothing_touches_the_heap_while_the_world_is_frozen() {
        let _guard = map_test_guard();
        let heap = compat_common::private_heap::handle() as usize;
        for round in 0..20 {
            frozen_guard::arm();
            let (go, locked) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
            let (g, l, h) = (go.clone(), locked.clone(), heap);
            // the helper must exist before `prepare` lists the threads, but must not hold the lock yet
            let helper = std::thread::spawn(move || unsafe {
                while !g.load(Ordering::SeqCst) {
                    core::hint::spin_loop();
                }
                HeapLock(h as *mut c_void);
                l.store(true, Ordering::SeqCst);
                // keep the lock until the freezing thread has thawed the world (no heap use here: it is locked)
                while !frozen_guard::thawed() {
                    core::hint::spin_loop();
                }
                HeapUnlock(h as *mut c_void);
            });
            // everything that needs the heap is done before the helper takes its lock
            let mut w = Frozen::prepare();
            go.store(true, Ordering::SeqCst);
            while !locked.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            // the helper owns the heap lock from here until it sees the thaw; it is suspended while it does
            w.freeze();
            assert!(!frozen_guard::thawed(), "round {round}: the world is frozen");
            drop(w);
            assert!(frozen_guard::thawed(), "round {round}: Drop thaws before it frees anything");
            helper.join().unwrap();
        }
    }
}
