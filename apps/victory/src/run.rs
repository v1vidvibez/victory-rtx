use std::path::Path;

use anyhow::{Context, Result};

use crate::{Console, Unimplemented};

#[cfg(windows)]
pub fn run(
    exe: &Path,
    unimplemented: Unimplemented,
    strict: bool,
    execute: bool,
    console: Console,
) -> Result<()> {
    use std::sync::Arc;
    use victory_hle::{HleResolver, HostPolicy};
    use victory_loader::{BindReport, UnimplementedPolicy, WithTitleDlls};

    // A title that relaunched itself: the old process must be gone first.
    victory_hle::modules::lifecycle::wait_for_previous();
    if execute {
        // SAFETY: enables per-monitor DPI awareness before any title or monitor window opens.
        unsafe {
            let _ = windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        init_telemetry_and_monitor(exe);
        victory_hle::modules::d3d12x::set_progress(
            "Preparing executable...",
            &exe.display().to_string(),
            5,
        );
    }
    let cfg = victory_core::Config::load()?;
    let original = std::fs::read(exe).with_context(|| format!("reading {}", exe.display()))?;
    let prepared = victory_loader::prepare(&original)?;
    let hash = prepared.original_hash.to_hex();
    let build_label = victory_game::identify(&hash).map(|b| b.label);
    match build_label {
        Some(label) => println!("build          {label} ({hash})"),
        None => println!("build          unknown ({hash})"),
    }
    if execute {
        let short_hash = &hash[..hash.len().min(16)];
        victory_core::telemetry::update(|s| {
            victory_core::telemetry::write_str(
                &mut s.build_label,
                &mut s.build_label_len,
                build_label.unwrap_or("unknown"),
            );
            victory_core::telemetry::write_str(&mut s.build_hash, &mut s.build_hash_len, short_hash);
        });
    }

    let images = cfg.cache_subdir("images")?;
    victory_hle::modules::d3d12x::use_shader_cache(cfg.cache_subdir("shaders")?);
    if execute {
        victory_hle::modules::d3d12x::set_progress("Mapping main image...", "Loading translated PE into memory", 15);
    }
    let image = victory_loader::load(&prepared, &images)?;
    // The program is mapped from the copy `load` wrote: the file's bytes
    // and their patched copy are not read again, and were held for the
    // whole run (two blocks of 114.7 MB each for the title, in
    // physical memory: run 65's memory map).
    drop(prepared);
    drop(original);
    victory_hle::runtime::set_main_image(image.base());
    victory_hle::runtime::set_image_path(image.base(), exe);
    // `VICTORY_RESOLUTION=1920x1080`: cap the title's resolution ladder so its
    // frame matches the display instead of the console's 4K target.
    let resolution = std::env::var("VICTORY_RESOLUTION").ok().and_then(|text| {
        let (w, h) = text.split_once('x')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    });
    if let Some((width, height)) = resolution {
        let base = image.base();
        let size = image.info().size_of_image as usize;
        // SAFETY: the image is mapped for `size` bytes; the cap rewrites it.
        let writable =
            unsafe { victory_loader::make_writable(base as *const core::ffi::c_void, size) };
        if writable {
            // SAFETY: writable for `size` bytes, as above.
            let changed =
                unsafe { victory_game::output::cap_ladder(base as *mut u8, size, width, height) };
            println!("resolution     {changed} ladder entry(ies) capped to {width}x{height}");
            if let Some(at) = unsafe { victory_game::output::assert_site(base as usize, size) } {
                // SAFETY: `at` lies inside the image, made writable above.
                unsafe { std::ptr::write_bytes(at as *mut u8, 0x90, 1) };
                println!("resolution     int3 at {at:#x} patched to nop");
            }
        } else {
            eprintln!("resolution     the image could not be made writable; no patch");
        }
        if execute {
            victory_core::telemetry::update(|s| {
                s.capped_width = width;
                s.capped_height = height;
            });
        }
    }
    println!(
        "mapped at      {:#x} (preferred {:#x})",
        image.base(),
        image.info().image_base
    );
    let resolver = HleResolver::new(if strict {
        HostPolicy::none()
    } else {
        HostPolicy::default()
    });
    let wanted = |dll: &str| title_dll(&resolver.host, dll);
    let title_dir = exe.parent().unwrap_or(Path::new("."));
    // A missing manifest leaves id 0 and no storage quota.
    let manifest =
        std::fs::read_to_string(title_dir.join("MicrosoftGame.config")).unwrap_or_default();
    let title = victory_hle::runtime::Title::from_manifest(&manifest);
    let storage = cfg.data_subdir(&format!("{:08X}/storage", title.id))?;
    println!(
        "title          {:08X}, storage {} ({} MB)",
        title.id,
        storage.display(),
        title.storage_bytes >> 20
    );
    // The console's fixed drives: the package as `G:`, temporary files `T:`.
    victory_hle::modules::files::mount_at('G', title_dir);
    let temp = cfg.data_subdir(&format!("{:08X}/temp", title.id))?;
    victory_hle::modules::files::mount_at('T', &temp);
    victory_hle::runtime::set_title(victory_hle::runtime::Title { storage, ..title });
    victory_hle::console::set(match console {
        Console::SeriesX => victory_hle::console::Console::SeriesX,
        Console::SeriesS => victory_hle::console::Console::SeriesS,
    });
    println!("console        {}", victory_hle::console::current().name());
    if execute {
        victory_core::telemetry::update(|s| {
            s.title_id = title.id;
            s.storage_quota_mb = (title.storage_bytes >> 20) as u32;
            s.console_mode = match console {
                Console::SeriesX => 0,
                Console::SeriesS => 1,
            };
        });
        victory_hle::modules::d3d12x::set_progress("Loading title DLLs...", "Resolving Xbox system & title libraries", 25);
    }
    let dlls = victory_loader::load_title_dlls(image.info(), title_dir, &images, &wanted)?;
    for (name, dll) in &dlls {
        println!("title dll      {name} at {:#x}", dll.base());
        victory_hle::runtime::set_image_path(dll.base(), &title_dir.join(name));
    }

    if execute {
        victory_hle::modules::d3d12x::set_progress("Binding imports...", "Linking HLE, host, and title functions", 35);
    }

    let policy = match unimplemented {
        Unimplemented::Trap => UnimplementedPolicy::Trap,
        Unimplemented::Zero => UnimplementedPolicy::ReturnZero,
    };
    let with_dlls = WithTitleDlls {
        inner: &resolver,
        dlls: &dlls,
    };
    let show = |name: &str, report: BindReport| {
        println!(
            "bound          {name}: {} hle, {} host, {} title, {} missing",
            report.hle,
            report.host,
            report.title,
            report.missing.len()
        );
        for m in &report.missing {
            println!("  missing      {}!{}", m.dll, m.symbol);
        }
    };
    let mut missing_total = 0u32;
    let mut record = |name: &str, report: BindReport| {
        missing_total = missing_total.saturating_add(report.missing.len() as u32);
        show(name, report);
    };
    for (name, dll) in &dlls {
        record(name, dll.bind_imports(&with_dlls, policy)?);
    }
    let exe_name = exe.file_name().unwrap_or_default().to_string_lossy();
    record(&exe_name, image.bind_imports(&with_dlls, policy)?);
    if !execute {
        println!("not executing (pass --execute to start the title)");
        return Ok(());
    }
    victory_core::telemetry::update(|s| {
        s.title_dll_count = dlls.len() as u32;
        s.missing_imports = missing_total;
    });
    victory_loader::report_crashes();
    victory_loader::log_crashes_to(temp.with_file_name("crashes.log"));
    victory_hle::runtime::use_console_timers();
    #[cfg(feature = "diagnostics")]
    diagnostics(&image, &cfg);
    let late = LateDlls {
        host: resolver.host.clone(),
        title_dir: title_dir.to_owned(),
        cache_dir: images,
        policy,
        mapped: std::sync::Mutex::new(dlls.clone()),
        lock: LoaderLock::default(),
    };
    victory_hle::runtime::set_title_dll_loader(Box::new(move |file| {
        late.load(file, show).map_err(|e| format!("{e:#}"))
    }));
    victory_hle::modules::d3d12x::set_progress("Starting title & GPU...", "Initializing D3D12 device and pipeline cache", 45);
    victory_hle::modules::d3d12x::enter_gpu_progress();
    // SAFETY: imports are bound; from here on we run title code.
    let main = match unsafe { Arc::new(image).spawn_main(dlls) } {
        Ok(main) => main,
        Err(e) => {
            victory_core::telemetry::mark_crashed(&format!("failed to start title: {e:#}"));
            return Err(e);
        }
    };
    let code = match main.join() {
        Ok(Ok(code)) => code,
        Ok(Err(e)) => {
            victory_core::telemetry::mark_crashed(&format!("title main failed: {e:#}"));
            return Err(e);
        }
        Err(_) => {
            victory_core::telemetry::mark_crashed("title main thread panicked");
            anyhow::bail!("title main thread panicked");
        }
    };
    victory_core::telemetry::mark_exited(code as i32);
    println!("title exited with {code}");
    Ok(())
}

/// Initializes shared-memory telemetry and launches the companion debug monitor
/// window unless `VICTORY_MONITOR=0`.
#[cfg(windows)]
fn init_telemetry_and_monitor(exe: &Path) {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    if std::env::var_os("VICTORY_MONITOR").is_some_and(|v| v == "0") {
        return;
    }
    let Some(pid) = victory_core::telemetry::init_publisher() else {
        return;
    };
    let exe_name = exe.file_name().unwrap_or_default().to_string_lossy();
    let mut flags: Vec<String> = std::env::vars()
        .filter(|(k, v)| k.starts_with("VICTORY_") && !v.is_empty() && k != "VICTORY_AUDIO_CAPTURE_OF")
        .map(|(k, v)| format!("{}={v}", k.strip_prefix("VICTORY_").unwrap_or(&k)))
        .collect();
    flags.sort();
    let flags_summary = flags.join(" ");
    victory_core::telemetry::update(|s| {
        victory_core::telemetry::write_str(&mut s.exe_name, &mut s.exe_name_len, &exe_name);
        victory_core::telemetry::write_str_u16(
            &mut s.flags_summary,
            &mut s.flags_summary_len,
            &flags_summary,
        );
    });
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if let Ok(self_exe) = std::env::current_exe() {
        let mut cmd = Command::new(self_exe);
        cmd.args(["monitor", "--pid", &pid.to_string()])
            .env_remove("VICTORY_AUDIO_CAPTURE_OF")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        if let Err(e) = cmd.spawn() {
            eprintln!("monitor        could not launch companion debug monitor: {e}");
        }
    }
}

/// The debugging aids (README): scheduled stack dumps, watched values and
/// memory saved on a crash.
#[cfg(all(windows, feature = "diagnostics"))]
fn diagnostics(image: &victory_loader::LoadedImage, cfg: &victory_core::Config) {
    victory_loader::dump_stacks_on_schedule();
    victory_hle::modules::audio::capture::start();
    watch(image.base(), image.info().size_of_image as usize);
    // Debugging aid: `VICTORY_CRASH_SAVE=<rva>:<len>,...` (hex) writes those
    // title ranges to `crash-<rva>.bin` in the cache dir on a crash.
    let size = image.info().size_of_image as usize;
    let ranges = std::env::var("VICTORY_CRASH_SAVE").unwrap_or_default();
    let hex = |s: &str| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    victory_loader::save_on_crash(
        ranges
            .split(',')
            .filter_map(|r| r.split_once(':'))
            .filter_map(|(rva, len)| Some((hex(rva)?, hex(len)?)))
            .filter(|&(rva, len)| rva.checked_add(len).is_some_and(|end| end <= size))
            .map(|(rva, len)| {
                let file = cfg.cache_dir.join(format!("crash-{rva:x}.bin"));
                (image.base() + rva, len, file)
            })
            .collect(),
    );
}

/// Debugging aid: `VICTORY_WATCH=<rva>,...` (hex) logs the u32 at each
/// title+rva whenever it changes, polled every 5 ms (an engine's state, say).
#[cfg(all(windows, feature = "diagnostics"))]
fn watch(base: usize, size: usize) {
    let Ok(list) = std::env::var("VICTORY_WATCH") else {
        return;
    };
    let rvas: Vec<usize> = list
        .split(',')
        .filter_map(|r| usize::from_str_radix(r.trim().trim_start_matches("0x"), 16).ok())
        .filter(|&rva| rva + 4 <= size)
        .collect();
    std::thread::spawn(move || {
        let mut seen = vec![None; rvas.len()];
        loop {
            for (&rva, seen) in rvas.iter().zip(&mut seen) {
                // SAFETY: four bytes inside the mapped image, checked above.
                let value = unsafe { ((base + rva) as *const u32).read_volatile() };
                if *seen != Some(value) {
                    eprintln!("VIctory: title+{rva:#x} = {value:#x}");
                    *seen = Some(value);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
}

/// DLLs the host or our runtime provide are never taken from the title.
#[cfg(windows)]
fn title_dll(host: &victory_hle::HostPolicy, dll: &str) -> bool {
    !host.allows(dll) && !victory_hle::registry().iter().any(|m| m.serves(dll))
}

/// Title DLLs the title loads itself (`LoadLibrary`) that nothing imports,
/// e.g. The title's upscaler behind its FidelityFX loader.
#[cfg(windows)]
struct LateDlls {
    host: victory_hle::HostPolicy,
    title_dir: std::path::PathBuf,
    cache_dir: std::path::PathBuf,
    policy: victory_loader::UnimplementedPolicy,
    /// Every title DLL mapped so far, to bind new ones against.
    mapped: std::sync::Mutex<Vec<(String, victory_loader::LoadedImage)>>,
    lock: LoaderLock,
}

#[cfg(windows)]
impl LateDlls {
    /// Every `LoadLibrary` of the title comes here first. Returns a title
    /// image called `file`, mapping it and the title DLLs it needs first,
    /// binding them and running their `DllMain` on the calling thread, as
    /// the Windows loader would; `None` if `file` is no title image.
    fn load(
        &self,
        file: &str,
        show: impl Fn(&str, victory_loader::BindReport),
    ) -> Result<Option<usize>> {
        use victory_hle::runtime::{forget_image, image_named, set_image_path};
        // Waits while another thread initialises a DLL; a `DllMain` that
        // loads another DLL re-enters.
        self.lock.hold(|| {
            if let Some(base) = image_named(file) {
                return Ok(Some(base));
            }
            let new = {
                let mut mapped = self.mapped.lock().unwrap_or_else(|e| e.into_inner());
                let wanted = |dll: &str| title_dll(&self.host, dll) && image_named(dll).is_none();
                let new = victory_loader::load_title_dll(
                    file,
                    &self.title_dir,
                    &self.cache_dir,
                    &wanted,
                )?;
                let hle = victory_hle::HleResolver::new(self.host.clone());
                let old = victory_loader::WithTitleDlls {
                    inner: &hle,
                    dlls: &mapped,
                };
                let resolver = victory_loader::WithTitleDlls {
                    inner: &old,
                    dlls: &new,
                };
                for (name, dll) in &new {
                    println!(
                        "title dll      {name} at {:#x} (loaded by the title)",
                        dll.base()
                    );
                    show(name, dll.bind_imports(&resolver, self.policy)?);
                    set_image_path(dll.base(), &self.title_dir.join(name));
                }
                // Visible before their `DllMain`s run, which may load more.
                mapped.extend(new.iter().cloned());
                new
            };
            let failed = new.iter().enumerate().find_map(|(i, (name, dll))| {
                // SAFETY: bound above, dependencies attached first.
                let attached = unsafe { dll.attach() };
                attached
                    .err()
                    .map(|e| (i, e.context(format!("attaching {name}"))))
            });
            let Some((i, error)) = failed else {
                return Ok(new.last().map(|(_, dll)| dll.base()));
            };
            // As `LoadLibrary` does when a `DllMain` fails: detach what this
            // load attached (the failed one too) and unmap all of it.
            let bases: Vec<_> = new.iter().map(|(_, d)| d.base()).collect();
            self.mapped
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(_, d)| !bases.contains(&d.base()));
            for (_, done) in new[..=i].iter().rev() {
                // SAFETY: attached (or failing to) on this thread just now.
                unsafe { done.detach() };
            }
            for (_, dll) in new {
                forget_image(dll.base());
                // SAFETY: detached, and no title code holds it yet.
                unsafe { dll.unload() };
            }
            Err(error)
        })
    }
}

/// The Windows loader lock's shape: one thread at a time, re-entered by
/// the thread that holds it.
#[cfg(windows)]
#[derive(Default)]
struct LoaderLock {
    owner: std::sync::Mutex<(Option<std::thread::ThreadId>, usize)>,
    free: std::sync::Condvar,
}

#[cfg(windows)]
impl LoaderLock {
    fn hold<T>(&self, f: impl FnOnce() -> T) -> T {
        let me = std::thread::current().id();
        let lock = || self.owner.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut owner = lock();
            while owner.0.is_some_and(|o| o != me) {
                owner = self.free.wait(owner).unwrap_or_else(|e| e.into_inner());
            }
            *owner = (Some(me), owner.1 + 1);
        }
        let result = f();
        let mut owner = lock();
        owner.1 -= 1;
        if owner.1 == 0 {
            owner.0 = None;
            self.free.notify_one();
        }
        result
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::sync::{Arc, Mutex};

    #[test]
    fn loader_lock_is_reentrant_and_exclusive() {
        let lock = Arc::new(super::LoaderLock::default());
        assert_eq!(lock.hold(|| lock.hold(|| 7)), 7);
        let order = Arc::new(Mutex::new(Vec::new()));
        let other = lock.hold(|| {
            let (theirs, their_order) = (lock.clone(), order.clone());
            let other = std::thread::spawn(move || {
                theirs.hold(|| their_order.lock().unwrap().push(2));
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            order.lock().unwrap().push(1);
            other
        });
        other.join().unwrap();
        assert_eq!(*order.lock().unwrap(), [1, 2]);
    }
}

#[cfg(not(windows))]
pub fn run(_: &Path, _: Unimplemented, _: bool, _: bool, _: Console) -> Result<()> {
    anyhow::bail!("running titles requires Windows")
}
