//! Shared-memory telemetry channel between a running `victory` title process
//! and the companion debug monitor process.
//!
//! Uses a ping-pong double buffer (`slots: [TelemetrySnapshot; 2]`) plus a
//! lock-free frame-time ring and a lock-free log ring inside a named Windows
//! pagefile section (`Local\victory-telemetry-<pid>`). Because updates never
//! take locks across processes and never overwrite the currently published
//! slot, a sudden crash or device loss leaves the last complete snapshot intact.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use tracing::Level;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

pub const TELEMETRY_MAGIC: u64 = 0x5649_4354_4F52_5932;
pub const TELEMETRY_VERSION: u32 = 2;
pub const FRAME_RING_LEN: usize = 240;
pub const LOG_RING_LEN: usize = 64;

/// High-level lifecycle phase of the running title.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Boot = 0,
    Preparing = 1,
    Loading = 2,
    GpuInit = 3,
    Running = 4,
    Exited = 5,
    Crashed = 6,
}

impl Phase {
    pub fn from_raw(v: u32) -> Self {
        match v {
            0 => Self::Boot,
            1 => Self::Preparing,
            2 => Self::Loading,
            3 => Self::GpuInit,
            4 => Self::Running,
            5 => Self::Exited,
            6 => Self::Crashed,
            _ => Self::Boot,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Boot => "BOOT",
            Self::Preparing => "PREPARING",
            Self::Loading => "LOADING",
            Self::GpuInit => "GPU INIT",
            Self::Running => "RUNNING",
            Self::Exited => "EXITED",
            Self::Crashed => "CRASHED",
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
}

impl LogLevel {
    pub fn from_raw(v: u8) -> Self {
        match v {
            0 => Self::Error,
            1 => Self::Warn,
            _ => Self::Info,
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            Self::Error => "ERR",
            Self::Warn => "WRN",
            Self::Info => "INF",
        }
    }
}

/// One log entry in the shared ring buffer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogEntry {
    pub seq: u64,
    pub uptime_ms: u64,
    pub level: u8,
    pub target_len: u8,
    pub message_len: u16,
    pub target: [u8; 48],
    pub message: [u8; 256],
}

impl Default for LogEntry {
    fn default() -> Self {
        Self {
            seq: 0,
            uptime_ms: 0,
            level: LogLevel::Info as u8,
            target_len: 0,
            message_len: 0,
            target: [0; 48],
            message: [0; 256],
        }
    }
}

/// Complete telemetry snapshot published by the running game process.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TelemetrySnapshot {
    // Lifecycle & identity
    pub phase: u32,
    pub exit_code: i32,
    pub is_diagnostics_build: u8,
    pub console_mode: u8,
    pub gpu_upload_heap: u8,
    pub driver_tripped: u8,
    pub start_unix_ms: u64,
    pub uptime_ms: u64,
    pub first_frame_ms: u64,
    pub last_present_uptime_ms: u64,
    pub title_id: u32,
    pub storage_quota_mb: u32,
    pub target_width: u32,
    pub target_height: u32,
    pub capped_width: u32,
    pub capped_height: u32,
    pub wave_min: u32,
    pub wave_max: u32,
    pub missing_imports: u32,
    pub title_dll_count: u32,
    pub boot_percent: u32,

    // Strings (UTF-8, truncated safely at char boundaries)
    pub exe_name_len: u8,
    pub build_label_len: u8,
    pub build_hash_len: u8,
    pub adapter_name_len: u8,
    pub stage_len: u8,
    pub stage_detail_len: u8,
    pub crash_summary_len: u16,
    pub flags_summary_len: u16,
    pub exe_name: [u8; 64],
    pub build_label: [u8; 64],
    pub build_hash: [u8; 24],
    pub adapter_name: [u8; 128],
    pub stage: [u8; 64],
    pub stage_detail: [u8; 128],
    pub crash_summary: [u8; 256],
    pub flags_summary: [u8; 256],

