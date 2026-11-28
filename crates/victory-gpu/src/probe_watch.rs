//! The indirect-light volumes (32x16x64 froxel grids: a state volume in R16_FLOAT and the pairs of
//! R11G11B10_FLOAT and R8G8B8A8 volumes the probe passes write) looked at over time. With
//! `VICTORY_PROBE_WATCH=<seconds>` (diagnostics builds), every that many seconds at a present the
//! volumes the title has written through unordered access views are read back (a few hundred KiB)
//! and one line per volume says what they hold: how many voxels of the state volume are -1, -2 or
//! something else, and, for each light volume, how many voxels are exactly zero and how the
//! magnitudes of the voxels the state volume marks -1 are spread. The question it answers: does
//! the zero share of those voxels grow from 0 over minutes (a loop that erodes), or jump?

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::Win32::Graphics::Direct3D12::*;

use crate::object::This;
use crate::resource::HostTexture;

const WIDTH: u64 = 32;
const HEIGHT: u32 = 16;
const DEPTH: u16 = 64;
const R16_FLOAT: i32 = 54;
const R11G11B10_FLOAT: i32 = 26;
const R8G8B8A8_UNORM: i32 = 28;

/// Seconds between samples (`VICTORY_PROBE_WATCH`), 0: off.
fn period() -> u64 {
    static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| {
        std::env::var("VICTORY_PROBE_WATCH")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    })
}

struct Volume {
    resource: usize,
    copy: Arc<HostTexture>,
    format: i32,
}

#[derive(Default)]
struct Book {
    /// Resources looked at once (a volume or not).
    seen: std::collections::HashSet<usize>,
    volumes: Vec<Volume>,
}

static BOOK: Mutex<Option<Book>> = Mutex::new(None);
static LAST: AtomicU64 = AtomicU64::new(0);

fn clock() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs()
}

/// A title texture a dispatch writes through an unordered access view: kept if it is one of the volumes.
pub(crate) fn note(resource: This) {
    if resource.is_null() || period() == 0 {
        return;
    }
    let mut book = BOOK.lock().unwrap_or_else(|e| e.into_inner());
    let book = book.get_or_insert_with(Book::default);
    if !book.seen.insert(resource as usize) {
        return;
    }
    let Some(copy) = crate::resource::host_copy(resource) else {
        return;
    };
    // SAFETY: a live texture.
    let d = unsafe { copy.texture.GetDesc() };
    let volume = d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D
        && d.Width == WIDTH
        && d.Height == HEIGHT
        && d.DepthOrArraySize == DEPTH
        && matches!(d.Format.0, R16_FLOAT | R11G11B10_FLOAT | R8G8B8A8_UNORM);
    if volume && book.volumes.len() < 16 {
        tracing::info!(
            resource = format_args!("{:#x}", resource as usize),
            format = d.Format.0,
            "probe watch: a volume the title writes"
        );
        book.volumes.push(Volume {
            resource: resource as usize,
            copy,
            format: d.Format.0,
        });
    }
}

/// Whether the volumes are looked at at this present.
pub(crate) fn due(_frame: u64) -> bool {
    let every = period();
    if every == 0 {
        return false;
    }
    let now = clock();
    let last = LAST.load(Ordering::Relaxed);
    if now < last + every {
        return false;
    }
    let have = BOOK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|b| b.volumes.iter().any(|v| v.format == R16_FLOAT));
    if have {
        LAST.store(now, Ordering::Relaxed);
    }
    have
}

/// The bytes of a volume, `Depth * Height` rows of `pitch` bytes.
fn read(host: &crate::host::Host, volume: &Volume) -> Option<(Vec<u8>, usize)> {
    let texture = &volume.copy.texture;
    let list = host.new_list().ok()?;
    let state = D3D12_RESOURCE_STATES(crate::host::legal_state(
        texture,
        volume.copy.state.load(Ordering::Relaxed),
    ) as i32);
    let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
    // SAFETY: an open list of ours and a live texture.
    let read = unsafe {
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, state, copy)]);
        }
        let read = crate::capture::read_back(&host.device, &list.list, texture);
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, copy, state)]);
        }
        read
    };
    list.close();
    host.execute(&[&list]);
    let (buffer, footprint) = read?;
    let pitch = footprint.Footprint.RowPitch as usize;
    let rows = pitch * HEIGHT as usize * usize::from(DEPTH);
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished; the mapping spans the footprint's rows.
    unsafe {
        buffer.Map(0, None, Some(&mut data)).ok()?;
        let bytes = std::slice::from_raw_parts(data.cast::<u8>(), rows).to_vec();
        buffer.Unmap(0, None);
        Some((bytes, pitch))
    }
}

/// An unsigned small float (5 exponent bits, `man` mantissa bits) as a number.
fn small(bits: u32, man: u32) -> f32 {
    let e = bits >> man;
    let m = bits & ((1 << man) - 1);
    match e {
        0 => m as f32 / (1 << man) as f32 * 2f32.powi(-14),
        31 => f32::INFINITY,
        _ => (1.0 + m as f32 / (1 << man) as f32) * 2f32.powi(e as i32 - 15),
    }
}

