//! Periodic non-blocking telemetry sampler for the companion debug monitor.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use victory_core::telemetry::{self, write_str};

/// HLE-side stats supplied by `victory-hle` without introducing a crate cycle.
#[derive(Clone, Copy, Default)]
pub struct HleSnapshot {
    pub dstorage_queues: u32,
    pub dstorage_workers: u32,
    pub dstorage_queued_reads: u64,
    pub dstorage_running_reads: u64,
    pub dstorage_held_reads: u64,
    pub dstorage_done_reads: u64,
    pub dstorage_done_bytes: u64,
    pub dstorage_bcpack_bytes: u64,
    pub dstorage_staged_bytes: u64,
    pub dstorage_staging_limit: u64,
    pub dstorage_staging_waits: u64,
    pub xmem_title_limit_bytes: u64,
    pub xmem_physical_pages_used: u64,
    pub xmem_physical_bytes_mapped: u64,
}

static HLE_SAMPLER: OnceLock<fn() -> HleSnapshot> = OnceLock::new();

/// Registers the HLE stats callback (called once from `victory-hle`).
pub fn register_hle_sampler(f: fn() -> HleSnapshot) {
    let _ = HLE_SAMPLER.set(f);
}

#[derive(Clone, Copy, Default)]
struct MeterState {
    fps_1s: f64,
    frame_ms_1s: f64,
    draws_per_frame: f64,
    dropped_per_frame: f64,
    indirect_per_frame: f64,
    cpu_prepare_ms: f64,
    gpu_wait_ms: f64,
    copy_up_ms: f64,
    copy_up_mb: f64,
    tex_refresh_ms: f64,
    tex_refresh_mb: f64,
    copy_back_ms: f64,
    copy_back_mb: f64,
    card_idle_ms: f64,
    card_idle_behind_ms: f64,
    card_busy_pct: f64,
}

static METER: Mutex<MeterState> = Mutex::new(MeterState {
    fps_1s: 0.0,
    frame_ms_1s: 0.0,
    draws_per_frame: 0.0,
    dropped_per_frame: 0.0,
    indirect_per_frame: 0.0,
    cpu_prepare_ms: 0.0,
    gpu_wait_ms: 0.0,
    copy_up_ms: 0.0,
    copy_up_mb: 0.0,
    tex_refresh_ms: 0.0,
    tex_refresh_mb: 0.0,
    copy_back_ms: 0.0,
    copy_back_mb: 0.0,
    card_idle_ms: 0.0,
    card_idle_behind_ms: 0.0,
    card_busy_pct: 0.0,
});

static TOTAL_DROPPED: AtomicU64 = AtomicU64::new(0);
static ADAPTER_INFO: Mutex<(String, u32, u32, bool)> = Mutex::new((String::new(), 0, 0, false));

pub(crate) fn set_device_info(adapter: &str, wave_min: u32, wave_max: u32, gpu_upload: bool) {
    if let Ok(mut info) = ADAPTER_INFO.lock() {
        *info = (adapter.to_owned(), wave_min, wave_max, gpu_upload);
    }
    if telemetry::is_active() {
        telemetry::update(|s| {
            write_str(&mut s.adapter_name, &mut s.adapter_name_len, adapter);
            s.wave_min = wave_min;
            s.wave_max = wave_max;
            s.gpu_upload_heap = u8::from(gpu_upload);
        });
    }
}

