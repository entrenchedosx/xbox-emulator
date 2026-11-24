use core::ffi::c_void;
use std::time::{Duration, Instant};

use crate::device_wrap::Guid;

const IID_DEVICE: Guid = Guid { d1: 0x1898_19f1, d2: 0x1db6, d3: 0x4b57, d4: [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7] };
const IID_QUEUE: Guid = Guid { d1: 0x0ec8_70a6, d2: 0x5d7e, d3: 0x4c22, d4: [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed] };
const IID_ALLOCATOR: Guid = Guid { d1: 0x6102_dee4, d2: 0xaf59, d3: 0x4b09, d4: [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24] };
const IID_LIST: Guid = Guid { d1: 0x5b16_0d0f, d2: 0xac1b, d3: 0x4185, d4: [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55] };
const IID_RESOURCE: Guid = Guid { d1: 0x6964_42be, d2: 0xa72e, d3: 0x4059, d4: [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad] };
const IID_FENCE: Guid = Guid { d1: 0x0a75_3dcf, d2: 0xc4d8, d3: 0x4b91, d4: [0xad, 0xf6, 0xbe, 0x5a, 0x60, 0xd9, 0x5a, 0x76] };
const IID_ROOT_SIGNATURE: Guid = Guid { d1: 0xc54a_6b66, d2: 0x72df, d3: 0x4ee8, d4: [0x8b, 0xe5, 0xa9, 0x46, 0xa1, 0x42, 0x92, 0x14] };
const IID_PSO: Guid = Guid { d1: 0x765a_30f3, d2: 0xf624, d3: 0x4c6f, d4: [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45] };
const IID_HEAP: Guid = Guid { d1: 0x8efb_471d, d2: 0x616c, d3: 0x4f49, d4: [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51] };

const HEAP_DEFAULT: u32 = 1;
const HEAP_UPLOAD: u32 = 2;
const HEAP_READBACK: u32 = 3;
const ST_UAV: u32 = 8;
const ST_COPY_DEST: u32 = 0x400;
const ST_COPY_SOURCE: u32 = 0x800;
const ST_GENERIC_READ: u32 = 0xac3;

const X_CLOSE: usize = 9;
const X_DISPATCH: usize = 14;
const X_COPY_BUFFER_REGION: usize = 15;
const X_SET_PSO: usize = 25;
const X_BARRIER: usize = 26;
const X_HEAPS: usize = 32;
const X_CROOT: usize = 33;
const X_CTABLE: usize = 35;

unsafe fn vs(o: *mut c_void, i: usize) -> usize {
    unsafe { *(*(o as *const *const usize)).add(i) }
}

macro_rules! call {
    ($o:expr, $i:expr, ($($t:ty),*) -> $r:ty $(, $a:expr)*) => {{
        let o = $o as *mut c_void;
        let f: unsafe extern "system" fn(*mut c_void $(, $t)*) -> $r = core::mem::transmute(vs(o, $i));
        f(o $(, $a)*)
    }};
}

struct Buf {
    res: usize,
    va: u64,
    cpu: *mut u8,
}

unsafe fn buffer(dev: usize, heap: u32, size: u64, flags: u32, state: u32) -> Buf {
    let h: [u32; 5] = [heap, 0, 0, 1, 1];
    let mut d = [0u8; 56];
    d[0..4].copy_from_slice(&1u32.to_le_bytes());
    d[0x10..0x18].copy_from_slice(&size.to_le_bytes());
    d[0x18..0x1c].copy_from_slice(&1u32.to_le_bytes());
    d[0x1c..0x1e].copy_from_slice(&1u16.to_le_bytes());
    d[0x1e..0x20].copy_from_slice(&1u16.to_le_bytes());
    d[0x24..0x28].copy_from_slice(&1u32.to_le_bytes());
    d[0x2c..0x30].copy_from_slice(&1u32.to_le_bytes());
    d[0x30..0x34].copy_from_slice(&flags.to_le_bytes());
    let mut r: *mut c_void = core::ptr::null_mut();
    let hr = unsafe {
        call!(dev, 27, (*const u32, u32, *const u8, u32, *const c_void, *const Guid, *mut *mut c_void) -> i32,
            h.as_ptr(), 0, d.as_ptr(), state, core::ptr::null(), &IID_RESOURCE, &mut r)
    };
    assert!(hr >= 0, "CreateCommittedResource hr={hr:#010x}");
    let va = unsafe { call!(r, 11, () -> u64) };
    crate::gpu_va::insert(crate::gpu_va::Entry { start: va, len: size, res: r as usize, buffer: true });
    crate::gpu_va::set_state(r as usize, state);
    let mut cpu: *mut u8 = core::ptr::null_mut();
    if heap != HEAP_DEFAULT {
        assert!(unsafe { call!(r, 8, (u32, *const c_void, *mut *mut u8) -> i32, 0, core::ptr::null(), &mut cpu) } >= 0);
    }
    Buf { res: r as usize, va, cpu }
}

