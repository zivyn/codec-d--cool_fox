//! The title's frame rate (`VICTORY_FRAME_INTERVAL_MS`, off without it): the
//! rate Grand Theft Auto VI asks for is a setting of its own, and two
//! places in its code are changed at load to read another.
//!
//! What the title does (game_scarlett_master.exe 1.0.2.1, image offsets):
//!
//! - Its graphics settings hold a frame rate (+0x210 of the settings object,
//!   30 on Series X). Two functions that apply the settings, +0x1e0fb30 and
//!   +0x22434a0, turn it into a frame time: `frame_us = 1e6 /
//!   clamp(rate)`, 0 and 1000 or more being 1000 (the code's "no limit"),
//!   stored at +0x98f84ac.
//! - +0x247ab10 sets the frame interval from it: the display's rate is 60,
//!   or 120 if its mode list (the output's `GetDisplayModeList`) has a
//!   120 Hz mode (+0x2481ca0); the divisor is `min(display / rate, 4)`, or
//!   1 for a rate above the display's; the interval is a table at
//!   +0x61ece60 (`[16667, 16667, 33333, 50000, 66667]` at 60 Hz, `[8333,
//!   8333, 16667, 25000, 33333]` at 120 Hz) and goes to
//!   `SetFrameIntervalX` (device +0x270), with `ScheduleFrameEventX`
//!   (+0x278) after it. At 30 and 60 Hz: 33,333 us, the flat 33.3 ms of the
//!   menus and videos.
//! - +0x120530, once a flag (+0x7453ec9, set by +0x21b77c0) is up, is a
//!   limiter of its own on `QueryPerformanceCounter`: it waits until
//!   `1000 / trunc(1e6 / frame_us)` ms have passed since the last frame.
//! - +0x3eaf5d0 hands `1e6 / frame_us` to scripts, and +0x234490 scales
//!   per-frame amounts by it (1.0 below 60 except 0.7 at 40, 0.5 from 60).
//!
//! So reporting a 120 Hz display alone changes nothing at 30 (divisor 4,
//! 33,333 us). Here the two loads of the setting (`mov eax, [reg+210h]`)
//! become `mov eax, rate ; nop`, so the title's frame time, interval and
//! own limiter follow the rate asked for; the setting itself is left as it
//! is. Each site's bytes, from the load to the store of the frame time, are
//! compared before anything is written; a site whose bytes differ is left
//! alone and said so (another build of the title).

/// The switch: the interval in milliseconds our frame clock ticks at
/// (`victory-gpu`'s `frame`), 0 for no wait at all. `None` if unset or
/// malformed (said why).
pub fn asked_interval_ms() -> Option<f64> {
    let spec = std::env::var("VICTORY_FRAME_INTERVAL_MS").ok()?;
    let ms = parse_interval_ms(&spec);
    if ms.is_none() {
        eprintln!("VIctory: VICTORY_FRAME_INTERVAL_MS={spec}: expected milliseconds from 0 (no wait) to 1000");
    }
    ms
}

/// `16.667` → 16.667; 0 is allowed (no wait), more than a second is not.
pub fn parse_interval_ms(spec: &str) -> Option<f64> {
    spec.trim().parse::<f64>().ok().filter(|ms| ms.is_finite() && (0.0..=1000.0).contains(ms))
}

/// The frame rate the title is given for an interval: `1000 / ms` rounded
/// (16.667 → 60, 8.333 → 120, 33.333 → 30), 0 for no wait (the title's own
/// "no limit"). `VICTORY_TITLE_FPS=<rate>` gives it outright, `off` leaves the
/// title as it is (only our clock changes).
pub fn title_rate(interval_ms: f64, title_fps: Option<&str>) -> Option<u32> {
    match title_fps.map(str::trim) {
        Some("off") => None,
        Some(rate) => rate.parse::<u32>().ok().filter(|r| *r <= 1000),
        None if interval_ms <= 0.0 => Some(0),
        None => Some(((1000.0 / interval_ms).round() as u32).clamp(1, 1000)),
    }
}

/// The frame time the title derives from a rate, in microseconds, as its
/// code computes it (single-precision division, truncated).
pub fn title_frame_us(rate: u32) -> u32 {
    let clamped = if rate == 0 || rate >= 1000 { 1000 } else { rate };
    (1_000_000.0_f32 / clamped as f32) as u32
}

/// The interval the title then asks for (`SetFrameIntervalX`), in
/// microseconds, on a display of 60 Hz (or 120 Hz).
pub fn title_interval_us(rate: u32, display_120: bool) -> u32 {
    const TABLE: [[u32; 5]; 2] = [[16667, 16667, 33333, 50000, 66667], [8333, 8333, 16667, 25000, 33333]];
    let fps = (1_000_000.0_f32 / title_frame_us(rate) as f32) as u32 & 0xffff;
    let display = if display_120 { 120 } else { 60 };
    let index = if fps <= display { (display / fps.max(1)).min(4) } else { 1 };
    TABLE[usize::from(display_120)][index as usize]
}

