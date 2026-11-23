use compat_common::record;

const PC_PRIORITY_NORMAL: u32 = 0;
const PC_PRIORITY_HIGH: u32 = 100;
const PC_PRIORITY_GLOBAL_REALTIME: u32 = 10000;
const PC_TYPE_COPY: u32 = 3;
const XBOX_TYPE_DMA: u32 = 0x10;
/// Bits of `D3D12_COMMAND_QUEUE_FLAGS` that exist on the PC API.
const PC_FLAGS_KNOWN: u32 = 0x1;

fn get(d: &[u8; 16], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

fn put(d: &mut [u8; 16], o: usize, v: u32) {
    d[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn translate(d: &mut [u8; 16]) {
    let (ty, prio, flags, node) = (get(d, 0), get(d, 4), get(d, 8), get(d, 12));
    let pc_ty = if ty == XBOX_TYPE_DMA { PC_TYPE_COPY } else { ty };
    let pc_prio = match prio {
        PC_PRIORITY_NORMAL | PC_PRIORITY_HIGH | PC_PRIORITY_GLOBAL_REALTIME => prio,
        1 => PC_PRIORITY_HIGH,
        _ => PC_PRIORITY_HIGH,
    };
    let pc_node = if node == 0 { 0 } else { 1 };
    // The PC runtime defines exactly two queue flags (0 = none,
    // 1 = disable GPU timeout) and answers E_INVALIDARG for anything else.
    // The console descriptor carries additional Xbox-only bits: the GTA VI
    // startup sequence creates two compute queues with flags 0x2 and 0x3,
    // which the PC runtime rejected, failing the whole initialisation routine.
    // Keep the bit the PC API can express, drop the rest, and say so in the
    // log - the queue itself is still a real, usable PC queue.
    let pc_flags = flags & PC_FLAGS_KNOWN;
    put(d, 0, pc_ty);
    put(d, 4, pc_prio);
    put(d, 8, pc_flags);
    put(d, 12, pc_node);
    if pc_flags != flags {
        record("d3d12_xs", "xone.queue_flags", &format!(
            "console flags={flags:#x} -> PC flags={pc_flags:#x} (type={ty:#x})"
        ));
    }
    if (pc_ty, pc_prio, pc_node) != (ty, prio, node) {
        record("d3d12_xs", "xone.queue", &format!(
            "Xbox queue desc type={ty:#x} priority={prio} flags={flags:#x} node={node:#x} -> PC type={pc_ty} priority={pc_prio} node={pc_node}"
        ));
    }
}

pub fn list_type(t: u32) -> u32 {
    if t == XBOX_TYPE_DMA { PC_TYPE_COPY } else { t }
}

pub unsafe fn create_allocator(real: *mut core::ffi::c_void, t: u32, riid: *const core::ffi::c_void, ppv: *mut *mut core::ffi::c_void) -> i32 {
    let pc = list_type(t);
    let f: unsafe extern "system" fn(*mut core::ffi::c_void, u32, *const core::ffi::c_void, *mut *mut core::ffi::c_void) -> i32 =
        unsafe { core::mem::transmute(*(*(real as *const *const usize)).add(9)) };
    let hr = unsafe { f(real, pc, riid, ppv) };
    if pc != t || hr != 0 {
        record("d3d12_xs", "xone.CreateCommandAllocator", &format!("Xbox type={t:#x} -> PC type={pc} hr={hr:#010x}"));
    }
    hr
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(ty: u32, prio: u32, flags: u32, node: u32) -> [u8; 16] {
        let mut d = [0u8; 16];
        d[0..4].copy_from_slice(&ty.to_le_bytes());
        d[4..8].copy_from_slice(&prio.to_le_bytes());
        d[8..12].copy_from_slice(&flags.to_le_bytes());
        d[12..16].copy_from_slice(&node.to_le_bytes());
        d
    }

    fn fields(d: &[u8; 16]) -> (u32, u32, u32, u32) {
        (get(d, 0), get(d, 4), get(d, 8), get(d, 12))
    }

    /// Console-only queue flag bits must not reach the PC runtime, which
    /// rejects them with E_INVALIDARG (this is what failed GTA VI's init).
    #[test]
    fn translate_drops_console_only_queue_flags() {
        let mut d = desc(2, 0, 0x3, 1);
        translate(&mut d);
        assert_eq!(fields(&d).2, 0x1, "disable-GPU-timeout bit must survive");
        let mut d = desc(2, 0, 0x2, 1);
        translate(&mut d);
        assert_eq!(fields(&d).2, 0, "console-only bit 0x2 must be dropped");
        let mut d = desc(3, 0, 0x0, 1);
        translate(&mut d);
        assert_eq!(fields(&d).2, 0, "no flags stays no flags");
    }

    #[test]
    fn translate_maps_dma_type_to_copy() {
        let mut d = desc(XBOX_TYPE_DMA, 0, 0, 1);
        translate(&mut d);
        assert_eq!(fields(&d).0, PC_TYPE_COPY);
    }
}
