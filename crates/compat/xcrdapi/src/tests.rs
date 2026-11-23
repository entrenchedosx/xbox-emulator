use crate::abi::*;
use crate::objects::*;
use crate::request::*;
use crate::DStorageGetFactory;
use core::ffi::c_void;
use std::io::Write;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

unsafe fn slot(obj: *mut c_void, i: usize) -> usize {
    unsafe { *(*(obj as *const *const usize)).add(i) }
}

fn zlib(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 7 + i / 13) % 251) as u8).collect()
}

fn build_request(options: u64, dst: u64, dst_size: u32, src: u64, off: u64, src_size: u32, tag: u64) -> [u8; XREQUEST_SIZE] {
    let mut b = [0u8; XREQUEST_SIZE];
    b[0x00..0x08].copy_from_slice(&options.to_le_bytes());
    b[0x08..0x10].copy_from_slice(&dst.to_le_bytes());
    b[0x18..0x1c].copy_from_slice(&dst_size.to_le_bytes());
    b[0x20..0x28].copy_from_slice(&src.to_le_bytes());
    b[0x28..0x30].copy_from_slice(&off.to_le_bytes());
    b[0x30..0x34].copy_from_slice(&src_size.to_le_bytes());
    b[0x38..0x40].copy_from_slice(&tag.to_le_bytes());
    b
}

fn temp_file(name: &str, data: &[u8]) -> (std::path::PathBuf, Vec<u16>) {
    let mut p = std::env::temp_dir();
    p.push(format!("xcrdapi_test_{}_{name}", std::process::id()));
    std::fs::File::create(&p).unwrap().write_all(data).unwrap();
    let w: Vec<u16> = p.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
    (p, w)
}

fn factory() -> *mut c_void {
    let mut f: *mut c_void = core::ptr::null_mut();
    assert_eq!(unsafe { DStorageGetFactory(IID_FACTORY_X.as_ptr() as *const c_void, &mut f) }, S_OK);
    f
}

unsafe fn create_queue(f: *mut c_void, source_type: u32, cap: u16, prio: i8) -> (i32, *mut c_void) {
    let mut d = [0u8; XQUEUE_DESC_SIZE];
    d[0..4].copy_from_slice(&source_type.to_le_bytes());
    d[8..10].copy_from_slice(&cap.to_le_bytes());
    d[10] = prio as u8;
    let mut q: *mut c_void = core::ptr::null_mut();
    let cq: unsafe extern "system" fn(*mut c_void, *const u8, *const c_void, *mut *mut c_void) -> i32 = unsafe { core::mem::transmute(slot(f, 3)) };
    let hr = unsafe { cq(f, d.as_ptr(), IID_QUEUE_X.as_ptr() as *const c_void, &mut q) };
    (hr, q)
}

unsafe fn open_file(f: *mut c_void, path: &[u16]) -> (i32, *mut c_void) {
    let mut o: *mut c_void = core::ptr::null_mut();
    let of: unsafe extern "system" fn(*mut c_void, *const u16, *const c_void, *mut *mut c_void) -> i32 = unsafe { core::mem::transmute(slot(f, 4)) };
    (unsafe { of(f, path.as_ptr(), IID_FILE_X.as_ptr() as *const c_void, &mut o) }, o)
}

unsafe fn create_status(f: *mut c_void, cap: u32) -> *mut c_void {
    let mut o: *mut c_void = core::ptr::null_mut();
    let cs: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const c_void, *mut *mut c_void) -> i32 = unsafe { core::mem::transmute(slot(f, 5)) };
    assert_eq!(unsafe { cs(f, cap, b"IoDispatcher\0".as_ptr(), IID_STATUS_ARRAY_X.as_ptr() as *const c_void, &mut o) }, S_OK);
    o
}

unsafe fn enqueue(q: *mut c_void, r: &[u8; XREQUEST_SIZE]) {
    let f: unsafe extern "system" fn(*mut c_void, *const u8) = unsafe { core::mem::transmute(slot(q, 3)) };
    unsafe { f(q, r.as_ptr()) }
}

unsafe fn enqueue_status(q: *mut c_void, a: *mut c_void, i: u32) {
    let f: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) = unsafe { core::mem::transmute(slot(q, 4)) };
    unsafe { f(q, a, i) }
}

unsafe fn submit(q: *mut c_void) {
    let f: unsafe extern "system" fn(*mut c_void) = unsafe { core::mem::transmute(slot(q, 6)) };
    unsafe { f(q) }
}

unsafe fn release(o: *mut c_void) -> u32 {
    let f: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { core::mem::transmute(slot(o, 2)) };
    unsafe { f(o) }
}

unsafe fn is_complete(a: *mut c_void, i: u32) -> bool {
    let f: unsafe extern "system" fn(*mut c_void, u32) -> bool = unsafe { core::mem::transmute(slot(a, 3)) };
    unsafe { f(a, i) }
}

unsafe fn get_hr(a: *mut c_void, i: u32) -> i32 {
    let f: unsafe extern "system" fn(*mut c_void, u32) -> i32 = unsafe { core::mem::transmute(slot(a, 4)) };
    unsafe { f(a, i) }
}

unsafe fn wait(a: *mut c_void, i: u32) -> i32 {
    let t = Instant::now();
    while !unsafe { is_complete(a, i) } {
        assert!(t.elapsed() < Duration::from_secs(10), "status {i} never completed");
        std::thread::sleep(Duration::from_millis(1));
    }
    unsafe { get_hr(a, i) }
}

