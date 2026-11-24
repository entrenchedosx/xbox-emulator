//! Diagnostic bisection of the commands a title records: `TOMBOX_DIAG_SKIP=draw,dispatch,clear,copy`
//! drops the named command classes before they reach the host driver, so that a device removal can be
//! attributed to a class of command rather than to the last breadcrumb DRED happened to see.
//!
//! This is a diagnostic, not a renderer: nothing here is on by default, every skipped command is
//! counted and logged, and a run with skips never counts as a working frame.

use compat_common::record;
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

const M: &str = "d3d12_xs";

#[derive(Clone, Copy, Default)]
struct Skips {
    draw: bool,
    dispatch: bool,
    clear: bool,
    copy: bool,
    indirect: bool,
    barrier: bool,
    resolve: bool,
    execute: bool,
    lists: bool,
}

fn skips() -> &'static Skips {
    static S: OnceLock<Skips> = OnceLock::new();
    S.get_or_init(|| {
        let v = std::env::var("TOMBOX_DIAG_SKIP").unwrap_or_default();
        let has = |w: &str| v.split(',').any(|x| x.trim().eq_ignore_ascii_case(w));
        Skips { draw: has("draw"), dispatch: has("dispatch"), clear: has("clear"), copy: has("copy"), indirect: has("indirect"), barrier: has("barrier"), resolve: has("resolve"), execute: has("execute"), lists: has("lists") }
    })
}

static NEXT: [AtomicUsize; 64] = [const { AtomicUsize::new(0) }; 64];
static SKIPPED: [AtomicUsize; 9] = [const { AtomicUsize::new(0) }; 9];

/// `TOMBOX_DIAG_SKIP=lists`: everything around a submission still runs (uploads, shadow merges, descriptor
/// mirroring) but the real `ExecuteCommandLists` of the title's lists is not issued.
pub fn skip_lists() -> bool {
    if skips().lists {
        note(8, "title list ExecuteCommandLists to the host driver");
        return true;
    }
    false
}

/// `TOMBOX_DIAG_SKIP=execute`: the title's command lists are not submitted at all (the emulator's own uploads,
/// copies and presents still are), to tell a hang caused by the title's work from one caused by the emulator's.
pub fn skip_execute() -> bool {
    if skips().execute {
        note(7, "title ExecuteCommandLists submission");
        return true;
    }
    false
}

fn note(class: usize, what: &str) {
    let n = SKIPPED[class].fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n.is_power_of_two() && n <= 4096 {
        record(M, "diag_skip", &format!("DIAGNOSTIC: {what} #{n} dropped before the host driver (TOMBOX_DIAG_SKIP); this run is not a rendering result"));
    }
}

macro_rules! hook {
    ($name:ident, $slot:literal, $class:literal, $label:literal, $flag:ident) => {
        unsafe extern "system" fn $name(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) {
            if skips().$flag {
                note($class, $label);
                return;
            }
            let next: unsafe extern "system" fn(*mut c_void, u64, u64, u64, u64, u64, u64, u64) = unsafe { core::mem::transmute(NEXT[$slot].load(Ordering::Relaxed)) };
            unsafe { next(this, a, b, c, d, e, f, g) }
        }
    };
}

hook!(h_draw, 12, 0, "DrawInstanced", draw);
hook!(h_draw_indexed, 13, 0, "DrawIndexedInstanced", draw);
hook!(h_dispatch, 14, 1, "Dispatch", dispatch);
hook!(h_clear_dsv, 51, 2, "ClearDepthStencilView", clear);
hook!(h_clear_rtv, 52, 2, "ClearRenderTargetView", clear);
hook!(h_copy_buffer, 15, 3, "CopyBufferRegion", copy);
hook!(h_copy_texture, 16, 3, "CopyTextureRegion", copy);
hook!(h_copy_resource, 17, 3, "CopyResource", copy);
hook!(h_indirect, 59, 4, "ExecuteIndirect", indirect);
hook!(h_barrier, 26, 5, "ResourceBarrier", barrier);
hook!(h_resolve, 19, 6, "ResolveSubresource", resolve);