    // FPS & frame timing
    pub total_frames_begun: u64,
    pub total_frames_presented: u64,
    pub fps_1s: f64,
    pub frame_ms_1s: f64,
    pub draws_per_frame: f64,
    pub dropped_per_frame: f64,
    pub indirect_per_frame: f64,
    pub cpu_prepare_ms: f64,
    pub gpu_wait_ms: f64,
    pub copy_up_ms: f64,
    pub copy_up_mb: f64,
    pub tex_refresh_ms: f64,
    pub tex_refresh_mb: f64,
    pub copy_back_ms: f64,
    pub copy_back_mb: f64,
    pub card_idle_ms: f64,
    pub card_idle_behind_ms: f64,
    pub card_busy_pct: f64,
    pub present_show_ms: f64,
    pub pipeline_wait_ms: f64,
    pub pace_ms: f64,
    pub total_draws: u64,
    pub total_dropped: u64,

    // Bindless heap & texture queue
    pub heap_tex_uploaded: u64,
    pub heap_tex_mb: u64,
    pub heap_tex_waiting: u64,
    pub heap_tex_away: u64,
    pub heap_tex_full: u64,
    pub heap_q_total: u64,
    pub heap_q_due: u64,
    pub heap_q_partly: u64,
    pub heap_q_oldest_frames: u64,

    // Pipelines & shaders
    pub pipelines_made: u64,
    pub pipelines_loaded: u64,
    pub pipelines_early: u64,
    pub pipelines_waiting: u64,
    pub pipelines_peak_waiting: u64,
    pub shaders_containers: u64,
    pub shaders_container_bytes: u64,
    pub shaders_translations: u64,
    pub shaders_translation_bytes: u64,
    pub shader_cache_hits: u64,
    pub shader_cache_misses: u64,
    pub pipeline_lib_bytes: u64,
    pub pipeline_lib_saved_bytes: u64,

    // DirectStorage streaming & loading
    pub dstorage_queues: u32,
    pub dstorage_workers: u32,
    pub dstorage_queued_reads: u64,
    pub dstorage_running_reads: u64,
    pub dstorage_held_reads: u64,
    pub dstorage_done_reads: u64,
    pub dstorage_done_bytes: u64,
    pub dstorage_read_mb_s: f64,
    pub dstorage_busy_workers: f64,
    pub dstorage_bcpack_bytes: u64,
    pub dstorage_staged_bytes: u64,
    pub dstorage_staging_limit: u64,
    pub dstorage_staging_waits: u64,

    // GPU VRAM & layer memory
    pub vram_dedicated_budget: u64,
    pub vram_dedicated_used: u64,
    pub vram_dedicated_avail_res: u64,
    pub vram_shared_budget: u64,
    pub vram_shared_used: u64,
    pub host_textures_alive: u64,
    pub host_textures_bytes: u64,
    pub placed_textures: u64,
    pub host_textures_evicted: u64,
    pub host_textures_evicted_bytes: u64,
    pub host_textures_recreated: u64,
    pub shadow_copies_alive: u64,
    pub shadow_copies_bytes: u64,
    pub shadow_windows_alive: u64,
    pub shadow_windows_bytes: u64,
    pub card_buffers_alive: u64,
    pub card_buffers_bytes: u64,
    pub card_before_bytes: u64,
    pub staging_alive: u64,
    pub staging_bytes: u64,
    pub staging_spare_bytes: u64,
    pub host_lists_alive: u64,
    pub host_lists_made: u64,
    pub scratch_bytes: u64,
    pub tiles_alive: u64,
    pub tiles_allocated_bytes: u64,
    pub tiles_saved_bytes: u64,

    // Sync, cores, faults & XMem
    pub driver_errors: u64,
    pub guard_faults_total: u64,
    pub guard_faults_per_sec: f64,
    pub remaps_total: u64,
    pub remaps_per_sec: f64,
    pub remap_wait_ms_per_sec: f64,
    pub render_slow_looks: u64,
    pub render_total_looks: u64,
    pub worker_slow_looks: u64,
    pub worker_total_looks: u64,
    pub record_slow_looks: u64,
    pub record_total_looks: u64,
    pub xmem_title_limit_bytes: u64,
    pub xmem_physical_pages_used: u64,
    pub xmem_physical_bytes_mapped: u64,
}

