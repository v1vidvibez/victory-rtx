//! Companion Win32 debug & telemetry monitor launched alongside `victory run --execute`.
//!
//! Displays real-time loading & shader/pipeline compilation progress, FPS & frame-time
//! graphs (including 1% low and per-stage frame budget), DirectStorage streaming I/O,
//! system & per-core CPU utilization, system RAM & commit, DXGI dedicated/shared VRAM,
//! internal VIctory memory pools, driver/guard fault health, and live log events.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use anyhow::Result;
use victory_core::telemetry::{LogEntry, LogLevel, Phase, TelemetryReader, TelemetrySnapshot, read_str};
use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WAIT_OBJECT_0, WPARAM,
};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    BLACK_BRUSH, BeginPaint, BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen, CreateSolidBrush,
    DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT,
    DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, EndPaint, FF_DONTCARE, FW_BOLD,
    FW_NORMAL, FW_SEMIBOLD, FillRect, GetStockObject, HDC, HFONT, InvalidateRect, LineTo,
    MoveToEx, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_DASH, PS_SOLID, Polyline, RoundRect, SRCCOPY,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX, PROCESS_MEMORY_COUNTERS_EX2,
};
use windows_sys::Win32::System::SystemInformation::{
    GetSystemCpuSetInformation, GlobalMemoryStatusEx, MEMORYSTATUSEX, SYSTEM_CPU_SET_INFORMATION,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessIoCounters, GetProcessTimes, GetSystemTimes,
    IO_COUNTERS, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_VM_READ, WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemProcessorPerformanceInformation,
};

const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 200;
const DWMWA_USE_IMMUTABLE_DARK_MODE: u32 = 20;
const CF_UNICODETEXT: u32 = 13;

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

mod palette {
    use super::rgb;
    pub const BG: u32 = rgb(13, 17, 23);
    pub const HEADER: u32 = rgb(20, 25, 34);
    pub const CARD: u32 = rgb(22, 27, 34);
    pub const CARD_ALT: u32 = rgb(28, 34, 44);
    pub const BORDER: u32 = rgb(44, 52, 64);
    pub const TEXT: u32 = rgb(230, 237, 243);
    pub const TEXT_DIM: u32 = rgb(139, 148, 158);
    pub const TEXT_MUTED: u32 = rgb(100, 110, 122);
    pub const BLUE: u32 = rgb(59, 130, 246);
    pub const CYAN: u32 = rgb(34, 211, 238);
    pub const GREEN: u32 = rgb(34, 197, 94);
    pub const AMBER: u32 = rgb(245, 158, 11);
    pub const RED: u32 = rgb(239, 68, 68);
    pub const PURPLE: u32 = rgb(168, 85, 247);
    pub const TRACK: u32 = rgb(36, 43, 55);
    pub const GRID: u32 = rgb(33, 40, 50);
    pub const BTN_ACTIVE: u32 = rgb(42, 74, 124);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview = 0,
    GpuMem = 1,
    LoadingCpu = 2,
    LogsEnv = 3,
}

#[derive(Clone, Copy)]
enum Action {
    Tab(Tab),
    TogglePause,
    ToggleTopmost,
    ToggleKeepOpen,
    CopyReport,
}

#[derive(Clone, Copy)]
struct HitRect {
    rect: RECT,
    action: Action,
}

#[derive(Clone, Default)]
struct CoreTopology {
    fast_mask: Vec<bool>,
    has_hybrid: bool,
}

#[derive(Clone, Default)]
struct OsSample {
    sys_cpu_pct: f64,
    proc_cpu_pct: f64,
    proc_cores_equiv: f64,
    logical_cores: usize,
    core_pcts: Vec<f32>,
    ram_used_bytes: u64,
    ram_total_bytes: u64,
    ram_avail_bytes: u64,
    ram_load_pct: u32,
    commit_used_bytes: u64,
    commit_limit_bytes: u64,
    proc_ws_bytes: u64,
    proc_peak_ws_bytes: u64,
    proc_private_bytes: u64,
    proc_priv_ws_bytes: u64,
    page_faults_per_sec: f64,
    io_read_mb_s: f64,
    io_write_mb_s: f64,
    thread_count: u32,
}

struct OsSampler {
    pid: u32,
    proc_handle: HANDLE,
    alive: bool,
    exit_code: Option<i32>,
    topology: CoreTopology,
    prev_sys: Option<(u64, u64, u64)>,
    prev_proc: Option<(Instant, u64)>,
    prev_cores: Vec<(i64, i64, i64)>,
    prev_faults: Option<(Instant, u32)>,
    prev_io: Option<(Instant, u64, u64)>,
    last_threads_at: Option<Instant>,
    cached_threads: u32,
    latest: OsSample,
}

impl Drop for OsSampler {
    fn drop(&mut self) {
        if !self.proc_handle.is_null() {
            // SAFETY: handle opened in `OsSampler::new`.
            unsafe { CloseHandle(self.proc_handle) };
        }
    }
}

fn ft_u64(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

fn query_core_topology() -> CoreTopology {
    let mut bytes = 0u32;
    // SAFETY: size query for current process CPU sets.
    unsafe {
        GetSystemCpuSetInformation(
            std::ptr::null_mut(),
            0,
            &mut bytes,
            GetCurrentProcess(),
            0,
        );
    }
    if bytes == 0 {
        return CoreTopology::default();
    }
    let mut buf = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: `buf` holds `bytes` bytes aligned for `SYSTEM_CPU_SET_INFORMATION`.
    let ok = unsafe {
        GetSystemCpuSetInformation(
            buf.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
            GetCurrentProcess(),
            0,
        )
    };
    if ok == 0 {
        return CoreTopology::default();
    }
    let mut pairs = Vec::new();
    let mut at = 0usize;
    while at + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes as usize {
        // SAFETY: points inside initialized `buf` up to `bytes`.
        let info = unsafe { &*(buf.as_ptr().cast::<u8>().add(at).cast::<SYSTEM_CPU_SET_INFORMATION>()) };
        if info.Size == 0 {
            break;
        }
        // SAFETY: CpuSet variant of union.
        let set = unsafe { &info.Anonymous.CpuSet };
        if set.Group == 0 {
            pairs.push((set.LogicalProcessorIndex as usize, set.EfficiencyClass));
        }
        at += info.Size as usize;
    }
    let max_idx = pairs.iter().map(|&(i, _)| i + 1).max().unwrap_or(0);
    let best = pairs.iter().map(|&(_, c)| c).max().unwrap_or(0);
    let min = pairs.iter().map(|&(_, c)| c).min().unwrap_or(0);
    let mut fast_mask = vec![true; max_idx];
    for (idx, eff) in pairs {
        if idx < fast_mask.len() {
            fast_mask[idx] = eff == best;
        }
    }
    CoreTopology {
        fast_mask,
        has_hybrid: best > min,
    }
}

fn count_process_threads(pid: u32) -> u32 {
    let mut count = 0u32;
    // SAFETY: snapshot handle closed below.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return 0;
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut more = Thread32First(snap, &mut entry) != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                count += 1;
            }
            more = Thread32Next(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
    }
    count
}

impl OsSampler {
    fn new(pid: u32) -> Self {
        // SAFETY: opens target process for querying CPU/RAM/I/O and exit status.
        let mut handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            // SAFETY: fallback without `PROCESS_VM_READ`.
            handle = unsafe {
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid)
            };
        }
        let topology = query_core_topology();
        let logical_cores = topology
            .fast_mask
            .len()
            .max(std::thread::available_parallelism().map_or(1, |n| n.get()));
        Self {
            pid,
            proc_handle: handle,
            alive: !handle.is_null(),
            exit_code: None,
            topology,
            prev_sys: None,
            prev_proc: None,
            prev_cores: Vec::new(),
            prev_faults: None,
            prev_io: None,
            last_threads_at: None,
            cached_threads: 0,
            latest: OsSample {
                logical_cores,
                ..Default::default()
            },
        }
    }

    fn poll(&mut self) -> &OsSample {
        let now = Instant::now();
        if !self.proc_handle.is_null() && self.alive {
            // SAFETY: valid process handle.
            let signaled = unsafe { WaitForSingleObject(self.proc_handle, 0) == WAIT_OBJECT_0 };
            if signaled {
                let mut code = 0u32;
                // SAFETY: valid process handle and out pointer.
                if unsafe { GetExitCodeProcess(self.proc_handle, &mut code) } != 0 {
                    self.exit_code = Some(code as i32);
                }
                self.alive = false;
            }
        }

        // System CPU
        let (mut idle_ft, mut kernel_ft, mut user_ft) = unsafe { std::mem::zeroed() };
        // SAFETY: valid out pointers.
        if unsafe { GetSystemTimes(&mut idle_ft, &mut kernel_ft, &mut user_ft) } != 0 {
            let (i, k, u) = (ft_u64(idle_ft), ft_u64(kernel_ft), ft_u64(user_ft));
            if let Some((pi, pk, pu)) = self.prev_sys {
                let di = i.saturating_sub(pi);
                let dk = k.saturating_sub(pk);
                let du = u.saturating_sub(pu);
                let total = dk.saturating_add(du);
                if total > 0 {
                    let busy = dk.saturating_sub(di).saturating_add(du);
                    self.latest.sys_cpu_pct = ((busy as f64 / total as f64) * 100.0).clamp(0.0, 100.0);
                }
            }
            self.prev_sys = Some((i, k, u));
        }

        // Per-core CPU
        let n_cores = self.latest.logical_cores.max(1);
        let mut perf = vec![SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION::default(); n_cores];
        let mut ret_len = 0u32;
        // SAFETY: queries kernel per-logical-processor cumulative times.
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessorPerformanceInformation,
                perf.as_mut_ptr().cast(),
                (perf.len() * size_of::<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>()) as u32,
                &mut ret_len,
            )
        };
        if status >= 0 {
            let actual = (ret_len as usize / size_of::<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>()).min(perf.len());
            if actual > 0 {
                self.latest.logical_cores = actual;
                if self.prev_cores.len() == actual {
                    self.latest.core_pcts.resize(actual, 0.0);
                    for (idx, info) in perf[..actual].iter().enumerate() {
                        let (pi, pk, pu) = self.prev_cores[idx];
                        let di = info.IdleTime.saturating_sub(pi) as f64;
                        let dk = info.KernelTime.saturating_sub(pk) as f64;
                        let du = info.UserTime.saturating_sub(pu) as f64;
                        let total = dk + du;
                        let pct = if total > 0.0 {
                            (((dk - di).max(0.0) + du) / total * 100.0).clamp(0.0, 100.0)
                        } else {
                            0.0
                        };
                        self.latest.core_pcts[idx] = pct as f32;
                    }
                }
                self.prev_cores = perf[..actual]
                    .iter()
                    .map(|p| (p.IdleTime, p.KernelTime, p.UserTime))
                    .collect();
            }
        }

        // System RAM & Commit
        // SAFETY: plain struct filled by GlobalMemoryStatusEx.
        let mut mem: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        mem.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: sized MEMORYSTATUSEX.
        if unsafe { GlobalMemoryStatusEx(&mut mem) } != 0 {
            self.latest.ram_total_bytes = mem.ullTotalPhys;
            self.latest.ram_avail_bytes = mem.ullAvailPhys;
            self.latest.ram_used_bytes = mem.ullTotalPhys.saturating_sub(mem.ullAvailPhys);
            self.latest.ram_load_pct = mem.dwMemoryLoad;
            self.latest.commit_limit_bytes = mem.ullTotalPageFile;
            self.latest.commit_used_bytes = mem.ullTotalPageFile.saturating_sub(mem.ullAvailPageFile);
        }

        if !self.proc_handle.is_null() {
            // Process CPU
            let (mut c, mut e, mut k, mut u) = unsafe { std::mem::zeroed() };
            // SAFETY: valid handle and FILETIME outputs.
            if unsafe { GetProcessTimes(self.proc_handle, &mut c, &mut e, &mut k, &mut u) } != 0 {
                let cpu_100ns = ft_u64(k).saturating_add(ft_u64(u));
                if let Some((prev_t, prev_cpu)) = self.prev_proc {
                    let wall_100ns = now.duration_since(prev_t).as_secs_f64() * 1e7;
                    if wall_100ns > 1.0 {
                        let dcpu = cpu_100ns.saturating_sub(prev_cpu) as f64;
                        let cores = (dcpu / wall_100ns).max(0.0);
                        self.latest.proc_cores_equiv = cores;
                        self.latest.proc_cpu_pct =
                            (cores / self.latest.logical_cores.max(1) as f64 * 100.0).clamp(0.0, 100.0);
                    }
                }
                self.prev_proc = Some((now, cpu_100ns));
            }

            // Process memory & page faults
            let mut pmc2 = PROCESS_MEMORY_COUNTERS_EX2 {
                cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
                ..Default::default()
            };
            // SAFETY: valid handle and sized `PROCESS_MEMORY_COUNTERS_EX2`.
            let got_ex2 = unsafe {
                K32GetProcessMemoryInfo(self.proc_handle, (&raw mut pmc2).cast(), pmc2.cb)
            } != 0;
            let (ws, peak_ws, priv_b, priv_ws, faults) = if got_ex2 {
                (
                    pmc2.WorkingSetSize as u64,
                    pmc2.PeakWorkingSetSize as u64,
                    pmc2.PrivateUsage as u64,
                    pmc2.PrivateWorkingSetSize as u64,
                    pmc2.PageFaultCount,
                )
            } else {
                let mut pmc = PROCESS_MEMORY_COUNTERS_EX {
                    cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
                    ..Default::default()
                };
                // SAFETY: valid handle and sized `PROCESS_MEMORY_COUNTERS_EX`.
                let ok = unsafe {
                    K32GetProcessMemoryInfo(self.proc_handle, (&raw mut pmc).cast(), pmc.cb)
                } != 0;
                if ok {
                    (
                        pmc.WorkingSetSize as u64,
                        pmc.PeakWorkingSetSize as u64,
                        pmc.PrivateUsage as u64,
                        0,
                        pmc.PageFaultCount,
                    )
                } else {
                    (0, 0, 0, 0, 0)
                }
            };
            if ws > 0 {
                self.latest.proc_ws_bytes = ws;
                self.latest.proc_peak_ws_bytes = peak_ws;
                self.latest.proc_private_bytes = priv_b;
                self.latest.proc_priv_ws_bytes = priv_ws;
                if let Some((prev_t, prev_f)) = self.prev_faults {
                    let dt = now.duration_since(prev_t).as_secs_f64();
                    if dt > 0.05 {
                        self.latest.page_faults_per_sec =
                            faults.saturating_sub(prev_f) as f64 / dt;
                    }
                }
                self.prev_faults = Some((now, faults));
            }

            // Process I/O
            // SAFETY: plain struct filled by `GetProcessIoCounters`.
            let mut io: IO_COUNTERS = unsafe { std::mem::zeroed() };
            // SAFETY: valid handle and out struct.
            if unsafe { GetProcessIoCounters(self.proc_handle, &mut io) } != 0 {
                if let Some((prev_t, pr, pw)) = self.prev_io {
                    let dt = now.duration_since(prev_t).as_secs_f64();
                    if dt > 0.05 {
                        self.latest.io_read_mb_s =
                            (io.ReadTransferCount.saturating_sub(pr) as f64 / 1e6) / dt;
                        self.latest.io_write_mb_s =
                            (io.WriteTransferCount.saturating_sub(pw) as f64 / 1e6) / dt;
                    }
                }
                self.prev_io = Some((now, io.ReadTransferCount, io.WriteTransferCount));
            }

            // Thread count (once a second)
            if self.alive
                && self
                    .last_threads_at
                    .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1))
            {
                let tc = count_process_threads(self.pid);
                if tc > 0 {
                    self.cached_threads = tc;
                }
                self.last_threads_at = Some(now);
            }
            self.latest.thread_count = self.cached_threads;
        }

        &self.latest
    }
}