#[test]
fn iid_bytes_match_console_tables() {
    assert_eq!(IID_FACTORY_X, [0xac, 0x54, 0x3b, 0xfb, 0x82, 0xbf, 0x3a, 0x46, 0xb7, 0x77, 0x73, 0x19, 0x58, 0x37, 0x86, 0x4d]);
    assert_eq!(iid_text(&IID_STATUS_ARRAY_X), "cd66d752-2786-44f7-9070-e2a6fdad3f1b");
    assert_eq!(iid_text(&IID_QUEUE_X), "a3f6649a-8c2c-4b3a-8b6e-10186a04b9bc");
    assert_eq!(iid_text(&IID_FILE_X), "43704f9f-27c6-4a9f-8815-d16527894df4");
}

#[test]
fn decode_file_request_layout() {
    let mut b = build_request(0x1, 0x1111_2222, 0x10000, 0x3333_4444, 0x5555_6666_7777, 0xa93, 0xabcd);
    b[0x10..0x12].copy_from_slice(&0x77u16.to_le_bytes());
    b[0x34..0x38].copy_from_slice(&0x10000u32.to_le_bytes());
    b[0x40..0x48].copy_from_slice(&0x9999u64.to_le_bytes());
    let r = XRequest::decode(&b);
    assert_eq!(r.compression(), COMPRESSION_ZLIB);
    assert!(!r.memory_source());
    assert_eq!(r.destination, 0x1111_2222);
    assert_eq!(r.ext10, 0);
    assert_eq!(r.destination_size, 0x10000);
    assert_eq!(r.source, 0x3333_4444);
    assert_eq!(r.source_offset, 0x5555_6666_7777);
    assert_eq!(r.source_size, 0xa93);
    assert_eq!(r.field34, 0x10000);
    assert_eq!(r.cancellation_tag, 0xabcd);
    assert_eq!(r.name, 0x9999);
}

#[test]
fn decode_memory_request_and_extension_fields() {
    let mut b = build_request(OPT_SOURCE_MEMORY | OPT_EXT28_VALID | OPT_EXT10_VALID, 0x10, 4, 0x20, 0xdead_0042, 4, 0);
    b[0x10..0x12].copy_from_slice(&0x1234u16.to_le_bytes());
    let r = XRequest::decode(&b);
    assert!(r.memory_source());
    assert_eq!(r.source_offset, 0);
    assert_eq!(r.ext28, 0x0042);
    assert_eq!(r.ext10, 0x1234);
}

#[test]
fn validation_rules() {
    let ok = XRequest::decode(&build_request(0, 0x1000, 16, 0x2000, 0, 16, 0));
    assert_eq!(validate(&ok, QUEUE_SOURCE_FILE), Ok(()));
    assert_eq!(validate(&ok, QUEUE_SOURCE_MEMORY).unwrap_err().hr, E_DSTORAGE_INVALID_SOURCE_TYPE);
    let bad_size = XRequest::decode(&build_request(0, 0x1000, 32, 0x2000, 0, 16, 0));
    assert_eq!(validate(&bad_size, QUEUE_SOURCE_FILE).unwrap_err().hr, E_DSTORAGE_INVALID_DESTINATION_SIZE);
    let zlib_small = XRequest::decode(&build_request(1, 0x1000, 16, 0x2000, 0, 16, 0));
    assert_eq!(validate(&zlib_small, QUEUE_SOURCE_FILE).unwrap_err().hr, E_DSTORAGE_INVALID_DESTINATION_SIZE);
    let zlib_ok = XRequest::decode(&build_request(1, 0x1000, 64, 0x2000, 0, 16, 0));
    assert_eq!(validate(&zlib_ok, QUEUE_SOURCE_FILE), Ok(()));
    for mode in 2u64..16 {
        let r = XRequest::decode(&build_request(mode << 1, 0x1000, 64, 0x2000, 0, 16, 0));
        assert_eq!(validate(&r, QUEUE_SOURCE_FILE).unwrap_err().hr, E_DSTORAGE_DECOMPRESSION_ERROR, "bcpack mode {mode}");
    }
    let bc1 = XRequest::decode(&build_request(2, 0x1000, 64, 0x2000, 0, 16, 0));
    assert_eq!(validate(&bc1, QUEUE_SOURCE_FILE), Ok(()));
    assert_eq!((bc1.zlib(), bc1.bcpack_mode()), (false, 1));
    let bc1_odd = XRequest::decode(&build_request(2, 0x1000, 60, 0x2000, 0, 16, 0));
    assert_eq!(validate(&bc1_odd, QUEUE_SOURCE_FILE).unwrap_err().hr, E_DSTORAGE_INVALID_DESTINATION_SIZE);
    let zlib_bc1_no_mid = XRequest::decode(&build_request(3, 0x1000, 64, 0x2000, 0, 16, 0));
    assert_eq!(validate(&zlib_bc1_no_mid, QUEUE_SOURCE_FILE).unwrap_err().hr, E_DSTORAGE_INVALID_INTERMEDIATE_SIZE);
    for bit in [5u64, 7, 8, 20, 21, 23, 40, 63] {
        let r = XRequest::decode(&build_request(1 << bit, 0x1000, 16, 0x2000, 0, 16, 0));
        assert_eq!(validate(&r, QUEUE_SOURCE_FILE).unwrap_err().hr, E_DSTORAGE_RESERVED_FIELDS, "bit {bit}");
    }
    let null_dst = XRequest::decode(&build_request(0, 0, 16, 0x2000, 0, 16, 0));
    assert_eq!(validate(&null_dst, QUEUE_SOURCE_FILE).unwrap_err().hr, E_DSTORAGE_ACCESS_VIOLATION);
    let mut b = build_request(1, 0x1000, 64, 0x2000, 0, 16, 0);
    b[0x34..0x38].copy_from_slice(&40u32.to_le_bytes());
    assert_eq!(validate(&XRequest::decode(&b), QUEUE_SOURCE_FILE).unwrap_err().hr, E_DSTORAGE_INVALID_INTERMEDIATE_SIZE);
}