impl Default for TelemetrySnapshot {
    fn default() -> Self {
        // SAFETY: `TelemetrySnapshot` is plain-old-data (`#[repr(C)]` integers, floats, byte arrays).
        unsafe { std::mem::zeroed() }
    }
}

/// Shared memory layout mapped under `Local\victory-telemetry-<pid>`.
#[repr(C)]
pub struct SharedTelemetry {
    pub magic: u64,
    pub version: u32,
    pub struct_size: u32,
    pub pid: u32,
    pub active_slot: AtomicU32,
    pub seq: AtomicU64,
    pub frame_seq: AtomicU64,
    pub frame_times_us: [AtomicU32; FRAME_RING_LEN],
    pub slots: [TelemetrySnapshot; 2],
    pub log_seq: AtomicU64,
    pub log_entry_seq: [AtomicU64; LOG_RING_LEN],
    pub log_entries: [LogEntry; LOG_RING_LEN],
}

/// Writes UTF-8 string into fixed buffer without splitting multi-byte codepoints.
pub fn write_str(buf: &mut [u8], len_out: &mut u8, text: &str) {
    let bytes = cut_utf8(text, buf.len());
    buf[..bytes.len()].copy_from_slice(bytes);
    *len_out = bytes.len() as u8;
}

/// Writes UTF-8 string into larger fixed buffer (`u16` length).
pub fn write_str_u16(buf: &mut [u8], len_out: &mut u16, text: &str) {
    let bytes = cut_utf8(text, buf.len());
    buf[..bytes.len()].copy_from_slice(bytes);
    *len_out = bytes.len() as u16;
}

/// Reads UTF-8 string from fixed buffer.
pub fn read_str(buf: &[u8], len: usize) -> &str {
    let n = len.min(buf.len());
    std::str::from_utf8(&buf[..n]).unwrap_or("")
}

