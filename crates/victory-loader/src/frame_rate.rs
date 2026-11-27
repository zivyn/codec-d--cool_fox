//! The title's frame rate (`VICTORY_FRAME_INTERVAL_MS`, off without it): the
//! rate the game asks for is a setting of its own, and two
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
//! - The simulation: +0x22264d0 hands each of its timers the real time
//!   since the last frame (`QueryPerformanceCounter`); +0x2226040 makes the
//!   step of it, at most 1/15 s and at least `1 / rate` (+0x6849844, made
//!   at +0x22435de and +0x2241190 from +0x2241fd0, which reads the same
//!   setting). So at 30 every frame shorter than 33.3 ms still moves game
//!   time 33.3 ms: a fixed 30 Hz step for fast frames, delta time for slow
//!   ones.
//!
//! So reporting a 120 Hz display alone changes nothing at 30 (divisor 4,
//! 33,333 us). Here the two loads of the setting (`mov eax, [reg+210h]`)
//! become `mov eax, rate ; nop`, so the title's frame time, interval and
//! own limiter follow the rate asked for, and +0x2241fd0 returns the rate
//! too (1000 for no limit), so the shortest step follows; the setting
//! itself is left as it is. Each site's bytes, from the load to the store of the frame time, are
//! compared before anything is written, by their length and BLAKE3 hash (no
//! bytes of the title are kept here); a site whose bytes differ is left alone
//! and said so (another build of the title).

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

/// `16.667` â†’ 16.667; 0 is allowed (no wait), more than a second is not.
pub fn parse_interval_ms(spec: &str) -> Option<f64> {
    spec.trim().parse::<f64>().ok().filter(|ms| ms.is_finite() && (0.0..=1000.0).contains(ms))
}

/// The frame rate the title is given for an interval: `1000 / ms` rounded
/// (16.667 â†’ 60, 8.333 â†’ 120, 33.333 â†’ 30), 0 for no wait (the title's own
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

/// A read of the setting to change: its offset in the image, what it is,
/// and the length and BLAKE3 hash (hex) of the bytes there, compared first;
/// the replacement goes at the start.
pub struct Site {
    pub at: usize,
    pub kind: Kind,
    pub len: usize,
    pub hash: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `mov eax, [reg+210h]` before the frame time is made of it: becomes
    /// `mov eax, rate ; nop` (6 bytes).
    Load,
    /// +0x2241fd0, the rate the timers' shortest step is made of (`1 /
    /// rate`, +0x6849844): `cmp dword [rax+210h], 0` becomes `mov eax, rate
    /// ; jmp` to its `vcvtsi2ss` (7 bytes). Left as it was, the timers step at
    /// least 1/30 s however short the frame: at 60 frames a second the game
    /// ran at twice its speed (run fps-1, menus). For no limit, 1000.
    SimRate,
}

/// The three reads (game_scarlett_master.exe 1.0.2.1, image offsets).
pub const SITES: [Site; 3] = [
    // +0x1e0fb30: mov eax, [rdi+210h] ... mov [+0x98f84ac], eax
    Site {
        at: 0x1e0fc62,
        kind: Kind::Load,
        len: 56,
        hash: "cd1788fdbef9991dd4f69d96f68c438bd9b739b149e532c4839db140f95ad6e7",
    },
    // +0x22434a0: mov eax, [rsi+210h] ... mov [+0x98f84ac], eax
    Site {
        at: 0x224359a,
        kind: Kind::Load,
        len: 58,
        hash: "dbf806417dd47b8d671825844b93516843e04d5c43cc3bc157cd473d9b905e50",
    },
    // +0x2241fd0: cmp dword [rax+210h], 0 ; je ; call ; mov eax, [rax+210h] ; vcvtsi2ss xmm0, xmm0, rax
    Site {
        at: 0x2241fdb,
        kind: Kind::SimRate,
        len: 25,
        hash: "8f23569c5501b2bf3c68aab953ce563647a47e6d2c872e4273b51a9d7f2365f1",
    },
];

/// Title data worth seeing while it runs (`VICTORY_TIMER_LOG=1`): the frame
/// time (+0x98f84ac) and the limiter's flag (+0x7453ec9).
pub const FRAME_US: usize = 0x98f84ac;
pub const LIMITER_ON: usize = 0x7453ec9;
/// The title's timers (each set up at +0x22261a9 with a step of 0.05 s and
/// 50 ms): +0 and +4 times, +0xc the step in seconds, +0x10 in milliseconds.
pub const TIMERS: [usize; 7] = [0xa943788, 0x856c6b4, 0xabbef40, 0xa958358, 0xa958380, 0x74fa558, 0xa943768];