#[derive(Clone, Copy, Default)]
struct FrameStats {
    min_ms: f64,
    med_ms: f64,
    avg_ms: f64,
    p95_ms: f64,
    max_ms: f64,
    one_pct_low_fps: f64,
    rolling_fps: f64,
}

fn compute_frame_stats(frames_us: &VecDeque<u32>) -> FrameStats {
    if frames_us.is_empty() {
        return FrameStats::default();
    }
    let mut sorted: Vec<u32> = frames_us.iter().copied().collect();
    sorted.sort_unstable();
    let n = sorted.len();
    let sum: u64 = sorted.iter().map(|&v| u64::from(v)).sum();
    let avg_us = sum as f64 / n as f64;
    let p95 = sorted[((n * 95) / 100).min(n - 1)];
    let p99 = sorted[((n * 99) / 100).min(n - 1)];
    FrameStats {
        min_ms: f64::from(sorted[0]) / 1000.0,
        med_ms: f64::from(sorted[n / 2]) / 1000.0,
        avg_ms: avg_us / 1000.0,
        p95_ms: f64::from(p95) / 1000.0,
        max_ms: f64::from(sorted[n - 1]) / 1000.0,
        one_pct_low_fps: if p99 > 0 { 1_000_000.0 / f64::from(p99) } else { 0.0 },
        rolling_fps: if avg_us > 0.0 { 1_000_000.0 / avg_us } else { 0.0 },
    }
}

struct MonitorState {
    pid: u32,
    reader: Option<TelemetryReader>,
    sampler: OsSampler,
    snap: TelemetrySnapshot,
    os: OsSample,
    frame_ring: VecDeque<u32>,
    frame_stats: FrameStats,
    fps_hist: VecDeque<f32>,
    sys_cpu_hist: VecDeque<f32>,
    proc_cpu_hist: VecDeque<f32>,
    ram_hist: VecDeque<f32>,
    vram_hist: VecDeque<f32>,
    gpu_busy_hist: VecDeque<f32>,
    logs: VecDeque<LogEntry>,
    active_tab: Tab,
    paused: bool,
    topmost: bool,
    keep_open: bool,
    copied_until: Option<Instant>,
    exited_at: Option<Instant>,
    log_scroll: usize,
    hits: Vec<HitRect>,
    log_panel_rect: RECT,
    last_title: String,
    dpi: u32,
}

impl MonitorState {
    fn new(pid: u32) -> Self {
        let reader = TelemetryReader::open(pid);
        let snap = reader.as_ref().map_or_else(TelemetrySnapshot::default, TelemetryReader::snapshot);
        Self {
            pid,
            reader,
            sampler: OsSampler::new(pid),
            snap,
            os: OsSample::default(),
            frame_ring: VecDeque::with_capacity(240),
            frame_stats: FrameStats::default(),
            fps_hist: VecDeque::with_capacity(120),
            sys_cpu_hist: VecDeque::with_capacity(120),
            proc_cpu_hist: VecDeque::with_capacity(120),
            ram_hist: VecDeque::with_capacity(120),
            vram_hist: VecDeque::with_capacity(120),
            gpu_busy_hist: VecDeque::with_capacity(120),
            logs: VecDeque::with_capacity(160),
            active_tab: Tab::Overview,
            paused: false,
            topmost: false,
            keep_open: false,
            copied_until: None,
            exited_at: None,
            log_scroll: 0,
            hits: Vec::new(),
            log_panel_rect: RECT::default(),
            last_title: String::new(),
            dpi: 96,
        }
    }

    fn push_hist(q: &mut VecDeque<f32>, v: f32) {
        if q.len() >= 120 {
            q.pop_front();
        }
        q.push_back(v);
    }

    fn tick(&mut self, hwnd: HWND) {
        if self.reader.is_none() && self.sampler.alive {
            self.reader = TelemetryReader::open(self.pid);
        }

        let was_alive = self.sampler.alive;
        self.os = self.sampler.poll().clone();

        if !self.paused {
            if let Some(reader) = &mut self.reader {
                self.snap = reader.snapshot();
                for us in reader.drain_frames() {
                    if self.frame_ring.len() >= 200 {
                        self.frame_ring.pop_front();
                    }
                    self.frame_ring.push_back(us);
                }
                self.frame_stats = compute_frame_stats(&self.frame_ring);
                for entry in reader.drain_logs() {
                    if self.logs.len() >= 160 {
                        self.logs.pop_front();
                    }
                    self.logs.push_back(entry);
                }
            }

            let fps = if self.snap.fps_1s > 0.0 {
                self.snap.fps_1s as f32
            } else {
                self.frame_stats.rolling_fps as f32
            };
            Self::push_hist(&mut self.fps_hist, fps);
            Self::push_hist(&mut self.sys_cpu_hist, self.os.sys_cpu_pct as f32);
            Self::push_hist(&mut self.proc_cpu_hist, self.os.proc_cpu_pct as f32);
            let ram_pct = if self.os.ram_total_bytes > 0 {
                (self.os.ram_used_bytes as f64 / self.os.ram_total_bytes as f64 * 100.0) as f32
            } else {
                self.os.ram_load_pct as f32
            };
            Self::push_hist(&mut self.ram_hist, ram_pct);
            let vram_pct = if self.snap.vram_dedicated_budget > 0 {
                (self.snap.vram_dedicated_used as f64 / self.snap.vram_dedicated_budget as f64 * 100.0) as f32
            } else {
                0.0
            };
            Self::push_hist(&mut self.vram_hist, vram_pct);
            Self::push_hist(&mut self.gpu_busy_hist, self.snap.card_busy_pct as f32);
        }

        if was_alive && !self.sampler.alive {
            // Drain final snapshot & logs on exit/crash.
            if let Some(reader) = &mut self.reader {
                self.snap = reader.snapshot();
                for entry in reader.drain_logs() {
                    if self.logs.len() >= 160 {
                        self.logs.pop_front();
                    }
                    self.logs.push_back(entry);
                }
            }
            let code = self.sampler.exit_code.unwrap_or(self.snap.exit_code);
            if self.snap.phase != Phase::Crashed as u32 {
                self.snap.phase = if code == 0 {
                    Phase::Exited as u32
                } else {
                    Phase::Crashed as u32
                };
                self.snap.exit_code = code;
            }
            self.exited_at = Some(Instant::now());
        }

        if let Some(at) = self.exited_at {
            let clean = self.snap.phase == Phase::Exited as u32
                && self.snap.exit_code == 0
                && self.snap.crash_summary_len == 0;
            if clean && !self.keep_open && at.elapsed() >= Duration::from_secs(4) {
                // SAFETY: destroys our own monitor window on clean exit countdown.
                unsafe { let _ = DestroyWindow(hwnd); };
                return;
            }
        }

        self.update_caption(hwnd);
        // SAFETY: triggers repaint of client area.
        unsafe { let _ = InvalidateRect(hwnd, std::ptr::null(), 0); };
    }

    fn effective_fps(&self) -> f64 {
        if self.snap.fps_1s > 0.0 {
            self.snap.fps_1s
        } else {
            self.frame_stats.rolling_fps
        }
    }

    fn is_stalled(&self) -> bool {
        self.sampler.alive
            && self.snap.phase == Phase::Running as u32
            && self.snap.last_present_uptime_ms > 0
            && self
                .snap
                .uptime_ms
                .saturating_sub(self.snap.last_present_uptime_ms)
                > 2500
    }

    fn update_caption(&mut self, hwnd: HWND) {
        let exe = read_str(&self.snap.exe_name, self.snap.exe_name_len as usize);
        let name = if exe.is_empty() { "VIctory" } else { exe };
        let phase = Phase::from_raw(self.snap.phase).label();
        let fps = self.effective_fps();
        let title = if fps > 0.0 {
            format!("VIctory Debug Monitor — {name} (PID {}) | {phase} | {fps:.1} FPS", self.pid)
        } else {
            format!("VIctory Debug Monitor — {name} (PID {}) | {phase}", self.pid)
        };
        if title != self.last_title {
            let w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: our own window and NUL-terminated caption.
            unsafe { let _ = SetWindowTextW(hwnd, w.as_ptr()); };
            self.last_title = title;
        }
    }