fn texel(bytes: &[u8], pitch: usize, size: usize, x: usize, y: usize, z: usize) -> u32 {
    let at = (z * HEIGHT as usize + y) * pitch + x * size;
    match size {
        2 => u32::from(u16::from_le_bytes([bytes[at], bytes[at + 1]])),
        _ => u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]),
    }
}

/// Reads the volumes back and logs them (called at a present, after the submissions ran).
pub(crate) fn sample(frame: u64) {
    let Some(host) = crate::host::host() else {
        return;
    };
    let volumes: Vec<(usize, Arc<HostTexture>, i32)> = {
        let book = BOOK.lock().unwrap_or_else(|e| e.into_inner());
        book.as_ref()
            .map(|b| {
                b.volumes
                    .iter()
                    .map(|v| (v.resource, v.copy.clone(), v.format))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut state: Option<Vec<bool>> = None;
    let mut minus1 = [0u32; 3];
    // The state volume first: the others are looked at by what it says.
    for (resource, copy, format) in volumes.iter().filter(|v| v.2 == R16_FLOAT) {
        let v = Volume {
            resource: *resource,
            copy: copy.clone(),
            format: *format,
        };
        let Some((bytes, pitch)) = read(host, &v) else {
            continue;
        };
        let mut m1 = vec![false; (WIDTH as usize) * HEIGHT as usize * usize::from(DEPTH)];
        for z in 0..usize::from(DEPTH) {
            for y in 0..HEIGHT as usize {
                for x in 0..WIDTH as usize {
                    let t = texel(&bytes, pitch, 2, x, y, z);
                    let i = (z * HEIGHT as usize + y) * WIDTH as usize + x;
                    match t {
                        0xbc00 => {
                            minus1[0] += 1;
                            m1[i] = true;
                        }
                        0xc000 => minus1[1] += 1,
                        _ => minus1[2] += 1,
                    }
                }
            }
        }
        tracing::info!(
            frame,
            seconds = clock(),
            resource = format_args!("{resource:#x}"),
            minus_one = minus1[0],
            minus_two = minus1[1],
            other = minus1[2],
            "probe watch: the state volume"
        );
        state = Some(m1);
        break;
    }
    let Some(minus_one) = state else { return };
    for (resource, copy, format) in volumes.iter().filter(|v| v.2 != R16_FLOAT) {
        let v = Volume {
            resource: *resource,
            copy: copy.clone(),
            format: *format,
        };
        let Some((bytes, pitch)) = read(host, &v) else {
            continue;
        };
        // Zero voxels among those the state volume marks -1 and among the others; the
        // magnitudes (largest channel) of the -1 ones: zero, < 2^-12, < 2^-8, < 1, >= 1.
        let (mut zero_m1, mut zero_other, mut flag5) = (0u32, 0u32, 0u32);
        let mut bins = [0u32; 5];
        for z in 0..usize::from(DEPTH) {
            for y in 0..HEIGHT as usize {
                for x in 0..WIDTH as usize {
                    let t = texel(&bytes, pitch, 4, x, y, z);
                    let m1 = minus_one[(z * HEIGHT as usize + y) * WIDTH as usize + x];
                    if *format == R8G8B8A8_UNORM {
                        flag5 += u32::from(t >> 24 == 5);
                        continue;
                    }
                    if t == 0 {
                        if m1 {
                            zero_m1 += 1;
                            bins[0] += 1;
                        } else {
                            zero_other += 1;
                        }
                        continue;
                    }
                    if !m1 {
                        continue;
                    }
                    let big = small(t & 0x7ff, 6)
                        .max(small(t >> 11 & 0x7ff, 6))
                        .max(small(t >> 22, 5));
                    let bin = if big < 2f32.powi(-12) {
                        1
                    } else if big < 2f32.powi(-8) {
                        2
                    } else if big < 1.0 {
                        3
                    } else {
                        4
                    };
                    bins[bin] += 1;
                }
            }
        }
        if *format == R8G8B8A8_UNORM {
            tracing::info!(
                frame,
                seconds = clock(),
                resource = format_args!("{resource:#x}"),
                alpha_5 = flag5,
                "probe watch: an R8G8B8A8 volume"
            );
        } else {
            tracing::info!(
                frame,
                seconds = clock(),
                resource = format_args!("{resource:#x}"),
                zero_in_minus_one = zero_m1,
                zero_elsewhere = zero_other,
                magnitudes = ?bins,
                "probe watch: a light volume (zero, <2^-12, <2^-8, <1, >=1 among the -1 voxels)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::small;

    #[test]
    fn the_small_floats_read_as_numbers() {
        // 11-bit: 5 exponent bits (bias 15), 6 mantissa bits.
        assert_eq!(small(15 << 6, 6), 1.0);
        assert_eq!(small(16 << 6 | 32, 6), 3.0);
        assert_eq!(small(1, 6), 2f32.powi(-20));
        // 10-bit: 5 mantissa bits.
        assert_eq!(small(15 << 5, 5), 1.0);
        assert!(small(31 << 5, 5).is_infinite());
    }
}