pub(crate) fn on_present(presented_total: u64, begun_total: u64, width: u32, height: u32) {
    if telemetry::is_active() {
        telemetry::record_frame(presented_total, begun_total, width, height);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_meter(
    fps_1s: f64,
    frame_ms_1s: f64,
    draws_per_frame: f64,
    dropped_per_frame: f64,
    dropped_raw: u64,
    indirect_per_frame: f64,
    cpu_prepare_ms: f64,
    gpu_wait_ms: f64,
    copy_up_ms: f64,
    copy_up_mb: f64,
    tex_refresh_ms: f64,
    tex_refresh_mb: f64,
    copy_back_ms: f64,
    copy_back_mb: f64,
    card_idle_ms: f64,
    card_idle_behind_ms: f64,
    card_busy_pct: f64,
) {
    TOTAL_DROPPED.fetch_add(dropped_raw, Relaxed);
    if !telemetry::is_active() {
        return;
    }
    if let Ok(mut m) = METER.try_lock() {
        *m = MeterState {
            fps_1s,
            frame_ms_1s,
            draws_per_frame,
            dropped_per_frame,
            indirect_per_frame,
            cpu_prepare_ms,
            gpu_wait_ms,
            copy_up_ms,
            copy_up_mb,
            tex_refresh_ms,
            tex_refresh_mb,
            copy_back_ms,
            copy_back_mb,
            card_idle_ms,
            card_idle_behind_ms,
            card_busy_pct,
        };
    }
}

/// Starts the 4 Hz telemetry sampler thread once when telemetry is active.
pub fn start() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    if !telemetry::is_active() {
        return;
    }
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("victory-telemetry".into())
            .spawn(sample_loop);
    });
}