fn raw_vsharp(va: u64, bytes: u32) -> [u32; 8] {
    [va as u32, (va >> 32) as u32 & 0xffff, bytes, 0xfac | 20 << 12 | 3 << 28, 0, 0, 0, 0]
}

fn barrier(res: usize, before: u32, after: u32) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[8..16].copy_from_slice(&(res as u64).to_le_bytes());
    b[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    b[20..24].copy_from_slice(&before.to_le_bytes());
    b[24..28].copy_from_slice(&after.to_le_bytes());
    b
}

unsafe fn run(q: usize, list: *mut c_void, fence: *mut c_void, v: u64) {
    let l = [list];
    unsafe {
        call!(q, 10, (u32, *const *mut c_void) -> (), 1, l.as_ptr());
        call!(q, 14, (*mut c_void, u64) -> i32, fence, v);
    }
    let t = Instant::now();
    while unsafe { call!(fence, 8, () -> u64) } < v {
        assert!(t.elapsed() < Duration::from_secs(10), "GPU did not reach fence value {v}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "needs a D3D12 device; run: cargo test -p d3d12_xs --lib hw_ -- --ignored --test-threads=1 (TOMBOX_HW_ADAPTER=warp for WARP)"]
fn hw_title_heap_descriptors_written_after_recording_are_read_at_execute() {
    unsafe {
        crate::enable_experimental_shader_models();
        let create: extern "system" fn(*mut c_void, u32, *const Guid, *mut *mut c_void) -> i32 = core::mem::transmute(crate::host_proc(b"D3D12CreateDevice\0"));
        let mut dev: *mut c_void = core::ptr::null_mut();
        let hr = create(core::ptr::null_mut(), 0xb000, &IID_DEVICE, &mut dev);
        assert!(hr >= 0 && !dev.is_null(), "D3D12CreateDevice hr={hr:#010x}");
        let dev = dev as usize;
        let qdesc = [0u32; 4];
        let mut q: *mut c_void = core::ptr::null_mut();
        assert!(call!(dev, 8, (*const u32, *const Guid, *mut *mut c_void) -> i32, qdesc.as_ptr(), &IID_QUEUE, &mut q) >= 0);
        crate::sys_ts::note_queue(q, vs(q, 16), vs(q, 17));
        crate::present::apply_queue_vtable(q);
        let hlsl = "ByteAddressBuffer src : register(t0);\nRWByteAddressBuffer dst : register(u0);\n[RootSignature(\"DescriptorTable(SRV(t0), UAV(u0))\")]\n[numthreads(1, 1, 1)]\nvoid main() { dst.Store(0, src.Load(0) + 1); }\n";
        let code = tombox_xshader::dxc::compile(hlsl, "cs_6_0").expect("dxc");
        let mut rs: *mut c_void = core::ptr::null_mut();
        assert!(call!(dev, 16, (u32, *const u8, usize, *const Guid, *mut *mut c_void) -> i32, 0, code.as_ptr(), code.len(), &IID_ROOT_SIGNATURE, &mut rs) >= 0);
        assert!(super::note_root_signature(rs as usize, &code));
        let mut pd = [0u8; 56];
        pd[0..8].copy_from_slice(&(rs as u64).to_le_bytes());
        pd[8..16].copy_from_slice(&(code.as_ptr() as u64).to_le_bytes());
        pd[16..24].copy_from_slice(&(code.len() as u64).to_le_bytes());
        let mut pso: *mut c_void = core::ptr::null_mut();
        assert!(call!(dev, 11, (*const u8, *const Guid, *mut *mut c_void) -> i32, pd.as_ptr(), &IID_PSO, &mut pso) >= 0);
        let hdesc: [u32; 4] = [0, 4, 1, 0];
        let mut heap: *mut c_void = core::ptr::null_mut();
        assert!(call!(dev, 14, (*const u32, *const Guid, *mut *mut c_void) -> i32, hdesc.as_ptr(), &IID_HEAP, &mut heap) >= 0);
        let mut mem = vec![[0u32; 8]; 4];
        let memory = mem.as_mut_ptr() as u64;
        assert_eq!(super::register(heap as usize, memory, 4u64 << 32, super::FAMILY_SCARLETT, 4), 0);
        let src_a = buffer(dev, HEAP_UPLOAD, 256, 0, ST_GENERIC_READ);
        let src_b = buffer(dev, HEAP_UPLOAD, 256, 0, ST_GENERIC_READ);
        (src_a.cpu as *mut u32).write_volatile(41);
        (src_b.cpu as *mut u32).write_volatile(99);
        let dst = buffer(dev, HEAP_DEFAULT, 256, 4, ST_UAV);
        let rb = buffer(dev, HEAP_READBACK, 256, 0, ST_COPY_DEST);
        let mut a: *mut c_void = core::ptr::null_mut();
        assert!(call!(dev, 9, (u32, *const Guid, *mut *mut c_void) -> i32, 0, &IID_ALLOCATOR, &mut a) >= 0);
        let mut raw: *mut c_void = core::ptr::null_mut();
        assert!(call!(dev, 12, (u32, u32, *mut c_void, *mut c_void, *const Guid, *mut *mut c_void) -> i32, 0, 0, a, core::ptr::null_mut(), &IID_LIST, &mut raw) >= 0);
        let list = crate::cmdlist_wrap::wrap(raw);
        let hs = [heap as usize];
        call!(list, X_HEAPS, (u32, *const usize) -> (), 1, hs.as_ptr());
        call!(list, X_CROOT, (*mut c_void) -> (), rs);
        call!(list, X_SET_PSO, (*mut c_void) -> (), pso);
        call!(list, X_CTABLE, (u32, u64) -> (), 0, memory);
        call!(list, X_DISPATCH, (u32, u32, u32) -> (), 1, 1, 1);
        let b1 = barrier(dst.res, ST_UAV, ST_COPY_SOURCE);
        call!(list, X_BARRIER, (u32, *const u8) -> (), 1, b1.as_ptr());
        call!(list, X_COPY_BUFFER_REGION, (usize, u64, usize, u64, u64) -> (), rb.res, 0, dst.res, 0, 4);
        let b2 = barrier(dst.res, ST_COPY_SOURCE, ST_UAV);
        call!(list, X_BARRIER, (u32, *const u8) -> (), 1, b2.as_ptr());
        assert!(call!(list, X_CLOSE, () -> i32) >= 0);
        mem[0] = raw_vsharp(src_a.va, 256);
        mem[1] = raw_vsharp(dst.va, 256);
        let mut fence: *mut c_void = core::ptr::null_mut();
        assert!(call!(dev, 36, (u64, u32, *const Guid, *mut *mut c_void) -> i32, 0, 0, &IID_FENCE, &mut fence) >= 0);
        run(q as usize, list, fence, 1);
        assert_eq!((rb.cpu as *const u32).read_volatile(), 42);
        mem[0] = raw_vsharp(src_b.va, 256);
        run(q as usize, list, fence, 2);
        assert_eq!((rb.cpu as *const u32).read_volatile(), 100);
        assert_eq!(call!(dev, 37, () -> i32), 0);
        assert!(super::forget(heap as usize));
    }
}
