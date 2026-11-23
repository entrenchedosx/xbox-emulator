use core::ffi::c_void;

extern "system" {
    fn CreateEventW(sa: *mut c_void, manual: i32, initial: i32, name: *const u16) -> isize;
    fn WaitForSingleObject(h: isize, ms: u32) -> u32;
}

thread_local! {
    static EV: isize = unsafe { CreateEventW(core::ptr::null_mut(), 0, 0, core::ptr::null()) };
}

fn reached(completed: u64, value: u64) -> bool {
    completed != u64::MAX && completed >= value
}

pub unsafe fn wait_fence(fence: *mut c_void, value: u64, timeout_ms: u32) -> bool {
    let vt = *(fence as *const *const usize);
    let completed: unsafe extern "system" fn(*mut c_void) -> u64 = core::mem::transmute(*vt.add(8));
    let first = completed(fence);
    if reached(first, value) {
        return true;
    }
    if first == u64::MAX || timeout_ms == 0 {
        return false;
    }
    let set_event: unsafe extern "system" fn(*mut c_void, u64, isize) -> i32 = core::mem::transmute(*vt.add(9));
    let ev = EV.with(|e| *e);
    let mut armed = ev != 0 && set_event(fence, value, ev) >= 0;
    let start = std::time::Instant::now();
    loop {
        let current = completed(fence);
        if reached(current, value) {
            return true;
        }
        if current == u64::MAX {
            return false;
        }
        let remaining = if timeout_ms == u32::MAX {
            u32::MAX
        } else {
            let elapsed = start.elapsed().as_millis();
            if elapsed >= u128::from(timeout_ms) {
                return false;
            }
            timeout_ms - elapsed as u32
        };
        if armed {
            // A wake can come from device removal or a previous timed-out wait.
            // Always confirm this fence reached its requested value.
            armed = WaitForSingleObject(ev, remaining) == 0;
        } else {
            std::thread::yield_now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::reached;

    #[test]
    fn device_removal_is_not_fence_completion() {
        assert!(!reached(u64::MAX, 1));
        assert!(!reached(u64::MAX, u64::MAX));
        assert!(!reached(4, 5));
        assert!(reached(5, 5));
        assert!(reached(6, 5));
    }
}
