//! Fixes the Xbox↔PC return-convention mismatch for the descriptor-handle getters
//! **without wrapping the heap object** (a wrapper would leak into real D3D12 calls
//! like `SetDescriptorHeaps`, crashing the driver).
//!
//! Instead we patch the real `ID3D12DescriptorHeap` vtable in place: slots 9/10
//! (`GetCPU/GPUDescriptorHandleForHeapStart`) are redirected to small bridges.
//! All descriptor heaps share one vtable, so patching once (on the first heap)
//! fixes them all, and the heap objects stay fully real.
//!
//! Xbox callers expect the 8-byte handle in RAX. PC `D3D12Core` implements these
//! as `fn(this, Handle* retval) -> Handle*` (writes *retval). Each bridge calls the
//! saved original with a local and returns the value in RAX.

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use compat_common::record;

extern "system" {
    fn VirtualProtect(addr: *mut c_void, size: usize, new_protect: u32, old_protect: *mut u32) -> i32;
}
const PAGE_READWRITE: u32 = 0x04;

// All descriptor-heap vtables share the same GetCPU/GPU impl in D3D12Core, so one
// saved original serves every patched vtable. Different heap TYPES can have
// different vtable objects, though, so we patch each distinct one.
static ORIG_CPU: AtomicUsize = AtomicUsize::new(0);
static ORIG_GPU: AtomicUsize = AtomicUsize::new(0);
static PATCHED_VTABLES: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Heaps whose descriptors live in memory the title supplied (see `title_heap`):
/// on the console such a heap's CPU start *is* that memory, and the title checks
/// it. Value is `(cpu_start, gpu_start)` as the title must see them.
static TITLE_HANDLES: Mutex<Option<std::collections::HashMap<usize, (u64, u64)>>> = Mutex::new(None);

pub fn bind_title_handles(heap: usize, cpu: u64, gpu: u64) {
    TITLE_HANDLES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(heap, (cpu, gpu));
}

/// A new heap can reuse the address of a released one; drop any stale binding.
pub fn unbind_title_handles(heap: usize) -> bool {
    TITLE_HANDLES.lock().unwrap_or_else(|e| e.into_inner()).as_mut().map_or(false, |m| m.remove(&heap).is_some())
}

pub fn title_handles(heap: usize) -> Option<(u64, u64)> {
    TITLE_HANDLES.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&heap).copied())
}

unsafe extern "system" fn bridge_cpu(heap: *mut c_void) -> u64 {
    if let Some((cpu, _)) = title_handles(heap as usize) {
        return cpu;
    }
    let orig: unsafe extern "system" fn(*mut c_void, *mut u64) -> *mut u64 =
        unsafe { core::mem::transmute(ORIG_CPU.load(Ordering::Relaxed)) };
    let mut out: u64 = 0;
    unsafe { orig(heap, &mut out) };
    static N: AtomicUsize = AtomicUsize::new(0);
    if N.fetch_add(1, Ordering::Relaxed) < 6 {
        record("d3d12_xs", "heap.GetCPUHandleBase", &format!("heap={heap:p} base={out:#x}"));
    }
    out
}

unsafe extern "system" fn bridge_gpu(heap: *mut c_void) -> u64 {
    if let Some((_, gpu)) = title_handles(heap as usize) {
        return gpu;
    }
    let orig: unsafe extern "system" fn(*mut c_void, *mut u64) -> *mut u64 =
        unsafe { core::mem::transmute(ORIG_GPU.load(Ordering::Relaxed)) };
    let mut out: u64 = 0;
    unsafe { orig(heap, &mut out) };
    static N: AtomicUsize = AtomicUsize::new(0);
    if N.fetch_add(1, Ordering::Relaxed) < 6 {
        record("d3d12_xs", "heap.GetGPUHandleBase", &format!("heap={heap:p} base={out:#x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_bound_heap_reports_the_title_handles_and_other_heaps_do_not() {
        let heap = 0x7e57_0001usize;
        assert_eq!(title_handles(heap), None);
        bind_title_handles(heap, 0x2_0000_0000, 0x2_0000_0010);
        assert_eq!(title_handles(heap), Some((0x2_0000_0000, 0x2_0000_0010)));
        assert_eq!(title_handles(heap + 1), None, "only the bound heap");
        assert!(unbind_title_handles(heap));
        assert_eq!(title_handles(heap), None);
        assert!(!unbind_title_handles(heap), "already gone");
    }
}

/// Patch this heap's descriptor-heap vtable (slots 9/10) if not already done.
/// Called for every created heap so each distinct heap-type vtable gets patched.
pub fn ensure_patched(real_heap: *mut c_void) {
    if real_heap.is_null() {
        return;
    }
    let vtbl = unsafe { *(real_heap as *const *mut usize) };
    let vtbl_key = vtbl as usize;
    let mut patched = PATCHED_VTABLES.lock().unwrap();
    if patched.contains(&vtbl_key) {
        return;
    }
    unsafe {
        let cur_cpu = *vtbl.add(9);
        let cur_gpu = *vtbl.add(10);
        // Don't re-save our own bridges if somehow already pointing at them.
        if cur_cpu != bridge_cpu as usize {
            ORIG_CPU.store(cur_cpu, Ordering::Relaxed);
        }
        if cur_gpu != bridge_gpu as usize {
            ORIG_GPU.store(cur_gpu, Ordering::Relaxed);
        }
        let addr = vtbl.add(9) as *mut c_void;
        let mut old: u32 = 0;
        if VirtualProtect(addr, 16, PAGE_READWRITE, &mut old) != 0 {
            *vtbl.add(9) = bridge_cpu as usize;
            *vtbl.add(10) = bridge_gpu as usize;
            let mut tmp: u32 = 0;
            VirtualProtect(addr, 16, old, &mut tmp);
            patched.push(vtbl_key);
            record("d3d12_xs", "heap.patch", &format!("vtable {vtbl:p} slots 9/10 bridged ({} total)", patched.len()));
        } else {
            record("d3d12_xs", "heap.patch", "VirtualProtect failed");
        }
    }
}