fn cut_utf8(text: &str, max_bytes: usize) -> &[u8] {
    if text.len() <= max_bytes {
        return text.as_bytes();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text.as_bytes()[..end]
}

pub fn shm_name(pid: u32) -> String {
    format!("Local\\victory-telemetry-{pid}")
}

static ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
        MapViewOfFile, OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    struct Mapping {
        _handle: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
    }

    // SAFETY: `SharedTelemetry` is synchronized via atomics and publisher mutex.
    unsafe impl Send for Mapping {}
    unsafe impl Sync for Mapping {}

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: valid view mapped in `create`/`open`.
            unsafe {
                UnmapViewOfFile(self.view);
                if !self._handle.is_null() {
                    CloseHandle(self._handle);
                }
            }
        }
    }

    struct Publisher {
        mapping: Mapping,
        started: Instant,
        last_present: Mutex<Option<Instant>>,
        local: Mutex<TelemetrySnapshot>,
        log_lock: Mutex<()>,
    }

    impl Publisher {
        fn shm(&self) -> &SharedTelemetry {
            // SAFETY: view is mapped for at least `size_of::<SharedTelemetry>()` bytes.
            unsafe { &*(self.mapping.view.Value as *const SharedTelemetry) }
        }

        fn shm_ptr(&self) -> *mut SharedTelemetry {
            self.mapping.view.Value.cast()
        }

        fn commit_locked(&self, snap: &mut TelemetrySnapshot) {
            snap.uptime_ms = self.started.elapsed().as_millis() as u64;
            let shm = self.shm();
            let cur = shm.active_slot.load(Ordering::Relaxed);
            let next = 1 - (cur & 1);
            // SAFETY: `p.local` serializes slot writes, and we only write the inactive slot `next`.
            unsafe {
                let slot_ptr = std::ptr::addr_of_mut!((*self.shm_ptr()).slots[next as usize]);
                slot_ptr.write(*snap);
            }
            std::sync::atomic::fence(Ordering::Release);
            shm.active_slot.store(next, Ordering::Release);
            shm.seq.fetch_add(1, Ordering::Release);
        }
    }

    static PUBLISHER: OnceLock<Option<Publisher>> = OnceLock::new();

    fn wide_nul(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn init_publisher() -> Option<u32> {
        let pid = unsafe { GetCurrentProcessId() };
        let ok = PUBLISHER
            .get_or_init(|| {
                let name = wide_nul(&shm_name(pid));
                let bytes = size_of::<SharedTelemetry>();
                // SAFETY: creates a pagefile-backed named section for this process.
                let handle = unsafe {
                    CreateFileMappingW(
                        INVALID_HANDLE_VALUE,
                        std::ptr::null(),
                        PAGE_READWRITE,
                        (bytes >> 32) as u32,
                        bytes as u32,
                        name.as_ptr(),
                    )
                };
                if handle.is_null() {
                    return None;
                }
                // SAFETY: `handle` is valid and `bytes` is `size_of::<SharedTelemetry>()`.
                let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, bytes) };
                if view.Value.is_null() {
                    // SAFETY: `handle` was opened above.
                    unsafe { CloseHandle(handle) };
                    return None;
                }
                // SAFETY: zero-initialize newly created mapping before exposing magic/version.
                unsafe { std::ptr::write_bytes(view.Value.cast::<u8>(), 0, bytes) };
                let now_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64);
                let mut init = TelemetrySnapshot {
                    phase: Phase::Boot as u32,
                    start_unix_ms: now_unix,
                    is_diagnostics_build: u8::from(cfg!(feature = "diagnostics")),
                    ..Default::default()
                };
                write_str(&mut init.stage, &mut init.stage_len, "Starting VIctory...");

                // SAFETY: initialized memory inside mapped region.
                let shm = unsafe { &mut *(view.Value as *mut SharedTelemetry) };
                shm.pid = pid;
                shm.version = TELEMETRY_VERSION;
                shm.struct_size = size_of::<TelemetrySnapshot>() as u32;
                shm.slots[0] = init;
                shm.slots[1] = init;
                std::sync::atomic::fence(Ordering::Release);
                shm.magic = TELEMETRY_MAGIC;

                ACTIVE.store(true, Ordering::Release);
                Some(Publisher {
                    mapping: Mapping {
                        _handle: handle,
                        view,
                    },
                    started: Instant::now(),
                    last_present: Mutex::new(None),
                    local: Mutex::new(init),
                    log_lock: Mutex::new(()),
                })
            })
            .is_some();
        ok.then_some(pid)
    }

    pub fn update(f: impl FnOnce(&mut TelemetrySnapshot)) {
        let Some(Some(p)) = PUBLISHER.get() else {
            return;
        };
        let mut snap = p.local.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut snap);
        p.commit_locked(&mut snap);
    }

    pub fn set_stage(phase: Phase, stage: &str, detail: &str, percent: u32) {
        update(|s| {
            if (phase as u32) >= s.phase || matches!(phase, Phase::Exited | Phase::Crashed) {
                s.phase = phase as u32;
            }
            write_str(&mut s.stage, &mut s.stage_len, stage);
            write_str(&mut s.stage_detail, &mut s.stage_detail_len, detail);
            s.boot_percent = percent.min(100);
        });
    }

    pub fn record_frame(presented_total: u64, begun_total: u64, width: u32, height: u32) {
        let Some(Some(p)) = PUBLISHER.get() else {
            return;
        };
        let now = Instant::now();
        let dt_us = {
            let mut prev = p.last_present.lock().unwrap_or_else(|e| e.into_inner());
            let dt = prev.map_or(0, |then| now.saturating_duration_since(then).as_micros() as u64);
            *prev = Some(now);
            dt.min(u64::from(u32::MAX)) as u32
        };
        let shm = p.shm();
        if dt_us > 0 {
            let idx = shm.frame_seq.fetch_add(1, Ordering::Relaxed);
            shm.frame_times_us[(idx as usize) % FRAME_RING_LEN].store(dt_us, Ordering::Release);
        }
        let uptime = p.started.elapsed().as_millis() as u64;
        if let Ok(mut snap) = p.local.try_lock() {
            if snap.phase < Phase::Running as u32 {
                snap.phase = Phase::Running as u32;
                snap.boot_percent = 100;
            }
            if snap.first_frame_ms == 0 {
                snap.first_frame_ms = uptime;
            }
            snap.last_present_uptime_ms = uptime;
            snap.total_frames_presented = presented_total;
            snap.total_frames_begun = begun_total;
            if width > 0 && height > 0 {
                snap.target_width = width;
                snap.target_height = height;
            }
            p.commit_locked(&mut snap);
        }
    }

    pub fn log_event(level: LogLevel, target: &str, message: &str) {
        let Some(Some(p)) = PUBLISHER.get() else {
            return;
        };
        let Ok(_guard) = p.log_lock.try_lock() else {
            return;
        };
        let uptime_ms = p.started.elapsed().as_millis() as u64;
        let shm = p.shm();
        let seq = shm.log_seq.fetch_add(1, Ordering::AcqRel) + 1;
        let slot = ((seq - 1) as usize) % LOG_RING_LEN;
        shm.log_entry_seq[slot].store(0, Ordering::Release);
        let mut entry = LogEntry {
            seq,
            uptime_ms,
            level: level as u8,
            ..Default::default()
        };
        write_str(&mut entry.target, &mut entry.target_len, target);
        write_str_u16(&mut entry.message, &mut entry.message_len, message);
        // SAFETY: `log_lock` serializes writes into `log_entries[slot]` while `log_entry_seq[slot] == 0`.
        unsafe {
            let entry_ptr = std::ptr::addr_of_mut!((*p.shm_ptr()).log_entries[slot]);
            entry_ptr.write(entry);
        }
        std::sync::atomic::fence(Ordering::Release);
        shm.log_entry_seq[slot].store(seq, Ordering::Release);
    }

    pub fn mark_exited(code: i32) {
        let Some(Some(p)) = PUBLISHER.get() else {
            return;
        };
        let mut snap = p.local.lock().unwrap_or_else(|e| e.into_inner());
        if snap.phase != Phase::Crashed as u32 {
            snap.phase = if code == 0 {
                Phase::Exited as u32
            } else {
                Phase::Crashed as u32
            };
            snap.exit_code = code;
        }
        p.commit_locked(&mut snap);
    }

    pub fn mark_crashed(summary: &str) {
        let Some(Some(p)) = PUBLISHER.get() else {
            return;
        };
        log_event(LogLevel::Error, "crash", summary);
        let mut snap = p.local.lock().unwrap_or_else(|e| e.into_inner());
        let snap = &mut *snap;
        snap.phase = Phase::Crashed as u32;
        if snap.exit_code == 0 {
            snap.exit_code = -1;
        }
        if snap.crash_summary_len == 0 {
            write_str_u16(&mut snap.crash_summary, &mut snap.crash_summary_len, summary);
        }
        p.commit_locked(snap);
    }

    /// Read-only view opened by the companion debug monitor process.
    pub struct TelemetryReader {
        mapping: Mapping,
        last_frame_seq: u64,
        last_log_seq: u64,
    }

    impl TelemetryReader {
        pub fn open(pid: u32) -> Option<Self> {
            let name = wide_nul(&shm_name(pid));
            // SAFETY: opens existing shared mapping by name.
            let handle = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()) };
            if handle.is_null() {
                return None;
            }
            let bytes = size_of::<SharedTelemetry>();
            // SAFETY: maps read-only view of the telemetry section.
            let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, bytes) };
            if view.Value.is_null() {
                // SAFETY: handle opened above.
                unsafe { CloseHandle(handle) };
                return None;
            }
            // SAFETY: mapped for `bytes`.
            let shm = unsafe { &*(view.Value as *const SharedTelemetry) };
            if shm.magic != TELEMETRY_MAGIC
                || shm.version != TELEMETRY_VERSION
                || shm.struct_size != size_of::<TelemetrySnapshot>() as u32
            {
                drop(Mapping {
                    _handle: handle,
                    view,
                });
                return None;
            }
            Some(Self {
                mapping: Mapping {
                    _handle: handle,
                    view,
                },
                last_frame_seq: 0,
                last_log_seq: 0,
            })
        }

        fn shm(&self) -> &SharedTelemetry {
            // SAFETY: view stays valid for the lifetime of `self.mapping`.
            unsafe { &*(self.mapping.view.Value as *const SharedTelemetry) }
        }

        pub fn snapshot(&self) -> TelemetrySnapshot {
            let shm = self.shm();
            let slot = shm.active_slot.load(Ordering::Acquire) & 1;
            shm.slots[slot as usize]
        }

        /// Drains newly recorded frame times (in microseconds) since the last call.
        pub fn drain_frames(&mut self) -> Vec<u32> {
            let shm = self.shm();
            let head = shm.frame_seq.load(Ordering::Acquire);
            if head <= self.last_frame_seq {
                return Vec::new();
            }
            let start = self.last_frame_seq.max(head.saturating_sub(FRAME_RING_LEN as u64));
            let mut out = Vec::with_capacity((head - start) as usize);
            for seq in start..head {
                let v = shm.frame_times_us[(seq as usize) % FRAME_RING_LEN].load(Ordering::Acquire);
                if v > 0 {
                    out.push(v);
                }
            }
            self.last_frame_seq = head;
            out
        }

        /// Drains newly recorded log entries since the last call.
        pub fn drain_logs(&mut self) -> Vec<LogEntry> {
            let shm = self.shm();
            let head = shm.log_seq.load(Ordering::Acquire);
            if head <= self.last_log_seq {
                return Vec::new();
            }
            let start = self.last_log_seq.max(head.saturating_sub(LOG_RING_LEN as u64));
            let mut out = Vec::with_capacity((head - start) as usize);
            for seq in (start + 1)..=head {
                let slot = ((seq - 1) as usize) % LOG_RING_LEN;
                if shm.log_entry_seq[slot].load(Ordering::Acquire) == seq {
                    let entry = shm.log_entries[slot];
                    if entry.seq == seq {
                        out.push(entry);
                    }
                }
            }
            self.last_log_seq = head;
            out
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub fn init_publisher() -> Option<u32> {
        None
    }
    pub fn update(_f: impl FnOnce(&mut TelemetrySnapshot)) {}
    pub fn set_stage(_phase: Phase, _stage: &str, _detail: &str, _percent: u32) {}
    pub fn record_frame(_presented_total: u64, _begun_total: u64, _width: u32, _height: u32) {}
    pub fn log_event(_level: LogLevel, _target: &str, _message: &str) {}
    pub fn mark_exited(_code: i32) {}
    pub fn mark_crashed(_summary: &str) {}
    pub struct TelemetryReader;
    impl TelemetryReader {
        pub fn open(_pid: u32) -> Option<Self> {
            None
        }
        pub fn snapshot(&self) -> TelemetrySnapshot {
            TelemetrySnapshot::default()
        }
        pub fn drain_frames(&mut self) -> Vec<u32> {
            Vec::new()
        }
        pub fn drain_logs(&mut self) -> Vec<LogEntry> {
            Vec::new()
        }
    }
}