#[test]
fn inflate_round_trip_and_errors() {
    let data = pattern(65536);
    let c = zlib(&data);
    let mut out = vec![0u8; data.len()];
    assert_eq!(inflate_into(&c, &mut out), Ok(data.len()));
    assert_eq!(out, data);
    let mut padded = c.clone();
    padded.extend_from_slice(&[0xee; 13]);
    let mut out2 = vec![0u8; data.len()];
    assert_eq!(inflate_into(&padded, &mut out2), Ok(data.len()));
    assert_eq!(out2, data);
    let mut small = vec![0u8; data.len() - 1];
    assert_eq!(inflate_into(&c, &mut small), Err(E_DSTORAGE_DECOMPRESSION_ERROR));
    let mut hdr = c.clone();
    hdr[0] = 0x00;
    assert_eq!(inflate_into(&hdr, &mut out), Err(E_DSTORAGE_ZLIB_BAD_HEADER));
    let mut trunc = c.clone();
    trunc.truncate(c.len() / 2);
    assert_eq!(inflate_into(&trunc, &mut out), Err(E_DSTORAGE_ZLIB_BAD_DATA));
    let mut adler = c.clone();
    let n = adler.len();
    adler[n - 1] ^= 0xff;
    assert_eq!(inflate_into(&adler, &mut out), Err(E_DSTORAGE_ZLIB_PARITY_FAIL));
    let mut junk = c.clone();
    for b in junk[2..20].iter_mut() {
        *b = 0xff;
    }
    assert!(matches!(inflate_into(&junk, &mut out), Err(E_DSTORAGE_ZLIB_BAD_DATA) | Err(E_DSTORAGE_DECOMPRESSION_ERROR) | Err(E_DSTORAGE_ZLIB_PARITY_FAIL)));
}

#[test]
fn positioned_reads_and_eof() {
    let data = pattern(10000);
    let (p, _) = temp_file("reads.bin", &data);
    let f = std::fs::File::open(&p).unwrap();
    let mut buf = vec![0u8; 100];
    read_at(&f, 4321, &mut buf).unwrap();
    assert_eq!(&buf[..], &data[4321..4421]);
    let mut over = vec![0u8; 200];
    assert_eq!(read_at(&f, 9900, &mut over), Err(E_DSTORAGE_END_OF_FILE));
    let c = zlib(&data[..5000]);
    let mut with = vec![0u8; 77];
    with.extend_from_slice(&c);
    let (p2, _) = temp_file("zl.bin", &with);
    let f2 = std::fs::File::open(&p2).unwrap();
    let mut dst = vec![0u8; 5000];
    assert_eq!(execute(COMPRESSION_ZLIB, Source::File(&f2, 77), c.len() as u32, &mut dst), Ok(Outcome { written: 5000 }));
    assert_eq!(&dst[..], &data[..5000]);
    drop(f);
    drop(f2);
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(p2);
}

