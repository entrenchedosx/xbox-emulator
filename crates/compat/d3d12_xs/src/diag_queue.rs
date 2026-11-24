//! `TOMBOX_DIAG_QUEUE=1`: log every host command-queue Signal and Wait (fence, value, the fence's completed
//! value at that moment), so that a device hang can be matched with the cross-queue and fence ordering that
//! produced it. A Wait whose value is above the fence's completed value and that nothing later signals is the
//! signature of a queue that never makes progress. Off by default; logging only, nothing is changed.

use compat_common::record;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const M: &str = "d3d12_xs";
const SIGNAL: usize = 14;
const WAIT: usize = 15;
const LIMIT: u64 = 400;

static PREV_SIGNAL: AtomicUsize = AtomicUsize::new(0);
static PREV_WAIT: AtomicUsize = AtomicUsize::new(0);
static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn enabled() -> bool {
    static E: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *E.get_or_init(|| std::env::var("TOMBOX_DIAG_QUEUE").map(|v| v.trim() == "1").unwrap_or(false))
}

unsafe fn completed(fence: *mut c_void) -> Option<u64> {
    if fence.is_null() {
        return None;
    }
    let vt = *(fence as *const *const usize);
    let get: unsafe extern "system" fn(*mut c_void) -> u64 = core::mem::transmute(*vt.add(8));
    Some(get(fence))
}

fn note(what: &str, queue: *mut c_void, fence: *mut c_void, value: u64, done: Option<u64>) {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    if n < LIMIT {
        let blocks = done.is_some_and(|d| what == "Wait" && value > d);
        record(M, "diag_queue", &format!("#{n} {what} queue={queue:p} fence={fence:p} value={value} completed={done:?}{}", if blocks { "  <- the queue stalls here until another signal reaches this value" } else { "" }));
    }
}

unsafe extern "system" fn q_signal(this: *mut c_void, fence: *mut c_void, value: u64) -> i32 {
    note("Signal", this, fence, value, unsafe { completed(fence) });
    let f: unsafe extern "system" fn(*mut c_void, *mut c_void, u64) -> i32 = unsafe { core::mem::transmute(PREV_SIGNAL.load(Ordering::Relaxed)) };
    unsafe { f(this, fence, value) }
}

unsafe extern "system" fn q_wait(this: *mut c_void, fence: *mut c_void, value: u64) -> i32 {
    note("Wait", this, fence, value, unsafe { completed(fence) });
    let f: unsafe extern "system" fn(*mut c_void, *mut c_void, u64) -> i32 = unsafe { core::mem::transmute(PREV_WAIT.load(Ordering::Relaxed)) };
    unsafe { f(this, fence, value) }
}

pub fn install(queue_vt: &[AtomicUsize]) {
    if !enabled() || queue_vt.len() <= WAIT {
        return;
    }
    PREV_SIGNAL.store(queue_vt[SIGNAL].load(Ordering::Relaxed), Ordering::Relaxed);
    PREV_WAIT.store(queue_vt[WAIT].load(Ordering::Relaxed), Ordering::Relaxed);
    queue_vt[SIGNAL].store(q_signal as usize, Ordering::Relaxed);
    queue_vt[WAIT].store(q_wait as usize, Ordering::Relaxed);
    record(M, "diag_queue", "TOMBOX_DIAG_QUEUE=1: host queue Signal/Wait are logged (first 400)");
}