pub use platform::{
    TelemetryReader, init_publisher, log_event, mark_crashed, mark_exited, record_frame, set_stage,
    update,
};

struct MsgVisitor {
    message: String,
    fields: String,
}

impl tracing::field::Visit for MsgVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            use std::fmt::Write;
            let _ = write!(self.fields, "{}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            use std::fmt::Write;
            let _ = write!(self.fields, "{}={value}", field.name());
        }
    }
}

/// Tracing subscriber layer that forwards warnings, errors, and high-signal
/// lifecycle/performance milestones into the shared telemetry log ring.
pub struct TelemetryLogLayer;

pub fn log_layer() -> TelemetryLogLayer {
    TelemetryLogLayer
}

fn should_forward(level: &Level, target: &str, message: &str) -> bool {
    if *level == Level::ERROR || *level == Level::WARN {
        return true;
    }
    if *level != Level::INFO {
        return false;
    }
    if matches!(
        target,
        "victory_gpu::pipeline" | "victory_gpu::pipeline::host" | "victory_gpu::framelog"
    ) {
        return false;
    }
    if target == "victory_hle::modules::dstorage"
        && (message.starts_with("dstorage: status array")
            || message.starts_with("dstorage: queue")
            || message.starts_with("dstorage: file opened"))
    {
        return false;
    }
    if target == "victory_gpu::host"
        && (message.contains("indirect mesh dispatches")
            || message.contains("pass over the heap")
            || message.contains("card's clock per frame")
            || message.contains("title buffers' host copies")
            || message.contains("placed texture views")
            || message.contains("copies left out")
            || message.contains("our passes over copies")
            || message.contains("texture refreshes")
            || message.contains("title buffer copies"))
    {
        return false;
    }
    true
}