#[test]
fn factory_identity_and_vtable_layout() {
    let f = factory();
    assert_eq!(f, factory());
    unsafe {
        assert_eq!(slot(f, 1), slot(f, 2));
        let add: unsafe extern "system" fn(*mut c_void) -> u32 = core::mem::transmute(slot(f, 1));
        assert_eq!(add(f), 1);
        assert_eq!(release(f), 1);
        let mut out: *mut c_void = 1 as *mut c_void;
        let bad = IID_D3D12_FENCE_PC;
        assert_eq!(DStorageGetFactory(bad.as_ptr() as *const c_void, &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        // The retail GTA VI build asks for the X2 factory ID and dereferences the
        // result without checking the HRESULT, so refusing it crashes the title.
        let mut x2: *mut c_void = core::ptr::null_mut();
        assert_eq!(DStorageGetFactory(IID_FACTORY_X2.as_ptr() as *const c_void, &mut x2), S_OK);
        assert_eq!(x2, f);
        assert_eq!(DStorageGetFactory(IID_IUNKNOWN.as_ptr() as *const c_void, &mut out), S_OK);
        assert_eq!(out, f);
        let set_dbg: unsafe extern "system" fn(*mut c_void, u32) = core::mem::transmute(slot(f, 7));
        set_dbg(f, 0x1);
        assert_eq!(DEBUG_FLAGS.load(Ordering::Relaxed), 1);
        set_dbg(f, 0);
        let aff: unsafe extern "system" fn(*mut c_void, u64) -> i32 = core::mem::transmute(slot(f, 6));
        assert_eq!(aff(f, process_affinity()), S_OK);
        assert_eq!(aff(f, 0), E_INVALIDARG);
        let before = crate::TRAPS.load(Ordering::SeqCst);
        let t9: unsafe extern "system" fn(*mut c_void) -> i32 = core::mem::transmute(slot(f, 9));
        assert_eq!(t9(f), E_NOTIMPL);
        assert!(crate::TRAPS.load(Ordering::SeqCst) > before);
    }
}

#[test]
fn queue_creation_validation() {
    let f = factory();
    unsafe {
        assert_eq!(create_queue(f, 0, 0x2000, 3).0, E_DSTORAGE_INVALID_QUEUE_PRIORITY);
        assert_eq!(create_queue(f, 7, 0x2000, 0).0, E_DSTORAGE_INVALID_QUEUE_TYPE);
        assert_eq!(create_queue(f, 1, 0x80, 0).0, E_DSTORAGE_INVALID_MEMORY_QUEUE_PRIORITY);
        assert_eq!(create_queue(f, 0, 1, 0).0, E_DSTORAGE_INVALID_QUEUE_CAPACITY);
        let (hr, q) = create_queue(f, 0, 0x2000, 2);
        assert_eq!(hr, S_OK);
        let mut other: *mut c_void = core::ptr::null_mut();
        let qi: unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> i32 = core::mem::transmute(slot(q, 0));
        assert_eq!(qi(q, IID_FILE_X.as_ptr() as *const c_void, &mut other), E_NOINTERFACE);
        assert_eq!(qi(q, IID_IUNKNOWN.as_ptr() as *const c_void, &mut other), S_OK);
        assert_eq!(other, q);
        assert_eq!(release(other), 1);
        let mut info = [0u8; XQUEUE_INFO_SIZE];
        let query: unsafe extern "system" fn(*mut c_void, *mut u8) = core::mem::transmute(slot(q, 11));
        query(q, info.as_mut_ptr());
        assert_eq!(u16::from_le_bytes([info[8], info[9]]), 0x2000);
        assert_eq!(info[10], 2);
        assert_eq!(u16::from_le_bytes([info[0x18], info[0x19]]), 0x1fff);
        assert_eq!(u16::from_le_bytes([info[0x1a], info[0x1b]]), 0x1000);
        assert_eq!(release(q), 0);
    }
}

#[test]
fn file_queue_end_to_end() {
    let data = pattern(200_000);
    let comp = zlib(&data[100_000..165_536]);
    let mut contents = data.clone();
    let comp_off = contents.len() as u64;
    contents.extend_from_slice(&comp);
    let (p, w) = temp_file("e2e.bin", &contents);
    let f = factory();
    unsafe {
        let (hr, file) = open_file(f, &w);
        assert_eq!(hr, S_OK);
        let gh: unsafe extern "system" fn(*mut c_void) -> isize = core::mem::transmute(slot(file, 4));
        let h = gh(file);
        assert!(h != 0 && h != -1);
        let (hr, q) = create_queue(f, QUEUE_SOURCE_FILE, 0x80, 0);
        assert_eq!(hr, S_OK);
        let arr = create_status(f, 4);
        assert!(!is_complete(arr, 0));
        assert_eq!(get_hr(arr, 0), E_PENDING);
        assert_eq!(get_hr(arr, 9), E_DSTORAGE_INDEX_BOUND);
        assert!(!is_complete(arr, 9));
        let mut raw = vec![0u8; 4096];
        let mut inflated = vec![0u8; 65536];
        enqueue(q, &build_request(0, raw.as_mut_ptr() as u64, 4096, file as u64, 12345, 4096, 1));
        enqueue(q, &build_request(1, inflated.as_mut_ptr() as u64, 65536, file as u64, comp_off, comp.len() as u32, 2));
        enqueue_status(q, arr, 0);
        let mut past = vec![0u8; 64];
        enqueue(q, &build_request(0, past.as_mut_ptr() as u64, 64, file as u64, contents.len() as u64 - 10, 64, 3));
        enqueue_status(q, arr, 1);
        enqueue(q, &build_request(0x100, raw.as_mut_ptr() as u64, 16, file as u64, 0, 16, 4));
        enqueue_status(q, arr, 2);
        enqueue_status(q, arr, 3);
        submit(q);
        assert_eq!(wait(arr, 0), S_OK);
        assert_eq!(&raw[..], &data[12345..12345 + 4096]);
        assert_eq!(&inflated[..], &data[100_000..165_536]);
        assert_eq!(wait(arr, 1), E_DSTORAGE_END_OF_FILE);
        assert_eq!(wait(arr, 2), E_DSTORAGE_RESERVED_FIELDS);
        assert_eq!(wait(arr, 3), S_OK);
        let mut rec = [0u8; XERROR_RECORD_SIZE];
        let rer: unsafe extern "system" fn(*mut c_void, *mut u8) = core::mem::transmute(slot(q, 10));
        rer(q, rec.as_mut_ptr());
        assert_eq!(u32::from_le_bytes(rec[0..4].try_into().unwrap()), 2);
        assert_eq!(i32::from_le_bytes(rec[8..12].try_into().unwrap()), E_DSTORAGE_END_OF_FILE);
        assert_eq!(u32::from_le_bytes(rec[0x0c..0x10].try_into().unwrap()), COMMAND_TYPE_REQUEST);
        assert_eq!(u64::from_le_bytes(rec[0x248..0x250].try_into().unwrap()), 3);
        assert_ne!(u16::from_le_bytes([rec[XERR_FILENAME_OFF], rec[XERR_FILENAME_OFF + 1]]), 0);
        let gev: unsafe extern "system" fn(*mut c_void) -> isize = core::mem::transmute(slot(q, 9));
        assert!(gev(q) != 0);
        let close: unsafe extern "system" fn(*mut c_void) = core::mem::transmute(slot(file, 3));
        close(file);
        let gid: unsafe extern "system" fn(*mut c_void) -> u32 = core::mem::transmute(slot(file, 7));
        assert_eq!(gid(file), u32::MAX);
        enqueue(q, &build_request(0, raw.as_mut_ptr() as u64, 16, file as u64, 0, 16, 5));
        enqueue_status(q, arr, 0);
        submit(q);
        assert_eq!(wait(arr, 0), E_DSTORAGE_INVALID_FILE_HANDLE);
        let qclose: unsafe extern "system" fn(*mut c_void) = core::mem::transmute(slot(q, 8));
        qclose(q);
        enqueue_status(q, arr, 1);
        assert_eq!(get_hr(arr, 1), E_DSTORAGE_END_OF_FILE);
        assert_eq!(release(q), 0);
        assert_eq!(release(arr), 0);
        assert_eq!(release(file), 0);
    }
    let _ = std::fs::remove_file(p);
}

#[test]
fn cancellation_by_tag() {
    let data = pattern(4096);
    let (p, w) = temp_file("cancel.bin", &data);
    let f = factory();
    unsafe {
        let (_, file) = open_file(f, &w);
        let (_, q) = create_queue(f, QUEUE_SOURCE_FILE, 0x80, 0);
        let arr = create_status(f, 2);
        let mut a = vec![0u8; 16];
        let mut b = vec![0u8; 16];
        enqueue(q, &build_request(0, a.as_mut_ptr() as u64, 16, file as u64, 0, 16, 0x10));
        enqueue_status(q, arr, 0);
        enqueue(q, &build_request(0, b.as_mut_ptr() as u64, 16, file as u64, 16, 16, 0x21));
        enqueue_status(q, arr, 1);
        let cancel: unsafe extern "system" fn(*mut c_void, u64, u64) = core::mem::transmute(slot(q, 7));
        cancel(q, 0xf0, 0x10);
        submit(q);
        assert_eq!(wait(arr, 0), HR_OPERATION_ABORTED);
        assert_eq!(a, vec![0u8; 16]);
        assert_eq!(wait(arr, 1), S_OK);
        assert_eq!(&b[..], &data[16..32]);
        release(q);
        release(arr);
        release(file);
    }
    let _ = std::fs::remove_file(p);
}

#[test]
fn memory_queue_and_auto_submit() {
    let f = factory();
    let data = pattern(3000);
    let comp = zlib(&data);
    unsafe {
        let (hr, q) = create_queue(f, QUEUE_SOURCE_MEMORY, 4, PRIORITY_REALTIME);
        assert_eq!(hr, S_OK);
        let arr = create_status(f, 1);
        let mut out = vec![0u8; 3000];
        enqueue(q, &build_request(OPT_SOURCE_MEMORY | 1, out.as_mut_ptr() as u64, 3000, comp.as_ptr() as u64, 0, comp.len() as u32, 0));
        enqueue_status(q, arr, 0);
        assert_eq!(wait(arr, 0), S_OK);
        assert_eq!(out, data);
        let mut d2 = vec![0u8; 8];
        enqueue(q, &build_request(0, d2.as_mut_ptr() as u64, 8, 0x1234, 0, 8, 0));
        enqueue_status(q, arr, 0);
        assert_eq!(wait(arr, 0), E_DSTORAGE_INVALID_SOURCE_TYPE);
        release(q);
        release(arr);
    }
}

#[repr(C)]
struct FakeFence {
    vtbl: *const [usize; 11],
    refs: AtomicU32,
    value: AtomicU64,
}

unsafe extern "system" fn ff_qi(this: *mut c_void, riid: *const Iid, ppv: *mut *mut c_void) -> i32 {
    if unsafe { *riid } == IID_D3D12_FENCE_PC {
        unsafe { (*(this as *const FakeFence)).refs.fetch_add(1, Ordering::SeqCst) };
        unsafe { *ppv = this };
        S_OK
    } else {
        E_NOINTERFACE
    }
}
unsafe extern "system" fn ff_add(this: *mut c_void) -> u32 {
    unsafe { (*(this as *const FakeFence)).refs.fetch_add(1, Ordering::SeqCst) + 1 }
}
unsafe extern "system" fn ff_rel(this: *mut c_void) -> u32 {
    unsafe { (*(this as *const FakeFence)).refs.fetch_sub(1, Ordering::SeqCst) - 1 }
}
unsafe extern "system" fn ff_other(_this: *mut c_void) -> i32 {
    E_NOTIMPL
}
unsafe extern "system" fn ff_signal(this: *mut c_void, v: u64) -> i32 {
    unsafe { (*(this as *const FakeFence)).value.store(v, Ordering::SeqCst) };
    S_OK
}

#[test]
fn enqueue_signal_reaches_pc_fence() {
    let mut vt = [0usize; 11];
    vt[0] = ff_qi as *const () as usize;
    vt[1] = ff_add as *const () as usize;
    vt[2] = ff_rel as *const () as usize;
    for s in vt.iter_mut().take(10).skip(3) {
        *s = ff_other as *const () as usize;
    }
    vt[10] = ff_signal as *const () as usize;
    let fence = FakeFence { vtbl: &vt, refs: AtomicU32::new(1), value: AtomicU64::new(0) };
    let f = factory();
    unsafe {
        let (_, q) = create_queue(f, QUEUE_SOURCE_FILE, 0x80, 0);
        let sig: unsafe extern "system" fn(*mut c_void, *mut c_void, u64) = core::mem::transmute(slot(q, 5));
        sig(q, &fence as *const FakeFence as *mut c_void, 42);
        submit(q);
        let t = Instant::now();
        while fence.value.load(Ordering::SeqCst) != 42 {
            assert!(t.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(1));
        }
        let t = Instant::now();
        while fence.refs.load(Ordering::SeqCst) != 1 {
            assert!(t.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(1));
        }
        release(q);
    }
}

#[test]
fn staging_buffer_locked_while_objects_live() {
    let f = factory();
    unsafe {
        let (_, q) = create_queue(f, QUEUE_SOURCE_FILE, 0x80, 0);
        let ssb: unsafe extern "system" fn(*mut c_void, u32) -> i32 = core::mem::transmute(slot(f, 8));
        assert_eq!(ssb(f, 32 << 20), E_DSTORAGE_STAGING_BUFFER_LOCKED);
        release(q);
    }
}

#[path = "../../console-dll/oracle/leaf.rs"]
mod leaf;

#[test]
fn fixed_result_exports_match_every_console_xcrdapi() {
    type F = unsafe extern "system" fn(usize, usize, usize, usize) -> i32;
    let ours: [(&str, extern "system" fn() -> i32, u32); 7] = [
        ("XCrdQueryEncryptedInfo", crate::xcrd_a::XCrdQueryEncryptedInfo, 0x8007_0032),
        ("XCrdQueryUnlockInfo", crate::xcrd_a::XCrdQueryUnlockInfo, 0x8007_0032),
        ("XCrdRepairFiles", crate::xcrd_a::XCrdRepairFiles, 0x8007_0032),
        ("XCrdTrimDeallocatedClusters", crate::xcrd2_storage::XCrdTrimDeallocatedClusters, 0x8007_0032),
        ("XCrdShuffle", crate::xcrd2_storage::XCrdShuffle, 0x8000_4001),
        ("XCrdWaitForAllRemovableDiskRemoval", crate::xcrd2_storage::XCrdWaitForAllRemovableDiskRemoval, 0x8000_4001),
        ("XCrdWaitForAnyRemovableDiskArrival", crate::xcrd2_storage::XCrdWaitForAnyRemovableDiskArrival, 0x8000_4001),
    ];
    let images = leaf::images("xcrdapi.dll");
    assert!(images.len() >= 3, "{} console xcrdapi.dll dumps", images.len());
    let mut seen = std::collections::HashMap::new();
    for img in &images {
        for (name, f, want) in ours {
            let (mine, mine_le) = leaf::with_sentinel(|| f());
            assert_eq!((mine as u32, mine_le), (want, leaf::SENTINEL), "{name}");
            let Some(p) = leaf::try_export(img, name) else { continue };
            let console: F = unsafe { core::mem::transmute(p) };
            for a in [[0usize; 4], [1, 0x1000, usize::MAX, 0xdead_beef]] {
                let (theirs, their_le) = leaf::with_sentinel(|| unsafe { console(a[0], a[1], a[2], a[3]) });
                assert_eq!((theirs, their_le), (mine, mine_le), "{name} vs {}", img.source.display());
            }
            *seen.entry(name).or_insert(0) += 1;
        }
    }
    for (name, _, _) in ours {
        assert!(seen.get(name).copied().unwrap_or(0) >= 3, "{name} compared against {:?} console dumps", seen.get(name));
    }
}

// ---- IDStorageFactoryX2::CreateQueue1 / IDStorageQueueX2 (the call the retail title makes) ----

#[link(name = "kernel32")]
extern "system" {
    fn CreateEventW(sa: *mut c_void, manual: i32, initial: i32, name: *const u16) -> isize;
    fn WaitForSingleObject(h: isize, ms: u32) -> u32;
    fn CloseHandle(h: isize) -> i32;
}

/// `CreateQueue1(&DSTORAGE_QUEUE_DESC1{ SourceType, Capacity, Priority, Name = 0, Options }, IID_QUEUE_X2, &q)`.
unsafe fn create_queue1(f: *mut c_void, source_type: u64, cap: u16, prio: i8, options: u64) -> (i32, *mut c_void) {
    let mut d = [0u8; XQUEUE_DESC1_SIZE];
    d[0..8].copy_from_slice(&source_type.to_le_bytes());
    d[8..10].copy_from_slice(&cap.to_le_bytes());
    d[10] = prio as u8;
    d[0x18..0x20].copy_from_slice(&options.to_le_bytes());
    let mut q: *mut c_void = core::ptr::null_mut();
    let cq: unsafe extern "system" fn(*mut c_void, *const u8, *const c_void, *mut *mut c_void) -> i32 = unsafe { core::mem::transmute(slot(f, 10)) };
    let hr = unsafe { cq(f, d.as_ptr(), IID_QUEUE_X2.as_ptr() as *const c_void, &mut q) };
    (hr, q)
}

unsafe fn query1(q: *mut c_void) -> [u8; XQUEUE_INFO1_SIZE] {
    let mut info = [0u8; XQUEUE_INFO1_SIZE];
    let f: unsafe extern "system" fn(*mut c_void, *mut u8) = unsafe { core::mem::transmute(slot(q, 13)) };
    unsafe { f(q, info.as_mut_ptr()) };
    info
}

#[test]
fn queue_x2_iid_is_the_one_the_title_asks_for() {
    assert_eq!(iid_text(&IID_QUEUE_X2), "3c02cb0c-c050-4195-99a1-a1ce94ba33c5");
}

#[test]
fn create_queue1_makes_a_queue_x2_and_query1_reports_its_desc1() {
    let f = factory();
    unsafe {
        let (hr, q) = create_queue1(f, QUEUE_SOURCE_FILE as u64, 0x40, PRIORITY_LOW, OPT_QUEUE_AUTO_SUBMIT_DISABLED);
        assert_eq!(hr, S_OK);
        assert!(!q.is_null());
        let info = query1(q);
        assert_eq!(u64::from_le_bytes(info[0..8].try_into().unwrap()), QUEUE_SOURCE_FILE as u64, "source type");
        assert_eq!(u16::from_le_bytes(info[8..10].try_into().unwrap()), 0x40, "capacity");
        assert_eq!(info[10] as i8, PRIORITY_LOW, "priority");
        assert_eq!(u64::from_le_bytes(info[0x18..0x20].try_into().unwrap()), 1, "options echo the AutoSubmitDisabled bit");
        assert_eq!(u16::from_le_bytes(info[0x20..0x22].try_into().unwrap()), 0x3f, "an empty queue has capacity - 1 empty slots");
        // the same object answers for the older queue interface, and refuses interfaces nobody has described
        let qi: unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> i32 = core::mem::transmute(slot(q, 0));
        let mut o: *mut c_void = core::ptr::null_mut();
        assert_eq!(qi(q, IID_QUEUE_X.as_ptr() as *const c_void, &mut o), S_OK);
        assert_eq!(o, q);
        release(o);
        assert_eq!(qi(q, IID_FILE_X.as_ptr() as *const c_void, &mut o), E_NOINTERFACE);
        release(q);
    }
}

#[test]
fn create_queue1_validates_like_create_queue_and_rejects_reserved_option_bits() {
    let f = factory();
    unsafe {
        assert_eq!(create_queue1(f, 0, 0x40, 0, 2).0, E_DSTORAGE_RESERVED_FIELDS, "reserved option bits must be zero");
        assert_eq!(create_queue1(f, 0, 0x40, 0, 1u64 << 63).0, E_DSTORAGE_RESERVED_FIELDS);
        assert_eq!(create_queue1(f, 0, 1, 0, 0).0, E_DSTORAGE_INVALID_QUEUE_CAPACITY);
        assert_eq!(create_queue1(f, 0, 0x40, 3, 0).0, E_DSTORAGE_INVALID_QUEUE_PRIORITY);
        assert_eq!(create_queue1(f, 7, 0x40, 0, 0).0, E_DSTORAGE_INVALID_QUEUE_TYPE);
        assert_eq!(create_queue1(f, QUEUE_SOURCE_MEMORY as u64, 0x40, 0, 0).0, E_DSTORAGE_INVALID_MEMORY_QUEUE_PRIORITY);
        let cq: unsafe extern "system" fn(*mut c_void, *const u8, *const c_void, *mut *mut c_void) -> i32 = core::mem::transmute(slot(f, 10));
        let mut q: *mut c_void = 1 as *mut c_void;
        assert_eq!(cq(f, core::ptr::null(), IID_QUEUE_X2.as_ptr() as *const c_void, &mut q), E_INVALIDARG);
        assert!(q.is_null(), "the out pointer is cleared on failure");
    }
}

/// With auto-submit on, half a queue's capacity of entries starts the work by itself; a queue created
/// with `AutoSubmitDisabled` runs nothing until Submit.
#[test]
fn auto_submit_disabled_queue_runs_nothing_until_submit() {
    let f = factory();
    let data = pattern(512);
    let comp = zlib(&data);
    unsafe {
        let arr = create_status(f, 2);
        for (disabled, label) in [(false, "auto-submit on"), (true, "auto-submit disabled")] {
            let (hr, q) = create_queue1(f, QUEUE_SOURCE_MEMORY as u64, 4, PRIORITY_REALTIME, disabled as u64);
            assert_eq!(hr, S_OK);
            let mut out = vec![0u8; 512];
            enqueue(q, &build_request(OPT_SOURCE_MEMORY | 1, out.as_mut_ptr() as u64, 512, comp.as_ptr() as u64, 0, comp.len() as u32, 0));
            enqueue_status(q, arr, 1); // two entries: half of a capacity of 4
            let t = Instant::now();
            let mut done = false;
            while t.elapsed() < Duration::from_millis(400) && !done {
                done = is_complete(arr, 1);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(done, !disabled, "{label}: completion before Submit");
            if disabled {
                assert!(out.iter().all(|&b| b == 0), "{label}: nothing may have been decoded yet");
                submit(q);
            }
            assert_eq!(wait(arr, 1), S_OK, "{label}");
            assert_eq!(out, data, "{label}");
            release(q);
        }
        release(arr);
    }
}

/// A full queue whose auto-submit is disabled stalls its Enqueue until another thread submits.
#[test]
fn enqueue_on_a_full_auto_submit_disabled_queue_waits_for_submit() {
    let f = factory();
    unsafe {
        let arr = create_status(f, 4);
        let (hr, q) = create_queue1(f, QUEUE_SOURCE_FILE as u64, 3, 0, OPT_QUEUE_AUTO_SUBMIT_DISABLED); // 2 usable slots
        assert_eq!(hr, S_OK);
        enqueue_status(q, arr, 0);
        enqueue_status(q, arr, 1);
        let (qa, aa) = (q as usize, arr as usize);
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fin = finished.clone();
        let t = std::thread::spawn(move || {
            enqueue_status(qa as *mut c_void, aa as *mut c_void, 2);
            fin.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(!finished.load(Ordering::SeqCst), "the third Enqueue must stall while nothing submits the queue");
        submit(q);
        let t0 = Instant::now();
        while !finished.load(Ordering::SeqCst) {
            assert!(t0.elapsed() < Duration::from_secs(10), "Submit did not release the stalled Enqueue");
            std::thread::sleep(Duration::from_millis(2));
        }
        t.join().unwrap();
        submit(q);
        assert_eq!(wait(arr, 2), S_OK);
        release(q);
        release(arr);
    }
}

#[test]
fn closing_a_queue_releases_a_stalled_enqueue() {
    let f = factory();
    unsafe {
        let arr = create_status(f, 4);
        let (_, q) = create_queue1(f, QUEUE_SOURCE_FILE as u64, 2, 0, OPT_QUEUE_AUTO_SUBMIT_DISABLED); // 1 usable slot
        enqueue_status(q, arr, 0);
        let (qa, aa) = (q as usize, arr as usize);
        let t = std::thread::spawn(move || enqueue_status(qa as *mut c_void, aa as *mut c_void, 1));
        std::thread::sleep(Duration::from_millis(200));
        let close: unsafe extern "system" fn(*mut c_void) = core::mem::transmute(slot(q, 8));
        close(q);
        t.join().unwrap();
        assert_eq!(get_hr(arr, 0), E_DSTORAGE_QUEUE_CLOSED, "an entry that never ran is reported as closed");
        release(q);
        release(arr);
    }
}

/// `EnqueueSetEvent` signals the event after everything enqueued before it has completed.
#[test]
fn enqueue_set_event_follows_the_requests_before_it() {
    let f = factory();
    let data = pattern(2048);
    let comp = zlib(&data);
    unsafe {
        let (hr, q) = create_queue1(f, QUEUE_SOURCE_MEMORY as u64, 8, PRIORITY_REALTIME, OPT_QUEUE_AUTO_SUBMIT_DISABLED);
        assert_eq!(hr, S_OK);
        let ev = CreateEventW(core::ptr::null_mut(), 1, 0, core::ptr::null());
        assert_ne!(ev, 0);
        let mut out = vec![0u8; 2048];
        enqueue(q, &build_request(OPT_SOURCE_MEMORY | 1, out.as_mut_ptr() as u64, 2048, comp.as_ptr() as u64, 0, comp.len() as u32, 0));
        let set: unsafe extern "system" fn(*mut c_void, isize) = core::mem::transmute(slot(q, 12));
        set(q, ev);
        assert_eq!(WaitForSingleObject(ev, 150), 0x102, "nothing runs before Submit");
        submit(q);
        assert_eq!(WaitForSingleObject(ev, 10_000), 0, "the event is signalled after Submit");
        assert_eq!(out, data, "the request before the event has completed when the event fires");
        // a bad handle is a recorded failure, not a hang or a crash
        set(q, 0);
        submit(q);
        let t0 = Instant::now();
        let rec = loop {
            let mut rec = [0u8; XERROR_RECORD_SIZE];
            let gr: unsafe extern "system" fn(*mut c_void, *mut u8) = core::mem::transmute(slot(q, 10));
            gr(q, rec.as_mut_ptr());
            if i32::from_le_bytes(rec[8..12].try_into().unwrap()) != 0 {
                break rec;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "no error record for the invalid event handle");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(i32::from_le_bytes(rec[8..12].try_into().unwrap()), E_INVALIDARG);
        CloseHandle(ev);
        release(q);
    }
}

#[test]
fn close_discards_an_unsubmitted_completion_event() {
    let f = factory();
    unsafe {
        let (hr, q) = create_queue1(f, QUEUE_SOURCE_MEMORY as u64, 8, PRIORITY_REALTIME, OPT_QUEUE_AUTO_SUBMIT_DISABLED);
        assert_eq!(hr, S_OK);
        let event = CreateEventW(core::ptr::null_mut(), 1, 0, core::ptr::null());
        assert_ne!(event, 0);
        let set: unsafe extern "system" fn(*mut c_void, isize) = core::mem::transmute(slot(q, 12));
        let close: unsafe extern "system" fn(*mut c_void) = core::mem::transmute(slot(q, 8));
        set(q, event);
        close(q);
        assert_eq!(WaitForSingleObject(event, 0), 0x102);
        assert_ne!(CloseHandle(event), 0);
        release(q);
    }
}