fn sample_loop() {
    use crate::framelog::T;

    let mut last_tick = Instant::now();
    let mut prev_fl = crate::framelog::snapshot();
    let mut prev_disk_bytes = 0u64;

    loop {
        std::thread::sleep(Duration::from_millis(250));
        if !telemetry::is_active() {
            return;
        }
        let now = Instant::now();
        let dt = now.duration_since(last_tick).as_secs_f64().max(0.05);
        last_tick = now;

        let fl = crate::framelog::snapshot();
        let delta = |t: T| fl[t as usize].saturating_sub(prev_fl[t as usize]);
        let guard_faults_s = delta(T::GuardFaults) as f64 / dt;
        let remaps_s = delta(T::Remaps) as f64 / dt;
        let remap_wait_ms_s = (delta(T::RemapWaitUs) as f64 / 1000.0) / dt;
        let disk_us_delta = delta(T::DiskUs);
        let pipe_wait_us_delta = delta(T::PipelineWaitUs);
        let pace_us_delta = delta(T::PaceUs);
        let show_us_delta = delta(T::PresentShowUs);
        prev_fl = fl;

        let meter = METER.lock().map(|m| *m).unwrap_or_default();
        let (made, loaded, early, waiting, peak_waiting) = crate::pipeline::telemetry_counts();
        let kept = crate::pipeline::kept();
        let (sc_hits, sc_misses) = crate::shader_cache::counts();
        let host_tex = [0, 1].map(|i| crate::resource::HOST_TEXTURES[i].load(Relaxed));
        let kinds = [2, 3, 4].map(|i| crate::resource::KINDS[i].load(Relaxed));
        let placed_tex = crate::resource::placed_textures_try();
        let (sh_live, sh_live_b, sh_win, sh_win_b, sh_card, sh_card_b, sh_before_b) =
            crate::shadow::telemetry_memory();
        let (st_count, st_bytes, st_spare) = crate::texture::telemetry_memory();
        let lists_alive = crate::host::ALIVE[0].load(Relaxed);
        let scratch_bytes = crate::host::ALIVE[2].load(Relaxed);
        let lists_made = crate::host::LISTS[0].load(Relaxed);
        let vram_groups = crate::host::host().and_then(|h| h.video_memory_groups());
        let hle = HLE_SAMPLER.get().map(|f| f()).unwrap_or_default();
        let disk_rate_mb_s = (hle.dstorage_done_bytes.saturating_sub(prev_disk_bytes) as f64 / 1e6) / dt;
        prev_disk_bytes = hle.dstorage_done_bytes;

        let draws_total = crate::draw::drawn();
        let dropped_total = TOTAL_DROPPED.load(Relaxed);
        let heap_uploaded = crate::draw::HEAP_TEXTURES.load(Relaxed);
        let heap_mb = crate::draw::HEAP_TEXTURE_BYTES.load(Relaxed) >> 20;
        let heap_waiting = crate::draw::HEAP_TEXTURES_WAITING.load(Relaxed);
        let heap_away = crate::draw::HEAP_TEXTURES_AWAY.load(Relaxed);
        let heap_full = crate::draw::HEAP_TEXTURES_FULL.load(Relaxed);
        let hq_total = crate::draw::HEAP_TEXTURES_QUEUE[0].load(Relaxed);
        let hq_due = crate::draw::HEAP_TEXTURES_QUEUE[1].load(Relaxed);
        let hq_partly = crate::draw::HEAP_TEXTURES_QUEUE[3].load(Relaxed);
        let hq_oldest = crate::draw::HEAP_TEXTURES_QUEUE[4].load(Relaxed);
        let tiles_alive = crate::tiles::STATS[0].load(Relaxed);
        let tiles_full_bytes = crate::tiles::STATS[1].load(Relaxed);
        let tiles_alloc_bytes = crate::tiles::STATS[2].load(Relaxed);
        let drv_errors = crate::drvwatch::errors();
        let drv_tripped = crate::drvwatch::tripped();
        let adapter_info = ADAPTER_INFO.lock().ok().map(|a| a.clone());

        let frames_in_window = (meter.fps_1s * dt).max(1.0);
        let per_frame_ms = |us: u64| (us as f64 / 1000.0) / frames_in_window;

        telemetry::update(|s| {
            if let Some((ref name, wmin, wmax, up)) = adapter_info
                && s.adapter_name_len == 0
                && !name.is_empty()
            {
                write_str(&mut s.adapter_name, &mut s.adapter_name_len, name);
                s.wave_min = wmin;
                s.wave_max = wmax;
                s.gpu_upload_heap = u8::from(up);
            }
            s.driver_errors = drv_errors;
            s.driver_tripped = u8::from(drv_tripped);
            s.total_frames_begun = crate::frame::current();
            s.fps_1s = meter.fps_1s;
            s.frame_ms_1s = meter.frame_ms_1s;
            s.draws_per_frame = meter.draws_per_frame;
            s.dropped_per_frame = meter.dropped_per_frame;
            s.indirect_per_frame = meter.indirect_per_frame;
            s.cpu_prepare_ms = meter.cpu_prepare_ms;
            s.gpu_wait_ms = meter.gpu_wait_ms;
            s.copy_up_ms = meter.copy_up_ms;
            s.copy_up_mb = meter.copy_up_mb;
            s.tex_refresh_ms = meter.tex_refresh_ms;
            s.tex_refresh_mb = meter.tex_refresh_mb;
            s.copy_back_ms = meter.copy_back_ms;
            s.copy_back_mb = meter.copy_back_mb;
            s.card_idle_ms = meter.card_idle_ms;
            s.card_idle_behind_ms = meter.card_idle_behind_ms;
            s.card_busy_pct = meter.card_busy_pct;
            s.present_show_ms = per_frame_ms(show_us_delta);
            s.pipeline_wait_ms = per_frame_ms(pipe_wait_us_delta);
            s.pace_ms = per_frame_ms(pace_us_delta);
            s.total_draws = draws_total;
            s.total_dropped = dropped_total;

            s.heap_tex_uploaded = heap_uploaded;
            s.heap_tex_mb = heap_mb;
            s.heap_tex_waiting = heap_waiting;
            s.heap_tex_away = heap_away;
            s.heap_tex_full = heap_full;
            s.heap_q_total = hq_total;
            s.heap_q_due = hq_due;
            s.heap_q_partly = hq_partly;
            s.heap_q_oldest_frames = hq_oldest;

            s.pipelines_made = made;
            s.pipelines_loaded = loaded;
            s.pipelines_early = early;
            s.pipelines_waiting = waiting;
            s.pipelines_peak_waiting = peak_waiting;
            s.shaders_containers = kept[0];
            s.shaders_container_bytes = kept[1];
            s.shaders_translations = kept[2];
            s.shaders_translation_bytes = kept[3];
            s.shader_cache_hits = sc_hits;
            s.shader_cache_misses = sc_misses;
            s.pipeline_lib_bytes = kept[4];
            s.pipeline_lib_saved_bytes = kept[5];

            s.dstorage_queues = hle.dstorage_queues;
            s.dstorage_workers = hle.dstorage_workers;
            s.dstorage_queued_reads = hle.dstorage_queued_reads;
            s.dstorage_running_reads = hle.dstorage_running_reads;
            s.dstorage_held_reads = hle.dstorage_held_reads;
            s.dstorage_done_reads = hle.dstorage_done_reads;
            s.dstorage_done_bytes = hle.dstorage_done_bytes;
            s.dstorage_read_mb_s = disk_rate_mb_s;
            s.dstorage_busy_workers = (disk_us_delta as f64 / 1e6) / dt;
            s.dstorage_bcpack_bytes = hle.dstorage_bcpack_bytes;
            s.dstorage_staged_bytes = hle.dstorage_staged_bytes;
            s.dstorage_staging_limit = hle.dstorage_staging_limit;
            s.dstorage_staging_waits = hle.dstorage_staging_waits;

            if let Some([[own_b, own_u, own_avail, _], [sh_b, sh_u, ..]]) = vram_groups {
                s.vram_dedicated_budget = own_b;
                s.vram_dedicated_used = own_u;
                s.vram_dedicated_avail_res = own_avail;
                s.vram_shared_budget = sh_b;
                s.vram_shared_used = sh_u;
            }

            s.host_textures_alive = host_tex[0];
            s.host_textures_bytes = host_tex[1];
            s.placed_textures = placed_tex;
            s.host_textures_evicted = kinds[0];
            s.host_textures_evicted_bytes = kinds[1];
            s.host_textures_recreated = kinds[2];
            s.shadow_copies_alive = sh_live;
            s.shadow_copies_bytes = sh_live_b;
            s.shadow_windows_alive = sh_win;
            s.shadow_windows_bytes = sh_win_b;
            s.card_buffers_alive = sh_card;
            s.card_buffers_bytes = sh_card_b;
            s.card_before_bytes = sh_before_b;
            s.staging_alive = st_count;
            s.staging_bytes = st_bytes;
            s.staging_spare_bytes = st_spare;
            s.host_lists_alive = lists_alive;
            s.host_lists_made = lists_made;
            s.scratch_bytes = scratch_bytes;
            s.tiles_alive = tiles_alive;
            s.tiles_allocated_bytes = tiles_alloc_bytes;
            s.tiles_saved_bytes = tiles_full_bytes.saturating_sub(tiles_alloc_bytes);

            s.guard_faults_total = fl[T::GuardFaults as usize];
            s.guard_faults_per_sec = guard_faults_s;
            s.remaps_total = fl[T::Remaps as usize];
            s.remaps_per_sec = remaps_s;
            s.remap_wait_ms_per_sec = remap_wait_ms_s;
            s.render_slow_looks = fl[T::RenderSlow as usize];
            s.render_total_looks = fl[T::RenderLooks as usize];
            s.worker_slow_looks = fl[T::WorkerSlow as usize];
            s.worker_total_looks = fl[T::WorkerLooks as usize];
            s.record_slow_looks = fl[T::RecordSlow as usize];
            s.record_total_looks = fl[T::RecordLooks as usize];

            s.xmem_title_limit_bytes = hle.xmem_title_limit_bytes;
            s.xmem_physical_pages_used = hle.xmem_physical_pages_used;
            s.xmem_physical_bytes_mapped = hle.xmem_physical_bytes_mapped;
        });
    }
}