    fn build_clipboard_report(&self) -> String {
        let s = &self.snap;
        let os = &self.os;
        let fs = &self.frame_stats;
        let mut out = String::with_capacity(2048);
        let exe = read_str(&s.exe_name, s.exe_name_len as usize);
        let build = read_str(&s.build_label, s.build_label_len as usize);
        let hash = read_str(&s.build_hash, s.build_hash_len as usize);
        let adapter = read_str(&s.adapter_name, s.adapter_name_len as usize);
        let flags = read_str(&s.flags_summary, s.flags_summary_len as usize);
        let _ = writeln!(out, "=== VIctory Debug Snapshot ===");
        let _ = writeln!(
            out,
            "Title: {exe} (TitleID {:08X}) | Build: {build} ({hash}) | PID: {} | Phase: {} | Uptime: {:.1}s",
            s.title_id,
            self.pid,
            Phase::from_raw(s.phase).label(),
            s.uptime_ms as f64 / 1000.0
        );
        let gpu_name = if adapter.is_empty() { "Pending" } else { adapter };
        let _ = writeln!(
            out,
            "GPU: {gpu_name} | Wave: {}-{} | ReBAR/GpuUpload: {} | Res: {}x{} (Cap: {}x{})",
            s.wave_min,
            s.wave_max,
            s.gpu_upload_heap != 0,
            s.target_width,
            s.target_height,
            s.capped_width,
            s.capped_height
        );
        let _ = writeln!(
            out,
            "FPS: {:.1} (1% low {:.1}) | Frame ms: avg {:.1}, med {:.1}, p95 {:.1}, max {:.1} | Draws/fr: {:.0}, Dropped/fr: {:.1}, Indirect/fr: {:.0}",
            self.effective_fps(),
            fs.one_pct_low_fps,
            fs.avg_ms,
            fs.med_ms,
            fs.p95_ms,
            fs.max_ms,
            s.draws_per_frame,
            s.dropped_per_frame,
            s.indirect_per_frame
        );
        let _ = writeln!(
            out,
            "Pipeline Breakdown (ms/fr): CPU Prep {:.1} (Copies {:.1} / {:.1} MB, TexRefresh {:.1} / {:.1} MB), GPU Wait {:.1}, CopyBack {:.1} / {:.2} MB, Card Busy {:.0}% (IdleBehind {:.1} ms), PipeWait {:.1}",
            s.cpu_prepare_ms,
            s.copy_up_ms,
            s.copy_up_mb,
            s.tex_refresh_ms,
            s.tex_refresh_mb,
            s.gpu_wait_ms,
            s.copy_back_ms,
            s.copy_back_mb,
            s.card_busy_pct,
            s.card_idle_behind_ms,
            s.pipeline_wait_ms
        );
        let _ = writeln!(
            out,
            "Loading & Shaders: Pipelines made {} loaded {} early {} queued {} (peak {}) | Shaders cache {}/{} ({:.0}% hit), containers {:.0} MB, DXIL {:.0} MB, Lib {:.0} MB",
            s.pipelines_made,
            s.pipelines_loaded,
            s.pipelines_early,
            s.pipelines_waiting,
            s.pipelines_peak_waiting,
            s.shader_cache_hits,
            s.shader_cache_hits + s.shader_cache_misses,
            pct_f(s.shader_cache_hits, s.shader_cache_hits + s.shader_cache_misses),
            mb_f(s.shaders_container_bytes),
            mb_f(s.shaders_translation_bytes),
            mb_f(s.pipeline_lib_bytes)
        );
        let _ = writeln!(
            out,
            "DirectStorage: {} queues, {} workers ({:.2} busy) | Q: {} running {} held {} | Done: {} ({:.1} MB @ {:.1} MB/s) | BCPack: {:.1} MB | Staged: {:.1}/{:.1} MB",
            s.dstorage_queues,
            s.dstorage_workers,
            s.dstorage_busy_workers,
            s.dstorage_queued_reads,
            s.dstorage_running_reads,
            s.dstorage_held_reads,
            s.dstorage_done_reads,
            mb_f(s.dstorage_done_bytes),
            s.dstorage_read_mb_s,
            mb_f(s.dstorage_bcpack_bytes),
            mb_f(s.dstorage_staged_bytes),
            mb_f(s.dstorage_staging_limit)
        );
        let _ = writeln!(
            out,
            "System Usage: Sys CPU {:.1}% | Game CPU {:.1}% ({:.2}/{} cores, {} threads, {:.0} PF/s) | RAM {:.2}/{:.2} GB (Commit {:.2}/{:.2} GB) | Proc WS {:.0} MB, Private {:.0} MB",
            os.sys_cpu_pct,
            os.proc_cpu_pct,
            os.proc_cores_equiv,
            os.logical_cores,
            os.thread_count,
            os.page_faults_per_sec,
            gb_f(os.ram_used_bytes),
            gb_f(os.ram_total_bytes),
            gb_f(os.commit_used_bytes),
            gb_f(os.commit_limit_bytes),
            mb_f(os.proc_ws_bytes),
            mb_f(os.proc_private_bytes)
        );
        let _ = writeln!(
            out,
            "VRAM & Layer Mem: Dedicated {:.0}/{:.0} MB | Shared {:.0}/{:.0} MB | HostTex {} ({:.0} MB, evicted {}, full {}) | Shadow {} ({:.0} MB) | CardBuf {} ({:.0} MB) | Staging {} ({:.0} MB, spare {:.0} MB)",
            mb_f(s.vram_dedicated_used),
            mb_f(s.vram_dedicated_budget),
            mb_f(s.vram_shared_used),
            mb_f(s.vram_shared_budget),
            s.host_textures_alive,
            mb_f(s.host_textures_bytes),
            s.host_textures_evicted,
            s.heap_tex_full,
            s.shadow_copies_alive,
            mb_f(s.shadow_copies_bytes),
            s.card_buffers_alive,
            mb_f(s.card_buffers_bytes),
            s.staging_alive,
            mb_f(s.staging_bytes),
            mb_f(s.staging_spare_bytes)
        );
        let _ = writeln!(
            out,
            "Health: DriverErr {} (tripped={}) | GuardFaults {:.1}/s | Remaps {:.1}/s ({:.1} ms/s) | SlowCoreLooks R {}/{} W {}/{} Rec {}/{}",
            s.driver_errors,
            s.driver_tripped != 0,
            s.guard_faults_per_sec,
            s.remaps_per_sec,
            s.remap_wait_ms_per_sec,
            s.render_slow_looks,
            s.render_total_looks,
            s.worker_slow_looks,
            s.worker_total_looks,
            s.record_slow_looks,
            s.record_total_looks
        );
        if s.crash_summary_len > 0 {
            let _ = writeln!(
                out,
                "Crash: {}",
                read_str(&s.crash_summary, s.crash_summary_len as usize)
            );
        }
        if !flags.is_empty() {
            let _ = writeln!(out, "Flags: {flags}");
        }
        if !self.logs.is_empty() {
            let _ = writeln!(out, "--- Recent Events ---");
            for e in self.logs.iter().rev().take(15).rev() {
                let lvl = LogLevel::from_raw(e.level).tag();
                let tgt = read_str(&e.target, e.target_len as usize);
                let msg = read_str(&e.message, e.message_len as usize);
                let _ = writeln!(out, "[{:>6.1}s] {lvl} {tgt}: {msg}", e.uptime_ms as f64 / 1000.0);
            }
        }
        out
    }
}

fn mb_f(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn gb_f(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0 * 1024.0)
}

fn pct_f(num: u64, den: u64) -> f64 {
    if den == 0 {
        0.0
    } else {
        (num as f64 / den as f64) * 100.0
    }
}