/// A load of the setting to change: its offset in the image and the bytes
/// from it to the store of the frame time (the first six are the load).
pub struct Site {
    pub at: usize,
    pub bytes: &'static [u8],
}

/// The length of the load (`8B 8x 10 02 00 00`).
const LOAD_LEN: usize = 6;

/// The two loads (game_scarlett_master.exe 1.0.2.1, image offsets).
pub const SITES: [Site; 2] = [
    // +0x1e0fb30: mov eax, [rdi+210h] ... mov [+0x98f84ac], eax
    Site {
        at: 0x1e0fc62,
        bytes: &[
            0x8b, 0x87, 0x10, 0x02, 0x00, 0x00, 0xba, 0xe8, 0x03, 0x00, 0x00, 0xc5, 0xfa, 0x10, 0x0d, 0x9f, 0xa1, 0x34, 0x04, 0xb9, 0xe8,
            0x03, 0x00, 0x00, 0x3d, 0xe8, 0x03, 0x00, 0x00, 0x0f, 0x42, 0xd0, 0x85, 0xc0, 0x0f, 0x44, 0xd1, 0xc5, 0xea, 0x2a, 0xc2, 0xc5,
            0xf2, 0x5e, 0xc0, 0xc4, 0xe1, 0xfa, 0x2c, 0xc0, 0x89, 0x05, 0x12, 0x88, 0xae, 0x07,
        ],
    },
    // +0x22434a0: mov eax, [rsi+210h] ... mov [+0x98f84ac], eax
    Site {
        at: 0x224359a,
        bytes: &[
            0x8b, 0x86, 0x10, 0x02, 0x00, 0x00, 0xba, 0xe8, 0x03, 0x00, 0x00, 0xc5, 0xfa, 0x10, 0x0d, 0x67, 0x68, 0xf1, 0x03, 0xb9, 0xe8,
            0x03, 0x00, 0x00, 0x3d, 0xe8, 0x03, 0x00, 0x00, 0x0f, 0x42, 0xd0, 0x85, 0xc0, 0x0f, 0x44, 0xd1, 0xb1, 0xff, 0xc5, 0xea, 0x2a,
            0xc2, 0xc5, 0xf2, 0x5e, 0xc0, 0xc4, 0xe1, 0xfa, 0x2c, 0xc0, 0x89, 0x05, 0xd8, 0x4e, 0x6b, 0x07,
        ],
    },
];

/// Title data worth seeing while it runs (`VICTORY_TIMER_LOG=1`): the frame
/// time (+0x98f84ac) and the limiter's flag (+0x7453ec9).
pub const FRAME_US: usize = 0x98f84ac;
pub const LIMITER_ON: usize = 0x7453ec9;
/// The title's timers (each set up at +0x22261a9 with a step of 0.05 s and
/// 50 ms): +0 and +4 times, +0xc the step in seconds, +0x10 in milliseconds.
pub const TIMERS: [usize; 7] = [0xa943788, 0x856c6b4, 0xabbef40, 0xa958358, 0xa958380, 0x74fa558, 0xa943768];

/// `mov eax, rate ; nop`, as long as the load.
pub fn replacement(rate: u32) -> [u8; LOAD_LEN] {
    let r = rate.to_le_bytes();
    [0xb8, r[0], r[1], r[2], r[3], 0x90]
}

/// What became of a site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Patched {
    Done,
    /// Its bytes are not the ones expected: left alone.
    Differs,
    Failed(&'static str),
}