/// What goes at a site of `kind` for `rate`.
pub fn replacement(kind: Kind, rate: u32) -> Vec<u8> {
    match kind {
        // mov eax, rate ; nop
        Kind::Load => {
            let r = rate.to_le_bytes();
            vec![0xb8, r[0], r[1], r[2], r[3], 0x90]
        }
        // mov eax, rate ; jmp +13 (to the vcvtsi2ss, 20 bytes on)
        Kind::SimRate => {
            let r = (if rate == 0 { 1000 } else { rate }).to_le_bytes();
            vec![0xb8, r[0], r[1], r[2], r[3], 0xeb, 0x0d]
        }
    }
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
    sites
        .iter()
        .map(|site| {
            let code = replacement(site.kind, rate);
            let len = code.len();
            let done = (|| {
                if site.len < len || site.at.checked_add(site.len).is_none_or(|end| end > size) {
                    return Patched::Failed("outside the image");
                }
                let at = base + site.at;
                // SAFETY: inside the mapped image (checked above).
                let there = unsafe { std::slice::from_raw_parts(at as *const u8, site.len) };
                if blake3::hash(there).to_hex().as_str() != site.hash {
                    return Patched::Differs;
                }
                let mut old = 0;
                // SAFETY: the load's bytes, made writable for this and put
                // back; nothing runs them yet.
                unsafe {
                    if VirtualProtect(at as *const _, len, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
                        return Patched::Failed("Windows would not make it writable");
                    }
                    std::ptr::copy_nonoverlapping(code.as_ptr(), at as *mut u8, len);
                    VirtualProtect(at as *const _, len, old, &mut old);
                    FlushInstructionCache(GetCurrentProcess(), at as *const _, len);
                }
                Patched::Done
            })();
            let offset = format_args!("{:#x}", site.at);
            match done {
                Patched::Done => tracing::info!(site = %offset, kind = ?site.kind, rate, "frame rate: the title's read of its frame rate setting patched (VICTORY_FRAME_INTERVAL_MS)"),
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
    fn the_replacements_load_the_rate_and_the_sites_are_hashes() {
        let code = replacement(Kind::SimRate, 60);
        assert_eq!(code.len() + usize::from(code[6]), 20, "the jump lands 20 bytes on (the title's vcvtsi2ss)");
        assert_eq!(replacement(Kind::Load, 60), [0xb8, 60, 0, 0, 0, 0x90]);
        assert_eq!(replacement(Kind::SimRate, 0)[1..5], 1000u32.to_le_bytes());
        for site in &SITES {
            assert!(site.hash.len() == 64 && site.hash.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(site.len >= replacement(site.kind, 60).len());
        }
    }

    #[cfg(windows)]
    #[test]
    fn sites_take_the_rate_and_others_are_left_alone() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree};
        let site = |at: usize, kind: Kind, bytes: &[u8]| Site {
            at,
            kind,
            len: bytes.len(),
            hash: Box::leak(blake3::hash(bytes).to_hex().to_string().into_boxed_str()),
        };
        // SAFETY: fresh memory of our own, written and run inside it.
        unsafe {
            let place = VirtualAlloc(std::ptr::null(), 0x10000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as *mut u8;
            assert!(!place.is_null());
            // Windows of bytes of our own (int3s): one as expected, one that differs.
            let bytes: Vec<u8> = (0..56).map(|n| if n % 7 == 0 { 0x90 } else { 0xcc }).collect();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), place.add(0x100), bytes.len());
            let mut other = bytes.clone();
            other[2] = 0x14;
            std::ptr::copy_nonoverlapping(other.as_ptr(), place.add(0x200), other.len());
            // The simulation's rate as a function: 20 bytes, then vcvtsi2ss
            // xmm0, xmm0, rax and ret; the replacement jumps over the 20.
            let mut sim = vec![0xcc_u8; 20];
            sim.extend([0xc4, 0xe1, 0xfa, 0x2a, 0xc0]);
            let mut f = sim.clone();
            f.push(0xc3);
            std::ptr::copy_nonoverlapping(f.as_ptr(), place.add(0x300), f.len());
            let mine = [
                site(0x100, Kind::Load, &bytes),
                Site { at: 0x200, ..site(0x200, Kind::Load, &bytes) },
                site(0x300, Kind::SimRate, &sim),
                site(0xfff0, Kind::Load, &bytes),
            ];
            let done = apply(place as usize, 0x10000, 60, &mine);
            assert_eq!(done[..3], [Patched::Done, Patched::Differs, Patched::Done]);
            assert!(matches!(done[3], Patched::Failed(_)));
            let after = std::slice::from_raw_parts(place.add(0x100), bytes.len());
            assert_eq!(after[..6], replacement(Kind::Load, 60)[..]);
            assert_eq!(after[6..], bytes[6..]);
            assert_eq!(std::slice::from_raw_parts(place.add(0x200), other.len()), &other[..]);
            let rate: extern "system" fn() -> f32 = std::mem::transmute(place.add(0x300));
            assert_eq!(rate(), 60.0);
            VirtualFree(place.cast(), 0, MEM_RELEASE);
        }
    }
}

