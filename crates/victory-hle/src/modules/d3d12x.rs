//! `d3d12_xs.dll`: virtual memory setup, device creation, root signature
//! serialization. The device and everything it creates live in
//! `victory-gpu`.

use std::ffi::c_void;

use victory_gpu::device;
use victory_gpu::object::{HRESULT, Iid, Object};

use crate::{HleExport, HleModule};

/// Device creation; first hands the device the title's general allocator
/// (`XMemAlloc`, which honours the title's hooks) for resource memory.
extern "system" fn create_device(
    adapter: *const c_void,
    parameters: *const u32,
    iid: *const Iid,
    out: *mut *const Object,
) -> HRESULT {
    victory_gpu::resource::use_allocator(super::xmem::XMemAlloc, super::xmem::XMemFree);
    victory_gpu::resource::use_page_mapper(super::xmem::map_alias);
    #[cfg(windows)]
    victory_gpu::on_failure(host_gpu_failed);
    device::create(adapter, parameters, iid, out)
}

/// A host GPU failure ends the run like a crash: reported to the crash log,
/// then the process ends as the console ends a title.
#[cfg(windows)]
fn host_gpu_failed(what: &str) -> ! {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
    victory_loader::log_crash(&format!("VIctory: host GPU failure: {what}"));
    const DXGI_ERROR_DEVICE_REMOVED: u32 = 0x887A_0005;
    // SAFETY: ends our own process.
    unsafe { TerminateProcess(GetCurrentProcess(), DXGI_ERROR_DEVICE_REMOVED) };
    unreachable!("the process has ended")
}

/// The offline warm-up: see `victory_gpu::shader_cache::warm`.
pub fn warm_shader(container: &[u8]) -> Result<bool, String> {
    #[cfg(windows)]
    {
        victory_gpu::shader_cache::warm(container)
    }
    #[cfg(not(windows))]
    {
        let _ = container;
        Err("no shader cache on this host".into())
    }
}

/// Keeps the host's translations of the title's shaders in `dir` (a
/// folder of the cache) across runs, and its compiled pipelines beside it
/// (`pipelines`).
pub fn use_shader_cache(dir: std::path::PathBuf) {
    #[cfg(windows)]
    {
        victory_gpu::pipeline::use_pipeline_cache(dir.with_file_name("pipelines"));
        victory_gpu::shader_cache::use_shader_cache(dir);
    }
    #[cfg(not(windows))]
    let _ = dir;
}

/// Updates the startup progress in telemetry and the GPU overlay.
pub fn set_progress(stage: &str, detail: &str, percent: u32) {
    let phase = if percent >= 40 {
        victory_core::telemetry::Phase::GpuInit
    } else if percent >= 20 {
        victory_core::telemetry::Phase::Loading
    } else {
        victory_core::telemetry::Phase::Preparing
    };
    victory_core::telemetry::set_stage(phase, stage, detail, percent);
    #[cfg(windows)]
    victory_gpu::progress::set_stage(stage, detail, percent);
    #[cfg(not(windows))]
    let _ = (stage, detail, percent);
}

#[cfg(windows)]
fn sample_hle() -> victory_gpu::telemetry::HleSnapshot {
    let ds = super::dstorage::telemetry_stats();
    let (limit, pages, mapped) = super::xmem::telemetry_stats();
    victory_gpu::telemetry::HleSnapshot {
        dstorage_queues: ds.queues,
        dstorage_workers: ds.workers,
        dstorage_queued_reads: ds.queued_reads,
        dstorage_running_reads: ds.running_reads,
        dstorage_held_reads: ds.held_reads,
        dstorage_done_reads: ds.done_reads,
        dstorage_done_bytes: ds.done_bytes,
        dstorage_bcpack_bytes: ds.bcpack_bytes,
        dstorage_staged_bytes: ds.staged_bytes,
        dstorage_staging_limit: ds.staging_limit,
        dstorage_staging_waits: ds.staging_waits,
        xmem_title_limit_bytes: limit,
        xmem_physical_pages_used: pages,
        xmem_physical_bytes_mapped: mapped,
    }
}

/// Registers HLE telemetry sampling and starts GPU pipeline/shader tracking.
pub fn enter_gpu_progress() {
    #[cfg(windows)]
    {
        victory_gpu::telemetry::register_hle_sampler(sample_hle);
        victory_gpu::progress::enter_gpu_phase();
    }
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
                address: victory_gpu::root::serialize as *const () as usize,
            },
            HleExport {
                name: "D3D12SerializeVersionedRootSignature",
                address: victory_gpu::root::serialize_versioned as *const () as usize,
            },
            // The AMD upscaler's (Series S).
            HleExport {
                name: "D3D12CreateRootSignatureDeserializer",
                address: victory_gpu::root::create_deserializer as *const () as usize,
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
    use victory_loader::ImportResolver;
    use victory_loader::Resolution;
    use victory_pe::ImportSymbol;

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
