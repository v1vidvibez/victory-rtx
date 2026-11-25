//! Boot/pipeline progress status helpers for external telemetry.

/// Updates the startup progress in the shared telemetry snapshot.
pub fn set_stage(stage: &str, detail: &str, percent: u32) {
    let phase = if percent >= 40 {
        victory_core::telemetry::Phase::GpuInit
    } else if percent >= 20 {
        victory_core::telemetry::Phase::Loading
    } else {
        victory_core::telemetry::Phase::Preparing
    };
    victory_core::telemetry::set_stage(phase, stage, detail, percent);
}

/// Starts GPU telemetry tracking once title execution begins.
pub fn enter_gpu_phase() {
    crate::telemetry::start();
}

/// Returns `(done, total, percent, hits, misses)` if pipelines are queued
/// or recently finished.
pub(crate) fn status() -> Option<(u64, u64, u32, u64, u64)> {
    let (made, loaded) = crate::pipeline::counts();
    let waiting = crate::pipeline::making();
    let (hits, misses) = crate::shader_cache::counts();
    let done = made + loaded;
    let total = done + waiting;
    if total == 0 {
        return None;
    }
    let pct = ((done * 100) / total).min(100) as u32;
    Some((done, total, pct, hits, misses))
}