/// Makes each site of `sites` in the image of `size` bytes at `base` load
/// `rate`. What became of each, said in the log too.
///
/// # Safety
/// The image stays mapped and no thread runs its code yet.
#[cfg(windows)]
pub unsafe fn apply(base: usize, size: usize, rate: u32, sites: &[Site]) -> Vec<Patched> {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let code = replacement(rate);
    sites
        .iter()
        .map(|site| {
            let done = (|| {
                if site.bytes.len() < LOAD_LEN || site.at.checked_add(site.bytes.len()).is_none_or(|end| end > size) {
                    return Patched::Failed("outside the image");
                }
                let at = base + site.at;
                // SAFETY: inside the mapped image (checked above).
                let there = unsafe { std::slice::from_raw_parts(at as *const u8, site.bytes.len()) };
                if there != site.bytes {
                    return Patched::Differs;
                }
                let mut old = 0;
                // SAFETY: the load's bytes, made writable for this and put
                // back; nothing runs them yet.
                unsafe {
                    if VirtualProtect(at as *const _, LOAD_LEN, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
                        return Patched::Failed("Windows would not make it writable");
                    }
                    std::ptr::copy_nonoverlapping(code.as_ptr(), at as *mut u8, LOAD_LEN);
                    VirtualProtect(at as *const _, LOAD_LEN, old, &mut old);
                    FlushInstructionCache(GetCurrentProcess(), at as *const _, LOAD_LEN);
                }
                Patched::Done
            })();
            let offset = format_args!("{:#x}", site.at);
            match done {
                Patched::Done => tracing::info!(site = %offset, rate, "frame rate: the title's load of its frame rate setting patched (VICTORY_FRAME_INTERVAL_MS)"),
                Patched::Differs => tracing::warn!(site = %offset, "frame rate: not patched, the title's bytes there are not the ones expected (another build?)"),
                Patched::Failed(why) => tracing::warn!(site = %offset, why, "frame rate: not patched"),
            }
            done
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_and_rates() {
        assert_eq!(parse_interval_ms("16.667"), Some(16.667));
        assert_eq!(parse_interval_ms(" 0 "), Some(0.0));
        assert_eq!(parse_interval_ms("-1"), None);
        assert_eq!(parse_interval_ms("fast"), None);
        assert_eq!(parse_interval_ms("1001"), None);
        assert_eq!(title_rate(16.667, None), Some(60));
        assert_eq!(title_rate(8.333, None), Some(120));
        assert_eq!(title_rate(33.333, None), Some(30));
        assert_eq!(title_rate(0.0, None), Some(0));
        assert_eq!(title_rate(0.5, None), Some(1000));
        assert_eq!(title_rate(16.667, Some("off")), None);
        assert_eq!(title_rate(0.0, Some("90")), Some(90));
        assert_eq!(title_rate(0.0, Some("lots")), None);
    }

    /// The title's own arithmetic: its 30 is the 33,333 us the log shows,
    /// on a 60 Hz display and on a 120 Hz one.
    #[test]
    fn the_titles_intervals() {
        assert_eq!(title_frame_us(30), 33333);
        assert_eq!(title_frame_us(60), 16666);
        assert_eq!(title_frame_us(0), 1000);
        assert_eq!(title_frame_us(5000), 1000);
        assert_eq!(title_interval_us(30, false), 33333);
        assert_eq!(title_interval_us(30, true), 33333);
        assert_eq!(title_interval_us(40, true), 25000);
        assert_eq!(title_interval_us(60, false), 16667);
        assert_eq!(title_interval_us(60, true), 16667);
        assert_eq!(title_interval_us(120, true), 8333);
        assert_eq!(title_interval_us(120, false), 16667);
        assert_eq!(title_interval_us(0, false), 16667);
        assert_eq!(title_interval_us(0, true), 8333);
        assert_eq!(title_interval_us(20, false), 50000);
    }

    #[test]
    fn the_sites_are_loads_before_the_clamp() {
        for site in &SITES {
            // mov eax, [rdi/rsi+210h]
            assert_eq!(site.bytes[0], 0x8b);
            assert!(matches!(site.bytes[1], 0x86 | 0x87));
            assert_eq!(&site.bytes[2..6], &[0x10, 0x02, 0x00, 0x00]);
            // mov edx, 1000 follows; the window ends with the store of eax.
            assert_eq!(&site.bytes[6..11], &[0xba, 0xe8, 0x03, 0x00, 0x00]);
            assert_eq!(&site.bytes[site.bytes.len() - 6..site.bytes.len() - 4], &[0x89, 0x05]);
            // RIP-relative: the store is of +0x98f84ac.
            let disp = i32::from_le_bytes(site.bytes[site.bytes.len() - 4..].try_into().unwrap());
            let end = site.at + site.bytes.len();
            assert_eq!(end as i64 + i64::from(disp), FRAME_US as i64);
        }
        assert_eq!(replacement(60), [0xb8, 60, 0, 0, 0, 0x90]);
    }

    #[cfg(windows)]
    #[test]
    fn a_site_loads_the_rate_and_others_are_left_alone() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc};
        // SAFETY: fresh memory of our own.
        unsafe {
            let place = VirtualAlloc(std::ptr::null(), 0x10000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as *mut u8;
            assert!(!place.is_null());
            let bytes = SITES[0].bytes;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), place.add(0x100), bytes.len());
            let mut other = SITES[1].bytes.to_vec();
            other[2] = 0x14; // [rsi+214h]
            std::ptr::copy_nonoverlapping(other.as_ptr(), place.add(0x200), other.len());
            let mine = [Site { at: 0x100, bytes }, Site { at: 0x200, bytes: SITES[1].bytes }, Site { at: 0xfff0, bytes }];
            let done = apply(place as usize, 0x10000, 60, &mine);
            assert_eq!(done[..2], [Patched::Done, Patched::Differs]);
            assert!(matches!(done[2], Patched::Failed(_)));
            let after = std::slice::from_raw_parts(place.add(0x100), bytes.len());
            assert_eq!(after[..6], replacement(60));
            assert_eq!(after[6..], bytes[6..]);
            assert_eq!(std::slice::from_raw_parts(place.add(0x200), other.len()), &other[..]);
            windows_sys::Win32::System::Memory::VirtualFree(place.cast(), 0, windows_sys::Win32::System::Memory::MEM_RELEASE);
        }
    }
}
