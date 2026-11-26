//! `d3d12_xs.dll`: virtual memory setup, device creation, root signature
//! serialization. The device and everything it creates live in
//! `vc-gpu`.

use std::ffi::c_void;

use vc_gpu::device;
use vc_gpu::object::{HRESULT, Iid, Object};

use crate::{HleExport, HleModule};

/// Device creation; first hands the device the title's general allocator
/// (`XMemAlloc`, which honours the title's hooks) for resource memory.
extern "system" fn create_device(
    adapter: *const c_void,
    parameters: *const u32,
    iid: *const Iid,
    out: *mut *const Object,
) -> HRESULT {
    vc_gpu::resource::use_allocator(super::xmem::XMemAlloc, super::xmem::XMemFree);
    vc_gpu::resource::use_page_mapper(super::xmem::map_alias);
    #[cfg(windows)]
    vc_gpu::on_failure(host_gpu_failed);
    device::create(adapter, parameters, iid, out)
}

/// A host GPU failure ends the run like a crash: reported to the crash log,
/// then the process ends as the console ends a title.
#[cfg(windows)]
fn host_gpu_failed(what: &str) -> ! {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
    vc_loader::log_crash(&format!("VC: host GPU failure: {what}"));
    const DXGI_ERROR_DEVICE_REMOVED: u32 = 0x887A_0005;
    // SAFETY: ends our own process.
    unsafe { TerminateProcess(GetCurrentProcess(), DXGI_ERROR_DEVICE_REMOVED) };
    unreachable!("the process has ended")
}

/// Keeps the host's translations of the title's shaders in `dir` (a
/// folder of the cache) across runs, and its compiled pipelines beside it
/// (`pipelines`).
pub fn use_shader_cache(dir: std::path::PathBuf) {
    #[cfg(windows)]
    {
        vc_gpu::pipeline::use_pipeline_cache(dir.with_file_name("pipelines"));
        vc_gpu::shader_cache::use_shader_cache(dir);
    }
    #[cfg(not(windows))]
    let _ = dir;
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "d3d12x",
        dlls: &["d3d12_xs.dll"],
        exports: vec![
            HleExport {
                name: "D3D12ConfigureVirtualMemoryX",
                address: device::configure_virtual_memory as *const () as usize,
            },
            HleExport {
                name: "D3D12XboxCreateDevice",
                address: create_device as *const () as usize,
            },
            HleExport {
                name: "D3D12SerializeRootSignature",
                address: vc_gpu::root::serialize as *const () as usize,
            },
            HleExport {
                name: "D3D12SerializeVersionedRootSignature",
                address: vc_gpu::root::serialize_versioned as *const () as usize,
            },
            // The AMD upscaler's (Series S).
            HleExport {
                name: "D3D12CreateRootSignatureDeserializer",
                address: vc_gpu::root::create_deserializer as *const () as usize,
            },
            // A data import: the title reads the id at this address.
            HleExport {
                name: "IID_ID3D12Device",
                address: (&raw const device::IID) as usize,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HleResolver, HostPolicy};
    use vc_loader::ImportResolver;
    use vc_loader::Resolution;
    use vc_pe::ImportSymbol;

    #[test]
    fn the_interface_id_import_is_bound_to_data() {
        let symbol = ImportSymbol::Name {
            hint: 0,
            name: "IID_ID3D12Device".into(),
        };
        let r = HleResolver::new(HostPolicy::none());
        let Resolution::Found { address, .. } = r.resolve("d3d12_xs.dll", &symbol) else {
            panic!("not bound");
        };
        // SAFETY: the address is our static id.
        assert_eq!(unsafe { *(address as *const [u8; 16]) }, device::IID);
    }
}
