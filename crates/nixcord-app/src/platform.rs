//! OS-specific renderer choice and memory tweaks.
//!
//! Nearly all of nixcord's memory is the GPU driver, not the app (the Rust heap is ~6 MB
//! in `--demo` with a channel open). So the levers here are which graphics API and driver
//! we load, how the driver is asked to allocate, and giving back what startup touched.

/// Renderer settings for this OS, as a base for [`eframe::NativeOptions`].
///
/// Windows: wgpu on Direct3D 12 only. The AMD/Intel/NVIDIA OpenGL drivers commit a lot
/// more memory than their D3D12 drivers (measured on AMD: ~190 MB private with glow vs
/// ~140 MB with D3D12), and Vulkan is worse still. Every Windows 10+ machine has D3D12,
/// with the WARP software adapter as a last resort.
#[cfg(windows)]
pub fn renderer_options() -> eframe::NativeOptions {
    use eframe::egui_wgpu::{
        wgpu, SurfaceConfig, WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew,
    };
    use std::sync::Arc;

    let mut setup = WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor.backends = wgpu::Backends::DX12;
    // FXC ships with Windows. The default ("Auto") picks up any `dxcompiler.dll` found on
    // PATH (some other app's copy), which is neither smaller nor something we control.
    setup
        .instance_descriptor
        .backend_options
        .dx12
        .shader_compiler = wgpu::Dx12Compiler::Fxc;
    // `None` takes the adapter DXGI lists first: the one driving the primary display, so
    // frames are not copied across GPUs (and on laptops that is the integrated GPU).
    setup.power_preference = wgpu::PowerPreference::None;
    let base = setup.device_descriptor.clone();
    setup.device_descriptor = Arc::new(move |adapter| wgpu::DeviceDescriptor {
        // The default (`Performance`) pre-allocates 128 MB device + 64 MB host memory
        // blocks; egui needs a few MB of buffers and one font atlas.
        memory_hints: wgpu::MemoryHints::MemoryUsage,
        ..base(adapter)
    });

    eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: WgpuConfiguration {
            // One frame in flight: fewer swapchain buffers held, lower input latency.
            surface: SurfaceConfig::LOW_LATENCY,
            wgpu_setup: WgpuSetup::CreateNew(setup),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Renderer settings for this OS, as a base for [`eframe::NativeOptions`].
#[cfg(not(windows))]
pub fn renderer_options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    }
}

/// Once startup has settled, return the pages only startup touched to the OS.
///
/// GPU driver init, shader compilation and font parsing touch a lot of memory exactly
/// once. Windows keeps those pages in the working set until there is memory pressure.
/// Trimming moves them to the standby list; pages the app really uses fault straight back
/// in on the next frame (soft faults, no disk I/O; no frame-time hitch measured), so the
/// working set afterwards is what nixcord actually uses. Committed (private) memory is
/// unchanged by this.
#[cfg(windows)]
pub fn trim_after_startup() {
    let spawned = std::thread::Builder::new()
        .name("nixcord-trim".into())
        .spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(3));
            trim_working_set();
        });
    if let Err(e) = spawned {
        tracing::debug!("working-set trim thread not started: {e}");
    }
}

#[cfg(not(windows))]
pub fn trim_after_startup() {}

#[cfg(windows)]
fn trim_working_set() {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn SetProcessWorkingSetSize(process: *mut c_void, min: usize, max: usize) -> i32;
    }

    // SAFETY: a plain Win32 call on our own process pseudo-handle. Passing usize::MAX for
    // both sizes is the documented way to ask Windows to trim the working set.
    let ok = unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
    if ok == 0 {
        tracing::debug!("SetProcessWorkingSetSize failed");
    }
}
