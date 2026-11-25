//! In-window progress overlay, drawn into the bottom-left corner of the
//! presented frame while pipelines and shaders are being built.
//!
//! Uses the same upload-buffer + `CopyTextureRegion` approach as `fpsbox.rs`
//! so there is only a single game window and zero impact on title render
//! targets. Disable with `VICTORY_PROGRESS=0`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PROGRESS").is_none_or(|v| v != "0"))
}

/// Updates the startup progress in both the telemetry monitor and log state.
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

fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110],
        'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        '%' => [0b11001, 0b11010, 0b00010, 0b00100, 0b01000, 0b01011, 0b10011],
        '/' => [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000],
        ':' => [0b00000, 0b01100, 0b01100, 0b00000, 0b01100, 0b01100, 0b00000],
        '.' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100],
        _ => [0; 7],
    }
}

fn pixel_bytes(format: DXGI_FORMAT) -> Option<usize> {
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_R10G10B10A2_UNORM => Some(4),
        DXGI_FORMAT_R16G16B16A16_FLOAT => Some(8),
        _ => None,
    }
}

fn half(v: f32) -> u16 {
    let bits = v.clamp(0.0, 1.0).to_bits();
    if v <= 0.0 {
        return 0;
    }
    let exponent = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
    if exponent <= 0 {
        return 0;
    }
    ((exponent as u32) << 10 | (bits >> 13) & 0x3FF) as u16
}

#[derive(Clone, Copy)]
enum Tone {
    Bg,
    Text,
    Track,
    Fill,
}

fn pixel(format: DXGI_FORMAT, tone: Tone, out: &mut [u8]) {
    let (r, g, b): (f32, f32, f32) = match tone {
        Tone::Bg => (0.04, 0.04, 0.05),
        Tone::Text => (0.95, 0.96, 0.98),
        Tone::Track => (0.16, 0.18, 0.22),
        Tone::Fill => (0.23, 0.51, 0.96),
    };
    let byte = |v: f32| (v * 255.0).round() as u8;
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM => out.copy_from_slice(&[byte(r), byte(g), byte(b), 255]),
        DXGI_FORMAT_B8G8R8A8_UNORM => out.copy_from_slice(&[byte(b), byte(g), byte(r), 255]),
        DXGI_FORMAT_R10G10B10A2_UNORM => {
            let ten = |v: f32| (v * 1023.0).round() as u32;
            out.copy_from_slice(&(ten(r) | ten(g) << 10 | ten(b) << 20 | 3 << 30).to_le_bytes());
        }
        _ => {
            for (i, v) in [r, g, b, 1.0].into_iter().enumerate() {
                out[i * 2..i * 2 + 2].copy_from_slice(&half(v).to_le_bytes());
            }
        }
    }
}

fn bitmap(text: &str, pct: u32, scale: u32) -> (u32, u32, Vec<Tone>) {
    let chars: Vec<char> = text.chars().collect();
    let pad = 2 * scale;
    let bar_h = 3 * scale;
    let gap = 2 * scale;
    let width = 2 * pad + chars.len() as u32 * 6 * scale - scale;
    let height = 2 * pad + 7 * scale + gap + bar_h;
    let mut tones = vec![Tone::Bg; (width * height) as usize];

    for (i, &c) in chars.iter().enumerate() {
        let rows = glyph(c);
        for (row, bits) in rows.into_iter().enumerate() {
            for column in 0..5u32 {
                if bits >> (4 - column) & 1 == 0 {
                    continue;
                }
                let (x0, y0) = (pad + (i as u32 * 6 + column) * scale, pad + row as u32 * scale);
                for y in y0..y0 + scale {
                    for x in x0..x0 + scale {
                        tones[(y * width + x) as usize] = Tone::Text;
                    }
                }
            }
        }
    }

    let bar_x0 = pad;
    let bar_x1 = width - pad;
    let bar_w = bar_x1.saturating_sub(bar_x0);
    let fill_x1 = bar_x0 + (bar_w * pct.min(100)) / 100;
    let bar_y0 = pad + 7 * scale + gap;
    let bar_y1 = bar_y0 + bar_h;
    for y in bar_y0..bar_y1 {
        for x in bar_x0..bar_x1 {
            tones[(y * width + x) as usize] = if x < fill_x1 { Tone::Fill } else { Tone::Track };
        }
    }

    (width, height, tones)
}

struct Made {
    buffer: ID3D12Resource,
    width: u32,
    height: u32,
    pitch: u32,
}

// SAFETY: a free-threaded D3D12 object and plain numbers.
unsafe impl Send for Made {}

static MADE: Mutex<Option<HashMap<(String, i32, u32), Made>>> = Mutex::new(None);
static LAST_ACTIVE_MS: AtomicU64 = AtomicU64::new(0);

/// Draws a compact loading/pipeline progress bar in the bottom-left corner of
/// `back` while pipelines are queued (and for 1.5 s after they finish).
pub(crate) fn draw(
    host: &crate::host::Host,
    list: &ID3D12GraphicsCommandList,
    back: &ID3D12Resource,
    (width, height, format): (u32, u32, DXGI_FORMAT),
) {
    if !on() {
        return;
    }
    let waiting = crate::pipeline::making();
    let now = crate::host::now_ms();
    if waiting > 0 {
        LAST_ACTIVE_MS.store(now, Ordering::Relaxed);
    } else {
        let last = LAST_ACTIVE_MS.load(Ordering::Relaxed);
        if last == 0 || now.saturating_sub(last) > 1500 {
            return;
        }
    }
    let Some((done, total, pct, hits, misses)) = status() else {
        return;
    };
    let Some(bytes) = pixel_bytes(format) else {
        return;
    };
    let label = format!(
        "LOADING {pct}%  PIPELINES {done}/{total}  SHADERS {}",
        hits + misses
    );
    let scale = (height / 540).clamp(2, 6);
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    let made = made.get_or_insert_with(HashMap::new);
    if made.len() > 256 {
        made.clear();
    }
    let key = (label.clone(), format.0, scale);
    if !made.contains_key(&key) {
        let (w, h, tones) = bitmap(&label, pct, scale);
        let pitch = (w as usize * bytes).next_multiple_of(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT as usize);
        let Some((buffer, data)) = host.shared_buffer((pitch * h as usize) as u64) else {
            return;
        };
        // SAFETY: the buffer's own mapped bytes, `pitch * h` of them.
        let out = unsafe { std::slice::from_raw_parts_mut(data, pitch * h as usize) };
        for y in 0..h as usize {
            for x in 0..w as usize {
                let at = y * pitch + x * bytes;
                pixel(format, tones[y * w as usize + x], &mut out[at..at + bytes]);
            }
        }
        made.insert(
            key.clone(),
            Made {
                buffer,
                width: w,
                height: h,
                pitch: pitch as u32,
            },
        );
    }
    let Some(m) = made.get(&key) else {
        return;
    };
    let margin = 6 * scale;
    if m.width + 2 * margin > width || m.height + 2 * margin > height {
        return;
    }
    let dst_y = height - m.height - margin;
    // SAFETY: an open list; `back` is a copy destination and our upload buffer outlives the copy.
    unsafe {
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&m.buffer),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: format,
                        Width: m.width,
                        Height: m.height,
                        Depth: 1,
                        RowPitch: m.pitch,
                    },
                },
            },
        };
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(back),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        list.CopyTextureRegion(&to, margin, dst_y, 0, &from, None);
    }
}