fn fmt_bytes_short(b: u64) -> String {
    if b >= (1 << 30) {
        format!("{:.2} GB", gb_f(b))
    } else if b >= (1 << 20) {
        format!("{:.1} MB", mb_f(b))
    } else if b >= (1 << 10) {
        format!("{:.0} KB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}

struct Fonts {
    title: HFONT,
    section: HFONT,
    hero: HFONT,
    body: HFONT,
    mono: HFONT,
    small: HFONT,
}

impl Fonts {
    fn create(dpi: u32) -> Self {
        let px = |pt: i32| -((pt * dpi as i32) / 72).max(9);
        let make = |pt: i32, weight: i32, face: &str| -> HFONT {
            let w: Vec<u16> = face.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: GDI font creation with valid face name.
            unsafe {
                CreateFontW(
                    px(pt),
                    0,
                    0,
                    0,
                    weight,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET as u32,
                    OUT_DEFAULT_PRECIS as u32,
                    CLIP_DEFAULT_PRECIS as u32,
                    CLEARTYPE_QUALITY as u32,
                    (DEFAULT_PITCH | FF_DONTCARE) as u32,
                    w.as_ptr(),
                )
            }
        };
        Self {
            title: make(11, FW_SEMIBOLD as i32, "Segoe UI"),
            section: make(9, FW_BOLD as i32, "Segoe UI"),
            hero: make(19, FW_BOLD as i32, "Segoe UI"),
            body: make(9, FW_NORMAL as i32, "Segoe UI"),
            mono: make(9, FW_NORMAL as i32, "Consolas"),
            small: make(8, FW_NORMAL as i32, "Segoe UI"),
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        // SAFETY: deletes GDI fonts created by `Fonts::create`.
        unsafe {
            for f in [self.title, self.section, self.hero, self.body, self.mono, self.small] {
                if !f.is_null() {
                    let _ = DeleteObject(f.cast());
                }
            }
        }
    }
}

struct Canvas<'a> {
    hdc: HDC,
    scale: f32,
    fonts: &'a Fonts,
    hits: &'a mut Vec<HitRect>,
}

impl<'a> Canvas<'a> {
    fn sc(&self, px: i32) -> i32 {
        ((px as f32) * self.scale).round() as i32
    }

    fn fill_rect(&self, r: RECT, color: u32) {
        // SAFETY: valid HDC and solid brush freed immediately.
        unsafe {
            let brush = CreateSolidBrush(color);
            let _ = FillRect(self.hdc, &r, brush);
            let _ = DeleteObject(brush.cast());
        }
    }

    fn draw_card(&self, r: RECT, title: &str, accent: u32) -> RECT {
        // SAFETY: GDI rectangle & pen/brush operations on valid HDC.
        unsafe {
            let brush = CreateSolidBrush(palette::CARD);
            let pen = CreatePen(PS_SOLID, 1, palette::BORDER);
            let old_b = SelectObject(self.hdc, brush.cast());
            let old_p = SelectObject(self.hdc, pen.cast());
            let rad = self.sc(6);
            let _ = RoundRect(self.hdc, r.left, r.top, r.right, r.bottom, rad, rad);
            let _ = SelectObject(self.hdc, old_b);
            let _ = SelectObject(self.hdc, old_p);
            let _ = DeleteObject(brush.cast());
            let _ = DeleteObject(pen.cast());
        }
        let bar_h = self.sc(24);
        let strip = RECT {
            left: r.left + 1,
            top: r.top + 1,
            right: r.right - 1,
            bottom: (r.top + bar_h).min(r.bottom),
        };
        self.fill_rect(strip, palette::CARD_ALT);
        let accent_bar = RECT {
            left: r.left + 1,
            top: r.top + 1,
            right: r.left + self.sc(4),
            bottom: (r.top + bar_h).min(r.bottom),
        };
        self.fill_rect(accent_bar, accent);
        let title_r = RECT {
            left: r.left + self.sc(10),
            top: r.top + 2,
            right: r.right - self.sc(8),
            bottom: r.top + bar_h,
        };
        self.text(title_r, title, self.fonts.section, palette::TEXT, false);
        let pad = self.sc(10);
        RECT {
            left: r.left + pad,
            top: r.top + bar_h + self.sc(6),
            right: r.right - pad,
            bottom: r.bottom - self.sc(8),
        }
    }

    fn text(&self, mut r: RECT, s: &str, font: HFONT, color: u32, right_align: bool) {
        if r.right <= r.left || r.bottom <= r.top || s.is_empty() {
            return;
        }
        let mut w: Vec<u16> = s.encode_utf16().collect();
        let align = if right_align { DT_RIGHT } else { DT_LEFT };
        // SAFETY: valid HDC, font, and UTF-16 slice.
        unsafe {
            let old = SelectObject(self.hdc, font.cast());
            SetBkMode(self.hdc, TRANSPARENT as i32);
            SetTextColor(self.hdc, color);
            let _ = DrawTextW(
                self.hdc,
                w.as_mut_ptr(),
                w.len() as i32,
                &mut r,
                align | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            let _ = SelectObject(self.hdc, old);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn kv_row(&self, x: i32, y: i32, w: i32, h: i32, label: &str, value: &str, val_color: u32) {
        let split = x + (w * 54) / 100;
        self.text(
            RECT { left: x, top: y, right: split, bottom: y + h },
            label,
            self.fonts.small,
            palette::TEXT_DIM,
            false,
        );
        self.text(
            RECT { left: split, top: y, right: x + w, bottom: y + h },
            value,
            self.fonts.mono,
            val_color,
            true,
        );
    }

    fn progress_bar(&self, r: RECT, frac: f64, fill_color: u32, label: &str) {
        self.fill_rect(r, palette::TRACK);
        let f = frac.clamp(0.0, 1.0);
        let fill_w = ((r.right - r.left) as f64 * f).round() as i32;
        if fill_w > 0 {
            self.fill_rect(
                RECT {
                    left: r.left,
                    top: r.top,
                    right: (r.left + fill_w).min(r.right),
                    bottom: r.bottom,
                },
                fill_color,
            );
        }
        if !label.is_empty() {
            let pad = RECT {
                left: r.left + self.sc(6),
                top: r.top,
                right: r.right - self.sc(6),
                bottom: r.bottom,
            };
            self.text(pad, label, self.fonts.small, palette::TEXT, false);
        }
    }

    fn draw_button(&mut self, r: RECT, label: &str, active: bool, accent: u32, action: Action) {
        let bg = if active { palette::BTN_ACTIVE } else { palette::CARD_ALT };
        let border = if active { accent } else { palette::BORDER };
        // SAFETY: valid HDC and temporary pen/brush.
        unsafe {
            let brush = CreateSolidBrush(bg);
            let pen = CreatePen(PS_SOLID, 1, border);
            let old_b = SelectObject(self.hdc, brush.cast());
            let old_p = SelectObject(self.hdc, pen.cast());
            let rad = self.sc(5);
            let _ = RoundRect(self.hdc, r.left, r.top, r.right, r.bottom, rad, rad);
            let _ = SelectObject(self.hdc, old_b);
            let _ = SelectObject(self.hdc, old_p);
            let _ = DeleteObject(brush.cast());
            let _ = DeleteObject(pen.cast());
        }
        let text_r = RECT {
            left: r.left + self.sc(4),
            top: r.top,
            right: r.right - self.sc(4),
            bottom: r.bottom,
        };
        let mut w: Vec<u16> = label.encode_utf16().collect();
        // SAFETY: draws centered button caption.
        unsafe {
            let old = SelectObject(self.hdc, self.fonts.small.cast());
            SetBkMode(self.hdc, TRANSPARENT as i32);
            SetTextColor(self.hdc, if active { palette::TEXT } else { palette::TEXT_DIM });
            let _ = DrawTextW(
                self.hdc,
                w.as_mut_ptr(),
                w.len() as i32,
                &mut { text_r },
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            let _ = SelectObject(self.hdc, old);
        }
        self.hits.push(HitRect { rect: r, action });
    }

    fn sparkline(&self, r: RECT, data: &VecDeque<f32>, max_v: f32, color: u32, caption: &str) {
        if r.right - r.left < 16 || r.bottom - r.top < 16 {
            return;
        }
        self.fill_rect(r, rgb(16, 20, 27));
        // SAFETY: draws grid line and sparkline polyline.
        unsafe {
            let grid_pen = CreatePen(PS_DASH, 1, palette::GRID);
            let old_p = SelectObject(self.hdc, grid_pen.cast());
            let mid_y = (r.top + r.bottom) / 2;
            let _ = MoveToEx(self.hdc, r.left, mid_y, std::ptr::null_mut());
            let _ = LineTo(self.hdc, r.right, mid_y);
            let _ = SelectObject(self.hdc, old_p);
            let _ = DeleteObject(grid_pen.cast());
        }
        let max_v = max_v.max(1.0);
        let w = (r.right - r.left - 2).max(1);
        let h = (r.bottom - r.top - 4).max(1);
        if data.len() >= 2 {
            let n = data.len();
            let mut pts = Vec::with_capacity(n);
            for (i, &val) in data.iter().enumerate() {
                let x = r.left + 1 + ((i as i32 * w) / (n.saturating_sub(1).max(1) as i32));
                let frac = (val / max_v).clamp(0.0, 1.0);
                let y = r.bottom - 2 - (frac * h as f32).round() as i32;
                pts.push(POINT { x, y });
            }
            // SAFETY: draws sparkline curve.
            unsafe {
                let pen = CreatePen(PS_SOLID, self.sc(1).max(1), color);
                let old_p = SelectObject(self.hdc, pen.cast());
                let _ = Polyline(self.hdc, pts.as_ptr(), pts.len() as i32);
                let _ = SelectObject(self.hdc, old_p);
                let _ = DeleteObject(pen.cast());
            }
        }
        if !caption.is_empty() {
            let tr = RECT {
                left: r.left + self.sc(4),
                top: r.top + 1,
                right: r.right - self.sc(4),
                bottom: r.top + self.sc(14),
            };
            self.text(tr, caption, self.fonts.small, palette::TEXT_MUTED, false);
        }
    }

    fn frametime_graph(&self, r: RECT, frames_us: &VecDeque<u32>, max_ms_cap: f64) {
        if r.right - r.left < 20 || r.bottom - r.top < 24 {
            return;
        }
        self.fill_rect(r, rgb(16, 20, 27));
        let max_ms = frames_us
            .iter()
            .map(|&v| f64::from(v) / 1000.0)
            .fold(35.0f64, f64::max)
            .clamp(35.0, max_ms_cap);
        let h = (r.bottom - r.top - 4).max(1) as f64;

        // Reference dashed lines at 16.7ms (60fps) and 33.3ms (30fps)
        for (ref_ms, col) in [(16.67, rgb(28, 70, 50)), (33.33, rgb(42, 62, 82))] {
            if ref_ms < max_ms {
                let y = r.bottom - 2 - ((ref_ms / max_ms) * h).round() as i32;
                // SAFETY: valid pen and line coordinates.
                unsafe {
                    let pen = CreatePen(PS_DASH, 1, col);
                    let old_p = SelectObject(self.hdc, pen.cast());
                    let _ = MoveToEx(self.hdc, r.left, y, std::ptr::null_mut());
                    let _ = LineTo(self.hdc, r.right, y);
                    let _ = SelectObject(self.hdc, old_p);
                    let _ = DeleteObject(pen.cast());
                }
            }
        }

        let w = (r.right - r.left - 4).max(1);
        let n = frames_us.len();
        if n > 0 {
            let bar_w = (w as f64 / n.max(1) as f64).max(1.0);
            for (i, &us) in frames_us.iter().enumerate() {
                let ms = f64::from(us) / 1000.0;
                let bh = ((ms / max_ms).clamp(0.0, 1.0) * h).round().max(1.0) as i32;
                let x0 = r.left + 2 + (i as f64 * bar_w).floor() as i32;
                let x1 = (r.left + 2 + ((i + 1) as f64 * bar_w).floor() as i32).max(x0 + 1);
                let col = if ms <= 20.0 {
                    palette::GREEN
                } else if ms <= 40.0 {
                    palette::CYAN
                } else if ms <= 105.0 {
                    palette::AMBER
                } else {
                    palette::RED
                };
                self.fill_rect(
                    RECT {
                        left: x0,
                        top: (r.bottom - 2 - bh).max(r.top + 1),
                        right: x1.min(r.right - 2),
                        bottom: r.bottom - 2,
                    },
                    col,
                );
            }
        }
        let lbl = format!("Frame times (scale 0..{max_ms:.0} ms)");
        self.text(
            RECT {
                left: r.left + self.sc(4),
                top: r.top + 1,
                right: r.right - self.sc(4),
                bottom: r.top + self.sc(14),
            },
            &lbl,
            self.fonts.small,
            palette::TEXT_MUTED,
            false,
        );
    }

    fn core_grid(&self, r: RECT, pcts: &[f32], topology: &CoreTopology, show_labels: bool) {
        if pcts.is_empty() || r.right <= r.left || r.bottom <= r.top {
            return;
        }
        let n = pcts.len();
        let cols = if n <= 8 { n } else if n <= 16 { 8 } else { n.div_ceil(2).clamp(8, 16) };
        let rows = n.div_ceil(cols);
        let gap = self.sc(3);
        let cw = ((r.right - r.left - gap * (cols as i32 - 1)) / cols as i32).max(4);
        let ch = ((r.bottom - r.top - gap * (rows as i32 - 1)) / rows as i32).max(8);

        for (i, &pct) in pcts.iter().enumerate() {
            let col = i % cols;
            let row = i / cols;
            let x0 = r.left + col as i32 * (cw + gap);
            let y0 = r.top + row as i32 * (ch + gap);
            let cell = RECT {
                left: x0,
                top: y0,
                right: (x0 + cw).min(r.right),
                bottom: (y0 + ch).min(r.bottom),
            };
            self.fill_rect(cell, palette::TRACK);
            let is_fast = topology.fast_mask.get(i).copied().unwrap_or(true);
            let bar_col = if pct >= 90.0 {
                palette::RED
            } else if !is_fast && topology.has_hybrid {
                palette::PURPLE
            } else if pct >= 60.0 {
                palette::AMBER
            } else {
                palette::CYAN
            };
            let fill_h = (((cell.bottom - cell.top) as f32) * (pct / 100.0).clamp(0.0, 1.0)).round() as i32;
            if fill_h > 0 {
                self.fill_rect(
                    RECT {
                        left: cell.left,
                        top: (cell.bottom - fill_h).max(cell.top),
                        right: cell.right,
                        bottom: cell.bottom,
                    },
                    bar_col,
                );
            }
            if show_labels && ch >= self.sc(18) {
                let kind = if topology.has_hybrid {
                    if is_fast { "P" } else { "E" }
                } else {
                    "C"
                };
                let lbl = format!("{kind}{i} {:.0}%", pct);
                self.text(
                    RECT {
                        left: cell.left + 2,
                        top: cell.top + 1,
                        right: cell.right - 2,
                        bottom: cell.bottom - 1,
                    },
                    &lbl,
                    self.fonts.small,
                    palette::TEXT,
                    false,
                );
            }
        }
    }
}

fn phase_style(state: &MonitorState) -> (&'static str, u32) {
    if state.is_stalled() {
        return ("STALLED?", palette::AMBER);
    }
    match Phase::from_raw(state.snap.phase) {
        Phase::Boot => ("BOOT", palette::BLUE),
        Phase::Preparing => ("PREPARING", palette::BLUE),
        Phase::Loading => ("LOADING", palette::CYAN),
        Phase::GpuInit => ("GPU INIT", palette::CYAN),
        Phase::Running => ("RUNNING", palette::GREEN),
        Phase::Exited => ("EXITED", palette::TEXT_DIM),
        Phase::Crashed => ("CRASHED", palette::RED),
    }
}

fn pressure_color(pct: f64) -> u32 {
    if pct >= 85.0 {
        palette::RED
    } else if pct >= 70.0 {
        palette::AMBER
    } else {
        palette::BLUE
    }
}

fn render(hdc: HDC, client: RECT, state: &mut MonitorState, fonts: &Fonts) {
    let scale = (state.dpi as f32 / 96.0).max(1.0);
    let mut hits = Vec::new();
    let mut c = Canvas {
        hdc,
        scale,
        fonts,
        hits: &mut hits,
    };

    c.fill_rect(client, palette::BG);

    let pad = c.sc(10);
    let header_h = c.sc(44);
    let header_r = RECT {
        left: 0,
        top: 0,
        right: client.right,
        bottom: header_h,
    };
    c.fill_rect(header_r, palette::HEADER);
    c.fill_rect(
        RECT {
            left: 0,
            top: header_h - 1,
            right: client.right,
            bottom: header_h,
        },
        palette::BORDER,
    );

    // Status pill & identity
    let (badge_txt, badge_col) = phase_style(state);
    let badge_r = RECT {
        left: pad,
        top: c.sc(9),
        right: pad + c.sc(84),
        bottom: c.sc(34),
    };
    c.fill_rect(badge_r, badge_col);
    c.text(
        RECT {
            left: badge_r.left + c.sc(6),
            top: badge_r.top,
            right: badge_r.right - c.sc(4),
            bottom: badge_r.bottom,
        },
        badge_txt,
        fonts.section,
        rgb(10, 14, 18),
        false,
    );

    let exe = read_str(&state.snap.exe_name, state.snap.exe_name_len as usize);
    let console = if state.snap.console_mode == 1 { "Series S" } else { "Series X" };
    let res = if state.snap.target_width > 0 {
        format!("{}x{}", state.snap.target_width, state.snap.target_height)
    } else if state.snap.capped_width > 0 {
        format!("cap {}x{}", state.snap.capped_width, state.snap.capped_height)
    } else {
        "pending".into()
    };
    let up_s = state.snap.uptime_ms / 1000;
    let flavor = if state.snap.is_diagnostics_build != 0 { "diag" } else { "rel" };
    let id_line = format!(
        "{} ({:08X}) | {} | {} | {} | {:02}:{:02} | PID {}",
        if exe.is_empty() { "VIctory" } else { exe },
        state.snap.title_id,
        console,
        res,
        flavor,
        up_s / 60,
        up_s % 60,
        state.pid
    );

    // Right-side action buttons + tabs
    let btn_h = c.sc(24);
    let btn_y = c.sc(10);
    let gap = c.sc(6);
    let mut rx = client.right - pad;
    let copy_lbl = if state
        .copied_until
        .is_some_and(|t| Instant::now() < t)
    {
        "Copied!"
    } else {
        "Copy"
    };
    let actions = [
        (copy_lbl, c.sc(60), false, palette::GREEN, Action::CopyReport),
        (
            if state.keep_open { "Pin: ON" } else { "Pin" },
            c.sc(58),
            state.keep_open,
            palette::CYAN,
            Action::ToggleKeepOpen,
        ),
        (
            if state.topmost { "Top: ON" } else { "Top" },
            c.sc(58),
            state.topmost,
            palette::PURPLE,
            Action::ToggleTopmost,
        ),
        (
            if state.paused { "Resume" } else { "Pause" },
            c.sc(64),
            state.paused,
            palette::AMBER,
            Action::TogglePause,
        ),
    ];
    for (lbl, w, active, acc, act) in actions {
        let br = RECT {
            left: rx - w,
            top: btn_y,
            right: rx,
            bottom: btn_y + btn_h,
        };
        c.draw_button(br, lbl, active, acc, act);
        rx -= w + gap;
    }

    let tabs = [
        ("1 Overview", Tab::Overview, c.sc(84)),
        ("2 GPU & Mem", Tab::GpuMem, c.sc(92)),
        ("3 Load & CPU", Tab::LoadingCpu, c.sc(96)),
        ("4 Logs & Env", Tab::LogsEnv, c.sc(92)),
    ];
    let tabs_total_w: i32 = tabs.iter().map(|t| t.2 + gap).sum();
    let mut tx = (rx - tabs_total_w - c.sc(8)).max(badge_r.right + c.sc(120));
    c.text(
        RECT {
            left: badge_r.right + c.sc(10),
            top: c.sc(8),
            right: tx - c.sc(8),
            bottom: c.sc(36),
        },
        &id_line,
        fonts.body,
        palette::TEXT_DIM,
        false,
    );
    for (lbl, tab, w) in tabs {
        if tx + w <= rx {
            c.draw_button(
                RECT {
                    left: tx,
                    top: btn_y,
                    right: tx + w,
                    bottom: btn_y + btn_h,
                },
                lbl,
                state.active_tab == tab,
                palette::BLUE,
                Action::Tab(tab),
            );
        }
        tx += w + gap;
    }

    let mut content_top = header_h + pad;

    // Optional top banner (crash, exit, stall, or active loading/pipeline compile)
    let banner_h = c.sc(28);
    if state.snap.phase == Phase::Crashed as u32 || state.snap.crash_summary_len > 0 {
        let br = RECT {
            left: pad,
            top: content_top,
            right: client.right - pad,
            bottom: content_top + banner_h,
        };
        c.fill_rect(br, rgb(85, 22, 26));
        let summary = read_str(&state.snap.crash_summary, state.snap.crash_summary_len as usize);
        let msg = if summary.is_empty() {
            format!("PROCESS ENDED WITH EXIT CODE {:#x} (monitor kept open for inspection)", state.snap.exit_code)
        } else {
            format!("FAILURE ({:#x}): {summary}", state.snap.exit_code)
        };
        c.text(
            RECT {
                left: br.left + c.sc(8),
                top: br.top,
                right: br.right - c.sc(8),
                bottom: br.bottom,
            },
            &msg,
            fonts.section,
            rgb(254, 202, 202),
            false,
        );
        content_top += banner_h + c.sc(8);
    } else if state.snap.phase == Phase::Exited as u32 {
        let br = RECT {
            left: pad,
            top: content_top,
            right: client.right - pad,
            bottom: content_top + banner_h,
        };
        c.fill_rect(br, rgb(30, 41, 59));
        let msg = if state.keep_open {
            format!("Title exited cleanly (code {}). Pinned open.", state.snap.exit_code)
        } else {
            let left = 4u64.saturating_sub(state.exited_at.map_or(0, |t| t.elapsed().as_secs()));
            format!(
                "Title exited cleanly (code {}). Auto-closing in {left}s — click Pin to keep open.",
                state.snap.exit_code
            )
        };
        c.text(
            RECT {
                left: br.left + c.sc(8),
                top: br.top,
                right: br.right - c.sc(8),
                bottom: br.bottom,
            },
            &msg,
            fonts.body,
            palette::TEXT,
            false,
        );
        content_top += banner_h + c.sc(8);
    } else if state.is_stalled() {
        let br = RECT {
            left: pad,
            top: content_top,
            right: client.right - pad,
            bottom: content_top + banner_h,
        };
        c.fill_rect(br, rgb(88, 56, 12));
        let age = state
            .snap
            .uptime_ms
            .saturating_sub(state.snap.last_present_uptime_ms) as f64
            / 1000.0;
        let msg = format!(
            "No frame presented for {age:.1}s (pipelines queued: {}, DStorage active: {} queued / {} running)",
            state.snap.pipelines_waiting,
            state.snap.dstorage_queued_reads,
            state.snap.dstorage_running_reads
        );
        c.text(
            RECT {
                left: br.left + c.sc(8),
                top: br.top,
                right: br.right - c.sc(8),
                bottom: br.bottom,
            },
            &msg,
            fonts.body,
            rgb(253, 230, 138),
            false,
        );
        content_top += banner_h + c.sc(8);
    } else if state.snap.phase < Phase::Running as u32 || state.snap.pipelines_waiting > 0 {
        let br = RECT {
            left: pad,
            top: content_top,
            right: client.right - pad,
            bottom: content_top + banner_h,
        };
        let done = state.snap.pipelines_made + state.snap.pipelines_loaded;
        let total = done + state.snap.pipelines_waiting;
        let (frac, label) = if state.snap.phase < Phase::Running as u32 && total == 0 {
            let st = read_str(&state.snap.stage, state.snap.stage_len as usize);
            let det = read_str(&state.snap.stage_detail, state.snap.stage_detail_len as usize);
            (
                f64::from(state.snap.boot_percent) / 100.0,
                format!("LOADING {}% — {}  ({})", state.snap.boot_percent, st, det),
            )
        } else {
            let f = if total > 0 { done as f64 / total as f64 } else { 0.0 };
            (
                f,
                format!(
                    "BUILDING PIPELINES {:.0}% ({done}/{total}, {} queued) | Shaders {} (hits {}) | DStorage {:.1} MB read",
                    f * 100.0,
                    state.snap.pipelines_waiting,
                    state.snap.shader_cache_hits + state.snap.shader_cache_misses,
                    state.snap.shader_cache_hits,
                    mb_f(state.snap.dstorage_done_bytes)
                ),
            )
        };
        c.progress_bar(br, frac, palette::BLUE, &label);
        content_top += banner_h + c.sc(8);
    }

    let body_r = RECT {
        left: pad,
        top: content_top,
        right: client.right - pad,
        bottom: client.bottom - pad,
    };

    state.log_panel_rect = RECT::default();
    match state.active_tab {
        Tab::Overview => draw_overview(&mut c, body_r, state),
        Tab::GpuMem => draw_gpu_mem(&mut c, body_r, state),
        Tab::LoadingCpu => draw_loading_cpu(&mut c, body_r, state),
        Tab::LogsEnv => draw_logs_env(&mut c, body_r, state),
    }

    state.hits = hits;
}

fn draw_overview(c: &mut Canvas<'_>, area: RECT, state: &mut MonitorState) {
    let gap = c.sc(8);
    let total_w = (area.right - area.left).max(100);
    let total_h = (area.bottom - area.top).max(100);
    let log_h = c.sc(138).min((total_h * 28) / 100);
    let rows_h = total_h - log_h - gap * 2;
    let row1_h = (rows_h * 52) / 100;
    let row2_h = rows_h - row1_h;

    let col_w = (total_w - gap * 2) / 3;
    let c0_l = area.left;
    let c1_l = c0_l + col_w + gap;
    let c2_l = c1_l + col_w + gap;
    let c2_r = area.right;

    let r1_top = area.top;
    let r1_bot = r1_top + row1_h;
    let r2_top = r1_bot + gap;
    let r2_bot = r2_top + row2_h;

    let s = &state.snap;
    let os = &state.os;
    let fs = &state.frame_stats;
    let row_h = c.sc(16);

    // Card 1: FPS & Frame Timing
    let inner = c.draw_card(
        RECT { left: c0_l, top: r1_top, right: c0_l + col_w, bottom: r1_bot },
        "FPS & FRAME TIMING",
        palette::GREEN,
    );
    let fps = state.effective_fps();
    let fps_col = if fps >= 28.0 {
        palette::GREEN
    } else if fps >= 15.0 {
        palette::CYAN
    } else if fps >= 5.0 {
        palette::AMBER
    } else if fps > 0.0 {
        palette::RED
    } else {
        palette::TEXT_DIM
    };
    let hero_h = c.sc(30);
    c.text(
        RECT { left: inner.left, top: inner.top, right: inner.left + c.sc(140), bottom: inner.top + hero_h },
        &if fps > 0.0 { format!("{fps:.1} FPS") } else { "-- FPS".into() },
        c.fonts.hero,
        fps_col,
        false,
    );
    c.text(
        RECT { left: inner.left + c.sc(138), top: inner.top + c.sc(4), right: inner.right, bottom: inner.top + hero_h },
        &format!("1% Low: {:.1} | {:.1} ms", fs.one_pct_low_fps, if s.frame_ms_1s > 0.0 { s.frame_ms_1s } else { fs.avg_ms }),
        c.fonts.mono,
        palette::TEXT,
        true,
    );
    let mut y = inner.top + hero_h + c.sc(4);
    let w = inner.right - inner.left;
    c.kv_row(
        inner.left,
        y,
        w,
        row_h,
        "Min / Med / P95 / Max",
        &format!("{:.1} / {:.1} / {:.1} / {:.1} ms", fs.min_ms, fs.med_ms, fs.p95_ms, fs.max_ms),
        palette::TEXT,
    );
    y += row_h;
    c.kv_row(
        inner.left,
        y,
        w,
        row_h,
        "Frames (Pres / Begun)",
        &format!("{} / {}", s.total_frames_presented, s.total_frames_begun),
        palette::TEXT,
    );
    y += row_h;
    c.kv_row(
        inner.left,
        y,
        w,
        row_h,
        "Draws / Drop / Indirect",
        &format!("{:.0} / {:.1} / {:.0} /fr", s.draws_per_frame, s.dropped_per_frame, s.indirect_per_frame),
        if s.dropped_per_frame > 1.0 { palette::AMBER } else { palette::TEXT },
    );
    y += row_h + c.sc(4);
    if inner.bottom - y >= c.sc(34) {
        c.frametime_graph(
            RECT { left: inner.left, top: y, right: inner.right, bottom: inner.bottom },
            &state.frame_ring,
            250.0,
        );
    }

    // Card 2: Frame Pipeline & GPU Execution
    let inner = c.draw_card(
        RECT { left: c1_l, top: r1_top, right: c1_l + col_w, bottom: r1_bot },
        "FRAME BREAKDOWN & GPU",
        palette::CYAN,
    );
    let w = inner.right - inner.left;
    let mut y = inner.top;
    c.progress_bar(
        RECT { left: inner.left, top: y, right: inner.right, bottom: y + c.sc(18) },
        s.card_busy_pct / 100.0,
        palette::CYAN,
        &format!("GPU Busy {:.0}% | Idle {:.1} ms (behind us {:.1} ms)", s.card_busy_pct, s.card_idle_ms, s.card_idle_behind_ms),
    );
    y += c.sc(22);
    c.kv_row(inner.left, y, w, row_h, "CPU Prepare / GPU Wait", &format!("{:.1} / {:.1} ms", s.cpu_prepare_ms, s.gpu_wait_ms), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "Copies Up (Time / Vol)", &format!("{:.1} ms / {:.1} MB", s.copy_up_ms, s.copy_up_mb), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "Tex Refresh (Time / Vol)", &format!("{:.1} ms / {:.1} MB", s.tex_refresh_ms, s.tex_refresh_mb), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "Copy-Back (Time / Vol)", &format!("{:.1} ms / {:.2} MB", s.copy_back_ms, s.copy_back_mb), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "Present / PipeWait / Pace", &format!("{:.1} / {:.1} / {:.1} ms", s.present_show_ms, s.pipeline_wait_ms, s.pace_ms), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "Guard Faults / Remaps", &format!("{:.0}/s / {:.1}/s ({:.1} ms)", s.guard_faults_per_sec, s.remaps_per_sec, s.remap_wait_ms_per_sec), palette::TEXT);
    y += row_h;
    let drv_col = if s.driver_errors > 0 { palette::RED } else { palette::GREEN };
    c.kv_row(inner.left, y, w, row_h, "Driver Errors / Heap Full", &format!("{} / {}", s.driver_errors, s.heap_tex_full), drv_col);
    y += row_h + c.sc(4);
    if inner.bottom - y >= c.sc(28) {
        c.sparkline(
            RECT { left: inner.left, top: y, right: inner.right, bottom: inner.bottom },
            &state.gpu_busy_hist,
            100.0,
            palette::CYAN,
            "GPU Busy %",
        );
    }

    // Card 3: Loading, Pipelines & DirectStorage
    let inner = c.draw_card(
        RECT { left: c2_l, top: r1_top, right: c2_r, bottom: r1_bot },
        "LOADING, SHADERS & DIRECTSTORAGE",
        palette::BLUE,
    );
    let w = inner.right - inner.left;
    let mut y = inner.top;
    let p_done = s.pipelines_made + s.pipelines_loaded;
    let p_tot = p_done + s.pipelines_waiting;
    let p_frac = if p_tot > 0 { p_done as f64 / p_tot as f64 } else { 1.0 };
    c.progress_bar(
        RECT { left: inner.left, top: y, right: inner.right, bottom: y + c.sc(18) },
        p_frac,
        if s.pipelines_waiting > 0 { palette::BLUE } else { palette::GREEN },
        &format!("Pipelines {p_done}/{p_tot} ({} queued, {} early)", s.pipelines_waiting, s.pipelines_early),
    );
    y += c.sc(22);
    let sc_tot = s.shader_cache_hits + s.shader_cache_misses;
    c.kv_row(inner.left, y, w, row_h, "Pipelines Made / Loaded", &format!("{} / {}", s.pipelines_made, s.pipelines_loaded), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "Shader Cache Hits / Total", &format!("{}/{} ({:.0}%)", s.shader_cache_hits, sc_tot, pct_f(s.shader_cache_hits, sc_tot)), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "DXIL / Lib File Size", &format!("{:.0} MB / {:.0} MB", mb_f(s.shaders_translation_bytes), mb_f(s.pipeline_lib_bytes)), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "DStorage Queues / Workers", &format!("{} q / {} w ({:.1} busy)", s.dstorage_queues, s.dstorage_workers, s.dstorage_busy_workers), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "DStorage Q / Run / Held", &format!("{} / {} / {}", s.dstorage_queued_reads, s.dstorage_running_reads, s.dstorage_held_reads), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "DStorage Done / Speed", &format!("{} ({:.1} MB/s)", fmt_bytes_short(s.dstorage_done_bytes), s.dstorage_read_mb_s), palette::CYAN);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "BCPack / Boot->1st Frame", &format!("{} / {:.1}s", fmt_bytes_short(s.dstorage_bcpack_bytes), s.first_frame_ms as f64 / 1000.0), palette::TEXT);
    y += row_h;
    if inner.bottom - y >= row_h {
        c.kv_row(
            inner.left,
            y,
            w,
            row_h,
            "Heap Tex Queue (Due/Wait)",
            &format!("{}/{} (oldest {} fr)", s.heap_q_due, s.heap_tex_waiting, s.heap_q_oldest_frames),
            palette::TEXT,
        );
    }

    // Card 4: CPU Usage & Cores
    let inner = c.draw_card(
        RECT { left: c0_l, top: r2_top, right: c0_l + col_w, bottom: r2_bot },
        "CPU USAGE & CORES",
        palette::PURPLE,
    );
    let w = inner.right - inner.left;
    let mut y = inner.top;
    c.progress_bar(
        RECT { left: inner.left, top: y, right: inner.right, bottom: y + c.sc(17) },
        os.sys_cpu_pct / 100.0,
        pressure_color(os.sys_cpu_pct),
        &format!("System CPU {:.1}% | Game {:.1}% ({:.1}/{} cores)", os.sys_cpu_pct, os.proc_cpu_pct, os.proc_cores_equiv, os.logical_cores),
    );
    y += c.sc(20);
    c.kv_row(inner.left, y, w, row_h, "Threads / Page Faults", &format!("{} / {:.0}/s", os.thread_count, os.page_faults_per_sec), palette::TEXT);
    y += row_h;
    let slow_tot = s.render_slow_looks + s.worker_slow_looks + s.record_slow_looks;
    c.kv_row(
        inner.left,
        y,
        w,
        row_h,
        "Slow-Core Looks (R/W/Rec)",
        &format!("{}/{}  {}/{}  {}/{}", s.render_slow_looks, s.render_total_looks, s.worker_slow_looks, s.worker_total_looks, s.record_slow_looks, s.record_total_looks),
        if slow_tot > 0 { palette::AMBER } else { palette::GREEN },
    );
    y += row_h + c.sc(3);
    if inner.bottom - y >= c.sc(26) {
        let cores_h = ((inner.bottom - y) / 2).max(c.sc(20));
        c.core_grid(
            RECT { left: inner.left, top: y, right: inner.right, bottom: y + cores_h },
            &os.core_pcts,
            &state.sampler.topology,
            false,
        );
        let sp_top = y + cores_h + c.sc(4);
        if inner.bottom - sp_top >= c.sc(20) {
            c.sparkline(
                RECT { left: inner.left, top: sp_top, right: inner.right, bottom: inner.bottom },
                &state.sys_cpu_hist,
                100.0,
                palette::PURPLE,
                "System CPU %",
            );
        }
    }

    // Card 5: System RAM & Process Memory
    let inner = c.draw_card(
        RECT { left: c1_l, top: r2_top, right: c1_l + col_w, bottom: r2_bot },
        "SYSTEM RAM & PROCESS MEMORY",
        palette::AMBER,
    );
    let w = inner.right - inner.left;
    let mut y = inner.top;
    let ram_pct = pct_f(os.ram_used_bytes, os.ram_total_bytes);
    c.progress_bar(
        RECT { left: inner.left, top: y, right: inner.right, bottom: y + c.sc(17) },
        ram_pct / 100.0,
        pressure_color(ram_pct),
        &format!("RAM {:.1}/{:.1} GB ({ram_pct:.0}% | {:.1} GB free)", gb_f(os.ram_used_bytes), gb_f(os.ram_total_bytes), gb_f(os.ram_avail_bytes)),
    );
    y += c.sc(20);
    let com_pct = pct_f(os.commit_used_bytes, os.commit_limit_bytes);
    c.progress_bar(
        RECT { left: inner.left, top: y, right: inner.right, bottom: y + c.sc(17) },
        com_pct / 100.0,
        pressure_color(com_pct),
        &format!("Commit {:.1}/{:.1} GB ({com_pct:.0}%)", gb_f(os.commit_used_bytes), gb_f(os.commit_limit_bytes)),
    );
    y += c.sc(20);
    c.kv_row(inner.left, y, w, row_h, "Game Working Set (Peak)", &format!("{} ({})", fmt_bytes_short(os.proc_ws_bytes), fmt_bytes_short(os.proc_peak_ws_bytes)), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "Game Private Commit", &fmt_bytes_short(os.proc_private_bytes), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "OS Disk I/O (Read / Write)", &format!("{:.1} / {:.1} MB/s", os.io_read_mb_s, os.io_write_mb_s), palette::TEXT);
    y += row_h + c.sc(3);
    if inner.bottom - y >= c.sc(20) {
        c.sparkline(
            RECT { left: inner.left, top: y, right: inner.right, bottom: inner.bottom },
            &state.ram_hist,
            100.0,
            palette::AMBER,
            "RAM %",
        );
    }

    // Card 6: GPU VRAM & Layer Memory
    let inner = c.draw_card(
        RECT { left: c2_l, top: r2_top, right: c2_r, bottom: r2_bot },
        "GPU VRAM & LAYER MEMORY",
        palette::RED,
    );
    let w = inner.right - inner.left;
    let mut y = inner.top;
    let v_pct = pct_f(s.vram_dedicated_used, s.vram_dedicated_budget);
    c.progress_bar(
        RECT { left: inner.left, top: y, right: inner.right, bottom: y + c.sc(17) },
        v_pct / 100.0,
        pressure_color(v_pct),
        &format!("VRAM {:.0}/{:.0} MB ({v_pct:.0}%)", mb_f(s.vram_dedicated_used), mb_f(s.vram_dedicated_budget)),
    );
    y += c.sc(20);
    let sh_pct = pct_f(s.vram_shared_used, s.vram_shared_budget);
    c.progress_bar(
        RECT { left: inner.left, top: y, right: inner.right, bottom: y + c.sc(17) },
        sh_pct / 100.0,
        palette::BLUE,
        &format!("Shared GPU {:.0}/{:.0} MB ({sh_pct:.0}%)", mb_f(s.vram_shared_used), mb_f(s.vram_shared_budget)),
    );
    y += c.sc(20);
    c.kv_row(inner.left, y, w, row_h, "Host Textures (Evicted)", &format!("{} ({:.0} MB, ev {})", s.host_textures_alive, mb_f(s.host_textures_bytes), s.host_textures_evicted), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "Shadow / Card Buffers", &format!("{:.0} MB / {:.0} MB", mb_f(s.shadow_copies_bytes), mb_f(s.card_buffers_bytes)), palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "Staging (Spare) / Lists", &format!("{:.0} ({:.0}) MB / {}", mb_f(s.staging_bytes), mb_f(s.staging_spare_bytes), s.host_lists_alive), palette::TEXT);
    y += row_h + c.sc(3);
    if inner.bottom - y >= c.sc(20) {
        c.sparkline(
            RECT { left: inner.left, top: y, right: inner.right, bottom: inner.bottom },
            &state.vram_hist,
            100.0,
            palette::RED,
            "VRAM %",
        );
    }

    // Bottom Log Feed
    let log_box = RECT {
        left: area.left,
        top: r2_bot + gap,
        right: area.right,
        bottom: area.bottom,
    };
    let inner = c.draw_card(log_box, "LIVE DEBUG & EVENT FEED (scroll or press 4 for full)", palette::TEXT_DIM);
    state.log_panel_rect = inner;
    draw_log_lines(c, inner, &state.logs, state.log_scroll);
}

fn draw_gpu_mem(c: &mut Canvas<'_>, area: RECT, state: &MonitorState) {
    let gap = c.sc(8);
    let col_w = (area.right - area.left - gap) / 2;
    let s = &state.snap;
    let row_h = c.sc(18);

    // Left: Adapter, VRAM, Textures & Tiles
    let inner = c.draw_card(
        RECT { left: area.left, top: area.top, right: area.left + col_w, bottom: area.bottom },
        "ADAPTER, VRAM, TEXTURES & TILES",
        palette::RED,
    );
    let w = inner.right - inner.left;
    let mut y = inner.top;
    let adapter = read_str(&s.adapter_name, s.adapter_name_len as usize);
    c.kv_row(inner.left, y, w, row_h, "GPU Adapter", if adapter.is_empty() { "(initializing)" } else { adapter }, palette::TEXT);
    y += row_h;
    c.kv_row(inner.left, y, w, row_h, "Wave Lanes / GPU Upload Heap", &format!("{}-{} / {}", s.wave_min, s.wave_max, if s.gpu_upload_heap != 0 { "Yes (ReBAR)" } else { "No" }), palette::TEXT);
    y += row_h + c.sc(4);

    let v_pct = pct_f(s.vram_dedicated_used, s.vram_dedicated_budget);
    c.progress_bar(
        RECT { left: inner.left, top: y, right: inner.right, bottom: y + c.sc(20) },
        v_pct / 100.0,
        pressure_color(v_pct),
        &format!("Dedicated VRAM: {:.0} / {:.0} MB ({v_pct:.1}%)", mb_f(s.vram_dedicated_used), mb_f(s.vram_dedicated_budget)),
    );
    y += c.sc(24);
    let sh_pct = pct_f(s.vram_shared_used, s.vram_shared_budget);
    c.progress_bar(
        RECT { left: inner.left, top: y, right: inner.right, bottom: y + c.sc(20) },
        sh_pct / 100.0,
        palette::BLUE,
        &format!("Shared GPU Memory: {:.0} / {:.0} MB ({sh_pct:.1}%)", mb_f(s.vram_shared_used), mb_f(s.vram_shared_budget)),
    );
    y += c.sc(26);

    let rows: [(&str, String, u32); 13] = [
        ("Dedicated Reservable", format!("{:.0} MB", mb_f(s.vram_dedicated_avail_res)), palette::TEXT),
        ("Host Textures Alive", format!("{} ({:.1} MB)", s.host_textures_alive, mb_f(s.host_textures_bytes)), palette::TEXT),
        ("Placed Textures", format!("{}", s.placed_textures), palette::TEXT),
        ("Textures Evicted / Recreated", format!("{} ({:.0} MB) / {}", s.host_textures_evicted, mb_f(s.host_textures_evicted_bytes), s.host_textures_recreated), palette::TEXT),
        ("Bindless Tex Uploaded", format!("{} ({} MB)", s.heap_tex_uploaded, s.heap_tex_mb), palette::TEXT),
        ("Bindless Waiting / Unmapped", format!("{} / {}", s.heap_tex_waiting, s.heap_tex_away), palette::TEXT),
        ("Bindless Refused (VRAM Full)", format!("{}", s.heap_tex_full), if s.heap_tex_full > 0 { palette::AMBER } else { palette::GREEN }),
        ("Tex Queue (Total/Due/Partly)", format!("{} / {} / {} (oldest {} fr)", s.heap_q_total, s.heap_q_due, s.heap_q_partly, s.heap_q_oldest_frames), palette::TEXT),
        ("Levels-on-Demand Alive", format!("{} ({:.0} MB alloc, {:.0} MB saved)", s.tiles_alive, mb_f(s.tiles_allocated_bytes), mb_f(s.tiles_saved_bytes)), palette::CYAN),
        ("Staging Buffers Alive", format!("{} ({:.1} MB)", s.staging_alive, mb_f(s.staging_bytes)), palette::TEXT),
        ("Staging Spare Pool", format!("{:.1} MB", mb_f(s.staging_spare_bytes)), palette::TEXT),
        ("Driver Event Errors", format!("{} (tripped={})", s.driver_errors, s.driver_tripped != 0), if s.driver_errors > 0 { palette::RED } else { palette::GREEN }),
        ("Total Draws / Dropped", format!("{} / {}", s.total_draws, s.total_dropped), palette::TEXT),
    ];
    for (k, v, col) in rows {
        if y + row_h <= inner.bottom {
            c.kv_row(inner.left, y, w, row_h, k, &v, col);
            y += row_h;
        }
    }

    // Right: Buffers, Lists, Memory & Sync
    let inner = c.draw_card(
        RECT { left: area.left + col_w + gap, top: area.top, right: area.right, bottom: area.bottom },
        "SHADOW BUFFERS, HOST LISTS & SYNC",
        palette::CYAN,
    );
    let w = inner.right - inner.left;
    let mut y = inner.top;
    let os = &state.os;
    let right_rows: [(&str, String, u32); 16] = [
        ("Shadow Copies Alive", format!("{} ({:.1} MB)", s.shadow_copies_alive, mb_f(s.shadow_copies_bytes)), palette::TEXT),
        ("Loose Windows Outside Buffers", format!("{} ({:.1} MB)", s.shadow_windows_alive, mb_f(s.shadow_windows_bytes)), palette::TEXT),
        ("Card-Resident Buffer Copies", format!("{} ({:.1} MB)", s.card_buffers_alive, mb_f(s.card_buffers_bytes)), palette::TEXT),
        ("Card Before-Snapshot Twins", format!("{:.1} MB", mb_f(s.card_before_bytes)), palette::TEXT),
        ("Host Command Lists Alive / Made", format!("{} / {}", s.host_lists_alive, s.host_lists_made), palette::TEXT),
        ("List Scratch Chunks Alive", format!("{:.1} MB", mb_f(s.scratch_bytes)), palette::TEXT),
        ("Write-Watch Guard Faults", format!("{} total ({:.1}/s)", s.guard_faults_total, s.guard_faults_per_sec), palette::TEXT),
        ("Title Memory Remaps", format!("{} total ({:.1}/s, {:.1} ms/s wait)", s.remaps_total, s.remaps_per_sec, s.remap_wait_ms_per_sec), palette::TEXT),
        ("XMem Console Limit", format!("{:.1} GB", gb_f(s.xmem_title_limit_bytes)), palette::TEXT),
        ("XMem Physical Pages / Mapped", format!("{} pages ({:.1} MB)", s.xmem_physical_pages_used, mb_f(s.xmem_physical_bytes_mapped)), palette::TEXT),
        ("System RAM Used / Free", format!("{:.2} / {:.2} GB free", gb_f(os.ram_used_bytes), gb_f(os.ram_avail_bytes)), palette::TEXT),
        ("System Commit Used / Limit", format!("{:.2} / {:.2} GB", gb_f(os.commit_used_bytes), gb_f(os.commit_limit_bytes)), palette::TEXT),
        ("Process Working Set (Priv WS)", format!("{} ({})", fmt_bytes_short(os.proc_ws_bytes), fmt_bytes_short(os.proc_priv_ws_bytes)), palette::TEXT),
        ("Process Peak WS / Private", format!("{} / {}", fmt_bytes_short(os.proc_peak_ws_bytes), fmt_bytes_short(os.proc_private_bytes)), palette::TEXT),
        ("Page Fault Rate", format!("{:.0} faults/sec", os.page_faults_per_sec), palette::TEXT),
        ("OS Disk I/O Rate", format!("{:.2} MB/s R | {:.2} MB/s W", os.io_read_mb_s, os.io_write_mb_s), palette::TEXT),
    ];
    for (k, v, col) in right_rows {
        if y + row_h <= inner.bottom {
            c.kv_row(inner.left, y, w, row_h, k, &v, col);
            y += row_h;
        }
    }
    y += c.sc(6);
    if inner.bottom - y >= c.sc(40) {
        c.sparkline(
            RECT { left: inner.left, top: y, right: inner.right, bottom: inner.bottom },
            &state.vram_hist,
            100.0,
            palette::RED,
            "Dedicated VRAM % History (60s)",
        );
    }
}

fn draw_loading_cpu(c: &mut Canvas<'_>, area: RECT, state: &MonitorState) {
    let gap = c.sc(8);
    let top_h = ((area.bottom - area.top) * 48) / 100;
    let col_w = (area.right - area.left - gap) / 2;
    let s = &state.snap;
    let os = &state.os;
    let row_h = c.sc(18);

    // Top-Left: Boot, Shaders & Pipelines
    let inner = c.draw_card(
        RECT { left: area.left, top: area.top, right: area.left + col_w, bottom: area.top + top_h },
        "BOOT, SHADERS & PIPELINE COMPILATION",
        palette::BLUE,
    );
    let w = inner.right - inner.left;
    let mut y = inner.top;
    let p_done = s.pipelines_made + s.pipelines_loaded;
    let p_tot = p_done + s.pipelines_waiting;
    let p_frac = if p_tot > 0 { p_done as f64 / p_tot as f64 } else { 1.0 };
    c.progress_bar(
        RECT { left: inner.left, top: y, right: inner.right, bottom: y + c.sc(20) },
        p_frac,
        palette::BLUE,
        &format!("Pipelines: {p_done}/{p_tot} ({} queued, peak {})", s.pipelines_waiting, s.pipelines_peak_waiting),
    );
    y += c.sc(24);
    let sc_tot = s.shader_cache_hits + s.shader_cache_misses;
    let rows: [(&str, String); 8] = [
        ("Current Stage", format!("{} ({}%)", read_str(&s.stage, s.stage_len as usize), s.boot_percent), ),
        ("Time to First Frame", if s.first_frame_ms > 0 { format!("{:.2} s", s.first_frame_ms as f64 / 1000.0) } else { "pending".into() }),
        ("Title DLLs / Missing Imports", format!("{} DLLs / {} missing", s.title_dll_count, s.missing_imports)),
        ("Pipelines Made / Loaded / Early", format!("{} / {} / {}", s.pipelines_made, s.pipelines_loaded, s.pipelines_early)),
        ("Shader Cache Hits / Misses", format!("{} / {} ({:.1}% hit)", s.shader_cache_hits, s.shader_cache_misses, pct_f(s.shader_cache_hits, sc_tot))),
        ("Shader Containers Held", format!("{} ({:.1} MB)", s.shaders_containers, mb_f(s.shaders_container_bytes))),
        ("Translated DXIL Held", format!("{} ({:.1} MB)", s.shaders_translations, mb_f(s.shaders_translation_bytes))),
        ("Pipeline Library File / Saved", format!("{:.1} MB / {:.1} MB", mb_f(s.pipeline_lib_bytes), mb_f(s.pipeline_lib_saved_bytes))),
    ];
    for (k, v) in rows {
        if y + row_h <= inner.bottom {
            c.kv_row(inner.left, y, w, row_h, k, &v, palette::TEXT);
            y += row_h;
        }
    }

    // Top-Right: DirectStorage Streaming
    let inner = c.draw_card(
        RECT { left: area.left + col_w + gap, top: area.top, right: area.right, bottom: area.top + top_h },
        "DIRECTSTORAGE STREAMING & DECOMPRESSION",
        palette::CYAN,
    );
    let w = inner.right - inner.left;
    let mut y = inner.top;
    let stg_frac = pct_f(s.dstorage_staged_bytes, s.dstorage_staging_limit) / 100.0;
    c.progress_bar(
        RECT { left: inner.left, top: y, right: inner.right, bottom: y + c.sc(20) },
        stg_frac,
        palette::CYAN,
        &format!("Staging Buffer: {} / {}", fmt_bytes_short(s.dstorage_staged_bytes), fmt_bytes_short(s.dstorage_staging_limit)),
    );
    y += c.sc(24);
    let ds_rows: [(&str, String); 8] = [
        ("Active Queues / Workers", format!("{} queues / {} workers", s.dstorage_queues, s.dstorage_workers)),
        ("Worker Busy Equivalents", format!("{:.2} / {} workers", s.dstorage_busy_workers, s.dstorage_workers)),
        ("Queued / Running / Held Reads", format!("{} / {} / {}", s.dstorage_queued_reads, s.dstorage_running_reads, s.dstorage_held_reads)),
        ("Completed Reads", format!("{}", s.dstorage_done_reads)),
        ("Total Streamed Data", fmt_bytes_short(s.dstorage_done_bytes)),
        ("Instantaneous Read Rate", format!("{:.1} MB/s", s.dstorage_read_mb_s)),
        ("BCPack Decompressed", fmt_bytes_short(s.dstorage_bcpack_bytes)),
        ("Staging Stalls (Refused)", format!("{}", s.dstorage_staging_waits)),
    ];
    for (k, v) in ds_rows {
        if y + row_h <= inner.bottom {
            c.kv_row(inner.left, y, w, row_h, k, &v, palette::TEXT);
            y += row_h;
        }
    }

    // Bottom: Full Per-Core CPU Grid & Thread Telemetry
    let inner = c.draw_card(
        RECT { left: area.left, top: area.top + top_h + gap, right: area.right, bottom: area.bottom },
        "LOGICAL CPU CORES (P-CORES = CYAN, E-CORES = PURPLE) & THREAD AFFINITY",
        palette::PURPLE,
    );
    let mut y = inner.top;
    let summary = format!(
        "System {:.1}%  |  Game Process {:.1}% ({:.2} of {} logical cores)  |  Threads: {}  |  Slow-Core Looks — Render: {}/{}  Submit: {}/{}  Recording: {}/{}",
        os.sys_cpu_pct,
        os.proc_cpu_pct,
        os.proc_cores_equiv,
        os.logical_cores,
        os.thread_count,
        s.render_slow_looks,
        s.render_total_looks,
        s.worker_slow_looks,
        s.worker_total_looks,
        s.record_slow_looks,
        s.record_total_looks
    );
    c.text(
        RECT { left: inner.left, top: y, right: inner.right, bottom: y + c.sc(20) },
        &summary,
        fonts_mono_or_body(c),
        palette::TEXT,
        false,
    );
    y += c.sc(24);
    let sp_h = c.sc(52).min(((inner.bottom - y) * 32) / 100);
    c.core_grid(
        RECT { left: inner.left, top: y, right: inner.right, bottom: inner.bottom - sp_h - gap },
        &os.core_pcts,
        &state.sampler.topology,
        true,
    );
    if sp_h >= c.sc(24) {
        c.sparkline(
            RECT { left: inner.left, top: inner.bottom - sp_h, right: inner.right, bottom: inner.bottom },
            &state.proc_cpu_hist,
            100.0,
            palette::PURPLE,
            "Game Process CPU % History (60s)",
        );
    }
}

fn fonts_mono_or_body(c: &Canvas<'_>) -> HFONT {
    c.fonts.mono
}

fn draw_logs_env(c: &mut Canvas<'_>, area: RECT, state: &mut MonitorState) {
    let gap = c.sc(8);
    let env_h = c.sc(110);
    let s = &state.snap;

    let env_box = c.draw_card(
        RECT { left: area.left, top: area.top, right: area.right, bottom: area.top + env_h },
        "ENVIRONMENT SWITCHES, BUILD & CRASH CONTEXT",
        palette::AMBER,
    );
    let mut y = env_box.top;
    let rh = c.sc(18);
    let build = read_str(&s.build_label, s.build_label_len as usize);
    let hash = read_str(&s.build_hash, s.build_hash_len as usize);
    let flags = read_str(&s.flags_summary, s.flags_summary_len as usize);
    c.text(
        RECT { left: env_box.left, top: y, right: env_box.right, bottom: y + rh },
        &format!("Build: {build} ({hash})  |  TitleID: {:08X}  |  Storage Quota: {} MB  |  Missing Imports: {}", s.title_id, s.storage_quota_mb, s.missing_imports),
        c.fonts.mono,
        palette::TEXT,
        false,
    );
    y += rh + 2;
    c.text(
        RECT { left: env_box.left, top: y, right: env_box.right, bottom: y + rh },
        &format!("Env Flags: {}", if flags.is_empty() { "(defaults)" } else { flags }),
        c.fonts.mono,
        palette::CYAN,
        false,
    );
    y += rh + 2;
    if s.crash_summary_len > 0 {
        c.text(
            RECT { left: env_box.left, top: y, right: env_box.right, bottom: y + rh },
            &format!("Last Failure: {}", read_str(&s.crash_summary, s.crash_summary_len as usize)),
            c.fonts.mono,
            palette::RED,
            false,
        );
    }

    let log_box = c.draw_card(
        RECT { left: area.left, top: area.top + env_h + gap, right: area.right, bottom: area.bottom },
        "EVENT & WARNING LOG (mouse wheel to scroll)",
        palette::TEXT_DIM,
    );
    state.log_panel_rect = log_box;
    draw_log_lines(c, log_box, &state.logs, state.log_scroll);
}

fn draw_log_lines(c: &Canvas<'_>, r: RECT, logs: &VecDeque<LogEntry>, scroll: usize) {
    let rh = c.sc(16);
    if rh <= 0 || r.bottom <= r.top {
        return;
    }
    let visible = ((r.bottom - r.top) / rh).max(1) as usize;
    if logs.is_empty() {
        c.text(
            RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + rh },
            "Waiting for events...",
            c.fonts.mono,
            palette::TEXT_MUTED,
            false,
        );
        return;
    }
    let total = logs.len();
    let max_scroll = total.saturating_sub(visible);
    let end = total.saturating_sub(scroll.min(max_scroll));
    let start = end.saturating_sub(visible);
    let mut y = r.top;
    for entry in logs.iter().skip(start).take(end - start) {
        let lvl = LogLevel::from_raw(entry.level);
        let col = match lvl {
            LogLevel::Error => palette::RED,
            LogLevel::Warn => palette::AMBER,
            LogLevel::Info => palette::TEXT_DIM,
        };
        let tgt = read_str(&entry.target, entry.target_len as usize);
        let msg = read_str(&entry.message, entry.message_len as usize);
        let line = format!(
            "[{:>6.1}s] {} {:<20} {}",
            entry.uptime_ms as f64 / 1000.0,
            lvl.tag(),
            tgt,
            msg
        );
        c.text(
            RECT { left: r.left, top: y, right: r.right, bottom: y + rh },
            &line,
            c.fonts.mono,
            col,
            false,
        );
        y += rh;
    }
}

fn copy_to_clipboard(hwnd: HWND, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * size_of::<u16>();
    // SAFETY: Win32 clipboard copy with UTF-16 NUL-terminated global memory.
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return;
        }
        let _ = EmptyClipboard();
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if !mem.is_null() {
            let dst = GlobalLock(mem).cast::<u16>();
            if !dst.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
                let _ = GlobalUnlock(mem);
                let _ = SetClipboardData(CF_UNICODETEXT, mem);
            }
        }
        let _ = CloseClipboard();
    }
}

fn pt_in_rect(r: RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: reads pointer stored in `GWLP_USERDATA` for this window.
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MonitorState };

    match msg {
        WM_CREATE => {
            // SAFETY: `lparam` points to `CREATESTRUCTW` passed by `CreateWindowExW`.
            let cs = unsafe { &*(lparam as *const CREATESTRUCTW) };
            // SAFETY: stores owned state pointer into window user data.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize) };
            let dark: i32 = 1;
            // SAFETY: best-effort dark title bar on Windows 10/11.
            unsafe {
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_USE_IMMUTABLE_DARK_MODE,
                    (&raw const dark).cast(),
                    size_of_val(&dark) as u32,
                );
                let dpi = GetDpiForWindow(hwnd);
                if dpi > 0 && !cs.lpCreateParams.is_null() {
                    (*cs.lpCreateParams.cast::<MonitorState>()).dpi = dpi;
                }
                SetTimer(hwnd, TIMER_ID, TIMER_MS, None);
            }
            0
        }
        WM_ERASEBKGND => 1,
        WM_GETMINMAXINFO => {
            // SAFETY: valid `MINMAXINFO` pointer from Windows.
            let mmi = unsafe { &mut *(lparam as *mut MINMAXINFO) };
            mmi.ptMinTrackSize = POINT { x: 840, y: 540 };
            0
        }
        WM_SIZE => {
            // SAFETY: repaint at new dimensions.
            unsafe { let _ = InvalidateRect(hwnd, std::ptr::null(), 0); };
            0
        }
        WM_TIMER if wparam == TIMER_ID => {
            if !state_ptr.is_null() {
                // SAFETY: `state_ptr` lives until `WM_DESTROY`.
                unsafe { (*state_ptr).tick(hwnd) };
            }
            0
        }
        WM_MOUSEMOVE => {
            if !state_ptr.is_null() {
                let x = (lparam & 0xffff) as i16 as i32;
                let y = ((lparam >> 16) & 0xffff) as i16 as i32;
                // SAFETY: `state_ptr` is valid on the UI thread.
                let over = unsafe { &(*state_ptr).hits }
                    .iter()
                    .any(|h| pt_in_rect(h.rect, x, y));
                // SAFETY: sets standard cursor.
                unsafe {
                    let cursor = LoadCursorW(std::ptr::null_mut(), if over { IDC_HAND } else { IDC_ARROW });
                    SetCursor(cursor);
                }
            }
            0
        }
        WM_LBUTTONDOWN => {
            if !state_ptr.is_null() {
                let x = (lparam & 0xffff) as i16 as i32;
                let y = ((lparam >> 16) & 0xffff) as i16 as i32;
                // SAFETY: `state_ptr` is valid on the UI thread.
                let state = unsafe { &mut *state_ptr };
                if let Some(action) = state.hits.iter().find(|h| pt_in_rect(h.rect, x, y)).map(|h| h.action) {
                    apply_action(hwnd, state, action);
                }
            }
            0
        }
        WM_MOUSEWHEEL => {
            if !state_ptr.is_null() {
                let delta = ((wparam >> 16) & 0xffff) as i16;
                // SAFETY: `state_ptr` is valid on the UI thread.
                let state = unsafe { &mut *state_ptr };
                if delta > 0 {
                    state.log_scroll = (state.log_scroll + 3).min(state.logs.len());
                } else {
                    state.log_scroll = state.log_scroll.saturating_sub(3);
                }
                // SAFETY: repaint scrolled log view.
                unsafe { let _ = InvalidateRect(hwnd, std::ptr::null(), 0); };
            }
            0
        }
        WM_KEYDOWN => {
            if !state_ptr.is_null() {
                // SAFETY: `state_ptr` is valid on the UI thread.
                let state = unsafe { &mut *state_ptr };
                let action = match wparam as u32 {
                    0x31 => Some(Action::Tab(Tab::Overview)),
                    0x32 => Some(Action::Tab(Tab::GpuMem)),
                    0x33 => Some(Action::Tab(Tab::LoadingCpu)),
                    0x34 => Some(Action::Tab(Tab::LogsEnv)),
                    0x50 => Some(Action::TogglePause),
                    0x54 => Some(Action::ToggleTopmost),
                    0x4B => Some(Action::ToggleKeepOpen),
                    0x43 => Some(Action::CopyReport),
                    _ => None,
                };
                if let Some(action) = action {
                    apply_action(hwnd, state, action);
                }
            }
            0
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: standard double-buffered Win32 GDI paint sequence.
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                if !state_ptr.is_null() && client.right > 0 && client.bottom > 0 {
                    let state = &mut *state_ptr;
                    let dpi = GetDpiForWindow(hwnd);
                    if dpi > 0 {
                        state.dpi = dpi;
                    }
                    let fonts = Fonts::create(state.dpi);
                    let mem_dc = CreateCompatibleDC(hdc);
                    let bmp = CreateCompatibleBitmap(hdc, client.right, client.bottom);
                    let old_bmp = SelectObject(mem_dc, bmp.cast());
                    render(mem_dc, client, state, &fonts);
                    let _ = BitBlt(hdc, 0, 0, client.right, client.bottom, mem_dc, 0, 0, SRCCOPY);
                    let _ = SelectObject(mem_dc, old_bmp);
                    let _ = DeleteObject(bmp.cast());
                    let _ = DeleteDC(mem_dc);
                }
                let _ = EndPaint(hwnd, &ps);
            }
            0
        }
        WM_DESTROY => {
            // SAFETY: kills timer and exits message loop.
            unsafe {
                let _ = KillTimer(hwnd, TIMER_ID);
                PostQuitMessage(0);
            }
            0
        }
        // SAFETY: forwards unhandled window messages to default proc.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn apply_action(hwnd: HWND, state: &mut MonitorState, action: Action) {
    match action {
        Action::Tab(tab) => {
            state.active_tab = tab;
            state.log_scroll = 0;
        }
        Action::TogglePause => state.paused = !state.paused,
        Action::ToggleTopmost => {
            state.topmost = !state.topmost;
            let insert_after = if state.topmost { HWND_TOPMOST } else { HWND_NOTOPMOST };
            // SAFETY: toggles topmost flag without moving or resizing.
            unsafe { let _ = SetWindowPos(hwnd, insert_after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE); };
        }
        Action::ToggleKeepOpen => state.keep_open = !state.keep_open,
        Action::CopyReport => {
            let report = state.build_clipboard_report();
            copy_to_clipboard(hwnd, &report);
            state.copied_until = Some(Instant::now() + Duration::from_secs(2));
        }
    }
    // SAFETY: immediate redraw after interactive action.
    unsafe { let _ = InvalidateRect(hwnd, std::ptr::null(), 0); };
}