pub fn install() {
    let s = skips();
    if !(s.draw || s.dispatch || s.clear || s.copy || s.indirect || s.barrier || s.resolve || s.execute || s.lists) {
        return;
    }
    let hooks: [(usize, usize); 11] = [
        (12, h_draw as usize), (13, h_draw_indexed as usize), (14, h_dispatch as usize), (51, h_clear_dsv as usize),
        (52, h_clear_rtv as usize), (15, h_copy_buffer as usize), (16, h_copy_texture as usize), (17, h_copy_resource as usize),
        (59, h_indirect as usize), (26, h_barrier as usize), (19, h_resolve as usize),
    ];
    for (slot, f) in hooks {
        let prev = crate::cmdlist_wrap::vt_get(slot);
        if prev == 0 {
            continue;
        }
        NEXT[slot].store(prev, Ordering::Relaxed);
        crate::cmdlist_wrap::vt_set(slot, f);
    }
    record(M, "diag_skip", &format!("DIAGNOSTIC skips on: draw={} dispatch={} clear={} copy={} indirect={} barrier={} resolve={}", s.draw, s.dispatch, s.clear, s.copy, s.indirect, s.barrier, s.resolve));
}


// ---- per-slot skipping: TOMBOX_DIAG_SKIP_SLOTS=18,27,31,50-58 drops those Xbox command-list vtable slots ----

static SLOT_SKIP: [core::sync::atomic::AtomicBool; 128] = [const { core::sync::atomic::AtomicBool::new(false) }; 128];
static SLOT_NEXT: [AtomicUsize; 128] = [const { AtomicUsize::new(0) }; 128];
static SLOT_HITS: [AtomicUsize; 128] = [const { AtomicUsize::new(0) }; 128];

#[inline(always)]
unsafe fn slot_call(slot: usize, this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) {
    if SLOT_SKIP[slot].load(Ordering::Relaxed) {
        let n = SLOT_HITS[slot].fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 {
            record(M, "diag_skip", &format!("DIAGNOSTIC: command-list slot {slot} dropped before the host driver (TOMBOX_DIAG_SKIP_SLOTS); this run is not a rendering result"));
        }
        return;
    }
    let next: unsafe extern "system" fn(*mut c_void, u64, u64, u64, u64, u64, u64, u64) = unsafe { core::mem::transmute(SLOT_NEXT[slot].load(Ordering::Relaxed)) };
    unsafe { next(this, a, b, c, d, e, f, g) }
}