impl<S: tracing::Subscriber> Layer<S> for TelemetryLogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if !is_active() {
            return;
        }
        let meta = event.metadata();
        let level = meta.level();
        if *level > Level::INFO {
            return;
        }
        let mut visitor = MsgVisitor {
            message: String::new(),
            fields: String::new(),
        };
        event.record(&mut visitor);
        if !should_forward(level, meta.target(), &visitor.message) {
            return;
        }
        let lvl = match *level {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            _ => LogLevel::Info,
        };
        let combined = if visitor.fields.is_empty() {
            visitor.message
        } else if visitor.message.is_empty() {
            visitor.fields
        } else {
            format!("{} | {}", visitor.message, visitor.fields)
        };
        let short_target = meta
            .target()
            .strip_prefix("victory_")
            .unwrap_or(meta.target());
        log_event(lvl, short_target, &combined);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_truncation_keeps_valid_boundaries() {
        let mut buf = [0u8; 8];
        let mut len = 0u8;
        // 'é' is 2 bytes, '🦀' is 4 bytes -> "abcé🦀" = 3 + 2 + 4 = 9 bytes.
        write_str(&mut buf, &mut len, "abcé🦀");
        assert_eq!(len, 5);
        assert_eq!(read_str(&buf, len as usize), "abcé");
    }

    #[test]
    fn ping_pong_and_rings_work_across_reader() {
        let Some(pid) = init_publisher() else {
            return;
        };
        update(|s| {
            s.title_id = 0x7952_987A;
            write_str(&mut s.exe_name, &mut s.exe_name_len, "test_game.exe");
        });
        record_frame(1, 1, 1920, 1080);
        std::thread::sleep(std::time::Duration::from_millis(2));
        record_frame(2, 2, 1920, 1080);
        log_event(LogLevel::Warn, "gpu", "sample warning");

        let mut reader = TelemetryReader::open(pid).expect("open telemetry");
        let snap = reader.snapshot();
        assert_eq!(snap.title_id, 0x7952_987A);
        assert_eq!(read_str(&snap.exe_name, snap.exe_name_len as usize), "test_game.exe");
        assert_eq!(snap.total_frames_presented, 2);

        let frames = reader.drain_frames();
        assert!(!frames.is_empty());
        let logs = reader.drain_logs();
        assert!(logs.iter().any(|l| {
            read_str(&l.target, l.target_len as usize) == "gpu"
                && read_str(&l.message, l.message_len as usize) == "sample warning"
        }));
    }
}