pub fn run(pid: u32) -> Result<()> {
    // SAFETY: enable per-monitor DPI awareness before creating window.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let mut state = Box::new(MonitorState::new(pid));
    state.os = state.sampler.poll().clone();

    let class_name: Vec<u16> = "VIctoryDebugMonitorClass\0".encode_utf16().collect();
    let title: Vec<u16> = format!("VIctory Debug Monitor (PID {pid})\0")
        .encode_utf16()
        .collect();

    // SAFETY: registers window class and creates visible companion monitor window.
    let hwnd = unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance,
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: GetStockObject(BLACK_BRUSH),
            lpszClassName: class_name.as_ptr(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let mut area = RECT::default();
        let has_area = SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut area).cast(), 0) != 0;
        let (aw, ah) = if has_area {
            ((area.right - area.left).max(900), (area.bottom - area.top).max(600))
        } else {
            (1920, 1080)
        };
        let win_w = ((aw * 62) / 100).clamp(960, 1680);
        let win_h = ((ah * 76) / 100).clamp(640, 1120);
        let win_x = if has_area { area.right - win_w - 24 } else { CW_USEDEFAULT };
        let win_y = if has_area { area.top + ((ah - win_h) / 2).max(16) } else { CW_USEDEFAULT };
        let state_raw: *mut MonitorState = &mut *state;
        let hwnd = CreateWindowExW(
            WS_EX_APPWINDOW,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            win_x,
            win_y,
            win_w,
            win_h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            state_raw.cast(),
        );
        if hwnd.is_null() {
            anyhow::bail!("CreateWindowExW failed: {}", std::io::Error::last_os_error());
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        hwnd
    };

    state.tick(hwnd);

    let mut msg = MSG::default();
    // SAFETY: standard Win32 message loop for the monitor window.
    unsafe {
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }
    drop(state);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_stats_computes_percentiles_and_one_percent_low() {
        let mut ring = VecDeque::new();
        for us in [10_000u32, 20_000, 30_000, 40_000, 100_000] {
            ring.push_back(us);
        }
        let fs = compute_frame_stats(&ring);
        assert!((fs.min_ms - 10.0).abs() < 0.01);
        assert!((fs.med_ms - 30.0).abs() < 0.01);
        assert!((fs.avg_ms - 40.0).abs() < 0.01);
        assert!((fs.max_ms - 100.0).abs() < 0.01);
        assert!((fs.one_pct_low_fps - 10.0).abs() < 0.01);
        assert!((fs.rolling_fps - 25.0).abs() < 0.01);
    }
}