unsafe extern "system" fn sk11(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(11, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk12(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(12, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk13(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(13, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk14(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(14, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk15(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(15, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk16(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(16, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk17(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(17, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk18(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(18, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk19(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(19, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk20(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(20, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk21(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(21, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk22(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(22, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk23(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(23, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk24(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(24, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk25(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(25, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk26(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(26, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk27(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(27, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk28(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(28, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk29(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(29, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk30(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(30, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk31(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(31, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk32(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(32, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk33(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(33, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk34(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(34, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk35(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(35, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk36(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(36, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk37(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(37, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk38(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(38, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk39(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(39, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk40(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(40, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk41(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(41, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk42(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(42, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk43(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(43, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk44(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(44, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk45(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(45, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk46(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(46, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk47(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(47, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk48(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(48, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk49(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(49, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk50(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(50, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk51(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(51, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk52(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(52, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk53(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(53, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk54(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(54, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk55(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(55, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk56(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(56, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk57(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(57, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk58(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(58, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk59(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(59, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk60(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(60, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk61(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(61, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk62(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(62, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk63(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(63, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk64(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(64, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk65(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(65, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk66(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(66, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk67(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(67, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk68(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(68, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk69(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(69, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk70(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(70, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk71(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(71, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk72(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(72, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk73(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(73, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk74(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(74, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk75(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(75, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk76(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(76, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk77(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(77, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk78(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(78, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk79(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(79, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk80(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(80, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk81(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(81, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk82(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(82, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk83(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(83, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk84(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(84, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk85(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(85, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk86(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(86, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk87(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(87, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk88(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(88, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk89(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(89, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk90(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(90, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk91(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(91, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk92(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(92, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk93(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(93, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk94(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(94, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk95(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(95, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk96(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(96, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk97(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(97, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk98(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(98, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk99(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(99, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk100(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(100, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk101(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(101, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk102(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(102, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk103(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(103, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk104(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(104, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk105(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(105, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk106(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(106, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk107(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(107, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk108(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(108, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk109(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(109, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk110(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(110, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk111(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(111, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk112(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(112, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk113(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(113, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk114(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(114, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk115(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(115, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk116(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(116, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk117(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(117, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk118(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(118, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk119(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(119, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk120(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(120, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk121(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(121, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk122(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(122, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk123(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(123, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk124(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(124, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk125(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(125, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk126(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(126, this, a, b, c, d, e, f, g) }
unsafe extern "system" fn sk127(this: *mut c_void, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) { slot_call(127, this, a, b, c, d, e, f, g) }

fn slot_fns() -> [usize; 117] {
    [sk11 as usize, sk12 as usize, sk13 as usize, sk14 as usize, sk15 as usize, sk16 as usize, sk17 as usize, sk18 as usize, sk19 as usize, sk20 as usize, sk21 as usize, sk22 as usize, sk23 as usize, sk24 as usize, sk25 as usize, sk26 as usize, sk27 as usize, sk28 as usize, sk29 as usize, sk30 as usize, sk31 as usize, sk32 as usize, sk33 as usize, sk34 as usize, sk35 as usize, sk36 as usize, sk37 as usize, sk38 as usize, sk39 as usize, sk40 as usize, sk41 as usize, sk42 as usize, sk43 as usize, sk44 as usize, sk45 as usize, sk46 as usize, sk47 as usize, sk48 as usize, sk49 as usize, sk50 as usize, sk51 as usize, sk52 as usize, sk53 as usize, sk54 as usize, sk55 as usize, sk56 as usize, sk57 as usize, sk58 as usize, sk59 as usize, sk60 as usize, sk61 as usize, sk62 as usize, sk63 as usize, sk64 as usize, sk65 as usize, sk66 as usize, sk67 as usize, sk68 as usize, sk69 as usize, sk70 as usize, sk71 as usize, sk72 as usize, sk73 as usize, sk74 as usize, sk75 as usize, sk76 as usize, sk77 as usize, sk78 as usize, sk79 as usize, sk80 as usize, sk81 as usize, sk82 as usize, sk83 as usize, sk84 as usize, sk85 as usize, sk86 as usize, sk87 as usize, sk88 as usize, sk89 as usize, sk90 as usize, sk91 as usize, sk92 as usize, sk93 as usize, sk94 as usize, sk95 as usize, sk96 as usize, sk97 as usize, sk98 as usize, sk99 as usize, sk100 as usize, sk101 as usize, sk102 as usize, sk103 as usize, sk104 as usize, sk105 as usize, sk106 as usize, sk107 as usize, sk108 as usize, sk109 as usize, sk110 as usize, sk111 as usize, sk112 as usize, sk113 as usize, sk114 as usize, sk115 as usize, sk116 as usize, sk117 as usize, sk118 as usize, sk119 as usize, sk120 as usize, sk121 as usize, sk122 as usize, sk123 as usize, sk124 as usize, sk125 as usize, sk126 as usize, sk127 as usize]
}

fn parse_slots(v: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in v.split(',') {
        let part = part.trim();
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                out.extend((a..=b).filter(|s| (11..128).contains(s)));
            }
        } else if let Ok(s) = part.parse::<usize>() {
            if (11..128).contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

pub fn install_slots() {
    let Ok(v) = std::env::var("TOMBOX_DIAG_SKIP_SLOTS") else { return };
    let slots = parse_slots(&v);
    let mut done = Vec::new();
    for s in slots {
        let prev = crate::cmdlist_wrap::vt_get(s);
        if prev == 0 {
            continue;
        }
        SLOT_NEXT[s].store(prev, Ordering::Relaxed);
        SLOT_SKIP[s].store(true, Ordering::Relaxed);
        crate::cmdlist_wrap::vt_set(s, slot_fns()[s - 11]);
        done.push(s);
    }
    record(M, "diag_skip", &format!("DIAGNOSTIC: command-list slots dropped: {done:?} (TOMBOX_DIAG_SKIP_SLOTS)"));
}

#[cfg(test)]
mod tests {
    #[test]
    fn slot_lists_accept_ranges_and_ignore_object_methods() {
        assert_eq!(super::parse_slots("12, 50-52,3,200"), vec![12, 50, 51, 52]);
        assert_eq!(super::parse_slots(""), Vec::<usize>::new());
    }
}
