//! Instructions the console's CPU (Zen 2) has and an Intel PC's lacks, emulated: the title's
//! code raises STATUS_ILLEGAL_INSTRUCTION on them (the title zeroes memory with a `clzero` loop at
//! its first frames), and the vectored handler steps over them here.
//!
//! Ported from XCL (github.com/ivall/xcl, GPL-2.0-only): `insn.rs` (decoder, as
//! `console_insn_decode.rs`, unchanged) and `eh.rs` `emulate_console_only`.

#[path = "console_insn_decode.rs"]
mod decode;

use decode::ConsoleOnly;
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use windows_sys::Win32::System::Diagnostics::Debug::CONTEXT;

const CONTINUE_EXECUTION: i32 = -1;

/// Whether what Windows said about memory is remembered per thread (the regions it found
/// writable, the code bytes it found readable) and not asked again at every emulated
/// instruction. A query walks the pages of the region it answers for: asked from outside, it
/// took 2-9 ms in the title's heaps of 0.6-2.2 GB and 89 ms in its 6.2 GB page pool (run 30),
/// and the title zeroes blocks of them with `clzero` loops all through a frame: six of its
/// threads spent 25-78 % of their time inside that query (runs 29 and 30), about 27 s of
/// processor time in Windows per 9 s frame. On by default; `VICTORY_CLZERO_ASK=1` asks every
/// time, as before.
fn remembers() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CLZERO_ASK").is_none_or(|v| v != "1"))
}

/// Answers a remembered region gives before Windows is asked about it again: one freed or
/// protected meanwhile is forgotten then.
const ANSWERS: u32 = 4096;

thread_local! {
    /// The regions Windows last said are committed and writable (start, end), newest first,
    /// and the answers they gave since they were emptied.
    static WRITABLE: Cell<([(usize, usize); 4], u32)> = const { Cell::new(([(0, 0); 4], 0)) };
    /// Faulting addresses and the code bytes found readable there, newest first.
    static CODE: Cell<[(usize, usize); 8]> = const { Cell::new([(0, 0); 8]) };
}

/// Instructions emulated, the times Windows was asked about memory for them, and the bytes
/// `clzero` loops zeroed, since the start.
static EMULATED: AtomicU64 = AtomicU64::new(0);
static ASKED: AtomicU64 = AtomicU64::new(0);
static ZEROED: AtomicU64 = AtomicU64::new(0);

/// Whether `monitorx` and `mwaitx` are replaced in the title's code the first time they fault
/// (`monitorx` by a 3-byte no-op, `mwaitx` by `pause` and a no-op), so that they never fault
/// again. Emulated, each pass of such a wait loop is two trips into Windows and back, through
/// every vectored handler, and no lighter a wait for it: the emulation does no more than
/// `pause` either, and the loop looks at its value again after it. On by default;
/// `VICTORY_PATCH_WAITS=0` emulates each time, as before.
fn patches_waits() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PATCH_WAITS").is_none_or(|v| v != "0"))
}

/// What replaces the 3 bytes of a wait instruction.
fn wait_patch(what: ConsoleOnly) -> Option<[u8; 3]> {
    match what {
        ConsoleOnly::Monitorx => Some([0x0f, 0x1f, 0x00]),
        ConsoleOnly::Mwaitx => Some([0xf3, 0x90, 0x90]),
        _ => None,
    }
}

/// The aligned 8 bytes `word` around address `at` with the 3 bytes at `at` replaced; `None`
/// where they do not all lie in those 8.
fn with_patch(word: u64, at: usize, patch: [u8; 3]) -> Option<u64> {
    let offset = at & 7;
    if offset + patch.len() > 8 {
        return None;
    }
    let mut bytes = word.to_le_bytes();
    bytes[offset..offset + patch.len()].copy_from_slice(&patch);
    Some(u64::from_le_bytes(bytes))
}

/// One thread at a time changes code: a second one would take the first one's loosened
/// protection for the page's own.
static PATCHING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Replaces the 3 bytes at `at`, which are `was`, by `patch`, in one store that other threads
/// see whole: whether they are replaced now (not where they cross 8 aligned bytes, or the page
/// cannot be written).
///
/// # Safety
/// `at` is in code that stays mapped, and `patch` does for it what `was` does.
unsafe fn patch(at: usize, was: [u8; 3], patch: [u8; 3]) -> bool {
    use std::sync::atomic::Ordering::SeqCst;
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    if (at & 7) + 3 > 8 {
        return false;
    }
    let _one = PATCHING.lock().unwrap_or_else(|e| e.into_inner());
    let word = (at & !7) as *mut u64;
    let here = (at & 7)..(at & 7) + 3;
    let mut old = 0u32;
    // SAFETY: the aligned 8 bytes around the instruction lie in one page of mapped code, made
    // writable for the store and put back.
    unsafe {
        if VirtualProtect(word.cast(), 8, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return false;
        }
        let done = AtomicU64::from_ptr(word)
            .fetch_update(SeqCst, SeqCst, |now| if now.to_le_bytes()[here.clone()] == was { with_patch(now, at, patch) } else { None })
            // (Another thread's fault at the same place: done already.)
            .map_or_else(|now| now.to_le_bytes()[here.clone()] == patch, |_| true);
        VirtualProtect(word.cast(), 8, old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), word.cast(), 8);
        done
    }
}

/// Diagnostics: how often each place in the title's code raises one of these exceptions, the
/// busiest logged every five seconds.
#[cfg(feature = "diagnostics")]
mod sites {
    use super::{AtomicU64, Relaxed};
    use std::sync::atomic::AtomicUsize;

    /// Address, what is there (an index into [`NAMES`]), exceptions since the last line.
    static SITES: [(AtomicUsize, AtomicUsize, AtomicU64); 512] =
        [const { (AtomicUsize::new(0), AtomicUsize::new(0), AtomicU64::new(0)) }; 512];
    const NAMES: [&str; 8] = ["other", "clwb", "monitorx", "mwaitx", "clzero", "clzero loop", "rdpru", "extrq/insertq"];
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    static PATCHED: AtomicU64 = AtomicU64::new(0);

    pub(super) fn patched() {
        PATCHED.fetch_add(1, Relaxed);
    }

    pub(super) fn count(at: usize, what: super::ConsoleOnly) {
        use super::ConsoleOnly as C;
        let kind = match what {
            C::Clwb => 1,
            C::Monitorx => 2,
            C::Mwaitx => 3,
            C::Clzero => 4,
            C::ClzeroLoop { .. } => 5,
            C::Rdpru => 6,
            C::Extrq { .. } | C::Insertq { .. } => 7,
            _ => 0,
        };
        let mut i = (at ^ at >> 9) % SITES.len();
        for _ in 0..8 {
            let s = &SITES[i];
            let there = s.0.compare_exchange(0, at, Relaxed, Relaxed).unwrap_or_else(|there| there);
            if there == 0 || there == at {
                s.1.store(kind, Relaxed);
                s.2.fetch_add(1, Relaxed);
                break;
            }
            i = (i + 1) % SITES.len();
        }
        // Now and then: is a line due? Never waited for.
        if let Ok(mut last) = LAST.try_lock() {
            let since = *last.get_or_insert_with(std::time::Instant::now);
            if since.elapsed().as_secs() >= 5 {
                let s = since.elapsed().as_secs_f64();
                *last = Some(std::time::Instant::now());
                drop(last);
                line(s);
            }
        }
    }

    fn line(seconds: f64) {
        let mut all: Vec<(u64, usize, usize)> =
            SITES.iter().map(|s| (s.2.swap(0, Relaxed), s.0.load(Relaxed), s.1.load(Relaxed))).filter(|s| s.0 > 0).collect();
        all.sort_unstable_by_key(|s| std::cmp::Reverse(s.0));
        let mut kinds = [0u64; NAMES.len()];
        for s in &all {
            kinds[s.2] += s.0;
        }
        let kinds: Vec<String> = NAMES
            .iter()
            .zip(kinds)
            .filter(|(_, n)| *n > 0)
            .map(|(name, n)| format!("{name} {:.0}", n as f64 / seconds))
            .collect();
        let top: Vec<String> =
            all.iter().take(8).map(|s| format!("{:#x} {} {:.0}", s.1, NAMES[s.2], s.0 as f64 / seconds)).collect();
        tracing::info!(
            "console CPU instructions that fault, a second: {}; wait instructions replaced in place so far: {}; the busiest places: {}",
            kinds.join(", "),
            PATCHED.load(Relaxed),
            top.join(", ")
        );
    }
}

fn readable(va: usize, bytes: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let found = unsafe { VirtualQuery(va as *const _, &mut m, size_of_val(&m)) } != 0;
    ASKED.fetch_add(1, Relaxed);
    found && m.State == MEM_COMMIT && m.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0 && m.BaseAddress as usize + m.RegionSize >= va + bytes
}

fn writable(va: usize, bytes: usize) -> bool {
    writable_to(va, va + bytes) == Some(va + bytes)
}

/// Where the committed, writable memory that `va` lies in ends, at most `end`; None if `va`
/// is not in such memory. A region found is remembered ([`remembers`]).
/// shortcut: a remembered region that was freed or protected since is written all the same
/// for up to [`ANSWERS`] answers: the write then faults inside the handler, which the guard
/// on view memory opens as it does any write, and which is a crash otherwise (as the
/// unchecked single-line emulation behind this one already is).
fn writable_to(va: usize, end: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE, PAGE_READWRITE, VirtualQuery};
    let remember = remembers();
    if remember {
        let (regions, answers) = WRITABLE.with(Cell::get);
        if let Some(&(_, to)) = regions.iter().find(|&&(from, to)| from <= va && va < to) {
            let next = if answers >= ANSWERS { ([(0, 0); 4], 0) } else { (regions, answers + 1) };
            WRITABLE.with(|w| w.set(next));
            return Some(to.min(end));
        }
    }
    // SAFETY: plain data filled in by the query.
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let found = unsafe { VirtualQuery(va as *const _, &mut m, size_of_val(&m)) } != 0;
    ASKED.fetch_add(1, Relaxed);
    if !found || m.State != MEM_COMMIT || m.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) == 0 {
        return None;
    }
    let (from, to) = (m.BaseAddress as usize, m.BaseAddress as usize + m.RegionSize);
    if remember {
        WRITABLE.with(|w| {
            let (mut regions, answers) = w.get();
            regions.rotate_right(1);
            regions[0] = (from, to);
            w.set((regions, answers));
        });
    }
    Some(to.min(end))
}

/// An illegal instruction at `ctx.Rip`: a console-only one is emulated and stepped over
/// (`CONTINUE_EXECUTION`); anything else returns 0 (the crash report and the title's handlers
/// see it).
///
/// # Safety
/// `ctx` is the faulting thread's context, from the exception being dispatched.
pub(crate) unsafe fn emulate(ctx: &mut CONTEXT) -> i32 {
    let rip = ctx.Rip as usize;
    // The same few instructions fault again and again: their bytes are asked about once.
    let known = if remembers() { CODE.with(Cell::get) } else { [(0, 0); 8] };
    let n = match known.iter().find(|&&(at, n)| at == rip && n > 0) {
        Some(&(_, n)) => n,
        None => {
            if !readable(rip, 1) {
                return 0;
            }
            let n = (1..=16).rev().find(|&n| readable(rip, n)).unwrap_or(0);
            if remembers() {
                let mut known = known;
                known.rotate_right(1);
                known[0] = (rip, n);
                CODE.with(|c| c.set(known));
            }
            n
        }
    };
    // SAFETY: `n` bytes at `rip` are readable.
    let code = unsafe { std::slice::from_raw_parts(rip as *const u8, n) };
    let Some((what, len)) = decode::decode(code) else { return 0 };
    static SEEN: std::sync::Mutex<Vec<std::mem::Discriminant<ConsoleOnly>>> = std::sync::Mutex::new(Vec::new());
    if let Ok(mut seen) = SEEN.lock()
        && !seen.contains(&std::mem::discriminant(&what))
    {
        seen.push(std::mem::discriminant(&what));
        if what != ConsoleOnly::Ud2 {
            tracing::warn!(instruction = ?what, at = format_args!("{rip:#x}"), "a console CPU instruction this CPU lacks: emulated (logged once)");
        }
    }
    #[cfg(feature = "diagnostics")]
    if what != ConsoleOnly::Ud2 {
        sites::count(rip, what);
    }
    match what {
        ConsoleOnly::Ud2 => return 0,
        ConsoleOnly::Clwb => {
            // nothing to write back on the PC: patched to NOPs, no exception next time
            use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
            let mut old = 0u32;
            let _one = PATCHING.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: the instruction's own bytes, made writable for the patch and restored.
            unsafe {
                if VirtualProtect(rip as *const _, len, PAGE_EXECUTE_READWRITE, &mut old) != 0 {
                    std::slice::from_raw_parts_mut(rip as *mut u8, len).fill(0x90);
                    VirtualProtect(rip as *const _, len, old, &mut old);
                }
            }
        }
        ConsoleOnly::Monitorx | ConsoleOnly::Mwaitx => {
            // Without a prefix, so 3 bytes: replaced, the next pass does not fault.
            if len == 3
                && patches_waits()
                && let Some(new) = wait_patch(what)
                // SAFETY: the title's code, where the instruction just faulted; both
                // replacements are waits that wait for nothing, as the emulation is.
                && unsafe { patch(rip, [code[0], code[1], code[2]], new) }
            {
                #[cfg(feature = "diagnostics")]
                sites::patched();
            }
            std::hint::spin_loop();
        }
        ConsoleOnly::Clzero => {
            // with an address-size prefix (0x67) the line is at EAX, zero-extended
            let a32 = code[..len - 3].contains(&0x67);
            let line = (if a32 { ctx.Rax & 0xFFFF_FFFF } else { ctx.Rax } as usize) & !63;
            if !writable(line, 64) {
                return 0; // the real instruction faults there too
            }
            // SAFETY: the 64-byte line is committed and writable.
            unsafe { std::ptr::write_bytes(line as *mut u8, 0, 64) };
        }
        ConsoleOnly::ClzeroLoop { end_reg } => {
            // the whole `clzero; add rax,40h; cmp rax,end; jb` loop at once
            let regs: [u64; 16] = [
                ctx.Rax, ctx.Rcx, ctx.Rdx, ctx.Rbx, ctx.Rsp, ctx.Rbp, ctx.Rsi, ctx.Rdi, ctx.R8, ctx.R9, ctx.R10, ctx.R11, ctx.R12, ctx.R13, ctx.R14, ctx.R15,
            ];
            let end = regs[end_reg as usize & 15] as usize;
            let start = ctx.Rax as usize;
            // A pass zeroes the whole line `rax` is in, wherever in it, and the test comes
            // after it: lines are zeroed past an `end` inside one, and one is for an `end`
            // not above the start. (The bytes from `start` to `end` were, `rax` came out as
            // `end`, and the flags stayed what they were.)
            const MOST: usize = (2 << 30) / 64;
            let wanted = if end > start { (end - start).div_ceil(64) } else { 1 };
            let passes = wanted.min(MOST);
            let first = start & !63;
            let Some(stop) = first.checked_add(passes * 64) else { return 0 };
            // Region by region: a block may lie across several (the title's pages are
            // mapped one view each), and one answer for all of it was a refusal, after
            // which every 64-byte line took an exception of its own.
            let mut at = first;
            while at < stop {
                let Some(to) = writable_to(at, stop) else { break };
                // SAFETY: committed and writable from `at` to `to`.
                unsafe { std::ptr::write_bytes(at as *mut u8, 0, to - at) };
                at = to;
            }
            let done = (at - first) / 64;
            if done == 0 {
                return 0;
            }
            ZEROED.fetch_add((done * 64) as u64, Relaxed);
            ctx.Rax = ctx.Rax.wrapping_add((done * 64) as u64);
            if done < wanted {
                // What follows is not writable (or the block is huge): the loop goes on
                // there (the same instruction again), where the real one faults too.
                return CONTINUE_EXECUTION;
            }
            ctx.EFlags = ctx.EFlags & !crate::crash::CMP_FLAGS | crate::crash::cmp_flags(ctx.Rax, end as u64);
        }
        ConsoleOnly::Rdpru => {
            let mut v = 0i64;
            // SAFETY: a valid out pointer.
            unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut v) };
            let v = if ctx.Rcx as u32 <= 1 { v as u64 } else { 0 };
            ctx.Rax = v & 0xFFFF_FFFF;
            ctx.Rdx = v >> 32;
        }
        ConsoleOnly::Rdpid(r) => {
            // SAFETY: no preconditions.
            let cpu = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessorNumber() } as u64;
            let regs: [&mut u64; 16] = [
                &mut ctx.Rax, &mut ctx.Rcx, &mut ctx.Rdx, &mut ctx.Rbx, &mut ctx.Rsp, &mut ctx.Rbp, &mut ctx.Rsi, &mut ctx.Rdi, &mut ctx.R8, &mut ctx.R9, &mut ctx.R10, &mut ctx.R11, &mut ctx.R12, &mut ctx.R13,
                &mut ctx.R14, &mut ctx.R15,
            ];
            *regs[r as usize & 15] = cpu;
        }
        // the low quadword changes; the high one is zeroed, as a Zen 5 does
        ConsoleOnly::Extrq { dst, src, len, idx } => {
            // SAFETY: the context's XMM registers (CONTEXT_FLOATING_POINT is captured for exceptions).
            let x = unsafe { &mut ctx.Anonymous.FltSave.XmmRegisters };
            let (len, idx) = match src {
                Some(s) => {
                    let c = x[s as usize & 15].Low;
                    (c as u8, (c >> 8) as u8)
                }
                None => (len, idx),
            };
            let d = &mut x[dst as usize & 15];
            d.Low = decode::extrq(d.Low, len, idx);
            d.High = 0;
        }
        ConsoleOnly::Insertq { dst, src, reg, len, idx } => {
            // SAFETY: as above.
            let x = unsafe { &mut ctx.Anonymous.FltSave.XmmRegisters };
            let s = x[src as usize & 15];
            let (len, idx) = if reg { (s.High as u8, (s.High >> 8) as u8) } else { (len, idx) };
            let d = &mut x[dst as usize & 15];
            d.Low = decode::insertq(d.Low, s.Low, len, idx);
            d.High = 0;
        }
    }
    ctx.Rip += len as u64;
    // How often this runs at all (each one is an exception: a trip into Windows and back).
    let emulated = EMULATED.fetch_add(1, Relaxed) + 1;
    if emulated >= 1 << 16 && emulated.is_power_of_two() {
        tracing::info!(
            emulated,
            asked = ASKED.load(Relaxed),
            zeroed_mb = ZEROED.load(Relaxed) >> 20,
            "console CPU instructions emulated so far, the times Windows was asked about memory for them, and what clzero loops zeroed"
        );
    }
    CONTINUE_EXECUTION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_instructions_are_replaced_inside_eight_aligned_bytes() {
        let word = u64::from_le_bytes([1, 2, 0x0f, 0x01, 0xfb, 6, 7, 8]);
        let new = with_patch(word, 0x1002, [0xf3, 0x90, 0x90]).unwrap();
        assert_eq!(new.to_le_bytes(), [1, 2, 0xf3, 0x90, 0x90, 6, 7, 8]);
        assert!(with_patch(word, 0x1005, [0; 3]).is_some());
        assert!(with_patch(word, 0x1006, [0; 3]).is_none(), "crosses into the next eight");
    }

    /// `monitorx; mwaitx; ret` in a page of its own: each is replaced at its first fault,
    /// and the page is protected as before.
    #[test]
    fn a_wait_faults_once_and_the_page_keeps_its_protection() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualFree,
            VirtualProtect, VirtualQuery,
        };
        // SAFETY: memory of our own, read, written and run within its page.
        unsafe {
            let page = VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE).cast::<u8>();
            assert!(!page.is_null());
            let code = [0x0f, 0x01, 0xfa, 0x0f, 0x01, 0xfb, 0xc3];
            // Both within the aligned eight bytes at 8; and one over the end of its eight
            // (at 22), which is left alone.
            std::ptr::copy_nonoverlapping(code.as_ptr(), page.add(8), code.len());
            std::ptr::copy_nonoverlapping(code.as_ptr(), page.add(22), code.len());
            let mut old = 0;
            assert_ne!(VirtualProtect(page.cast(), 4096, PAGE_EXECUTE_READ, &mut old), 0);
            let at = page as usize;
            let mut context: CONTEXT = std::mem::zeroed();
            for rip in [at + 8, at + 11, at + 22] {
                context.Rip = rip as u64;
                assert_eq!(emulate(&mut context), CONTINUE_EXECUTION);
                assert_eq!(context.Rip, rip as u64 + 3);
            }
            if patches_waits() {
                let now = std::slice::from_raw_parts(page.add(8), 7);
                assert_eq!(now, [0x0f, 0x1f, 0x00, 0xf3, 0x90, 0x90, 0xc3]);
                // They run now.
                let run: extern "system" fn() = std::mem::transmute(page.add(8));
                run();
                // A second thread that faulted at the same place meanwhile.
                assert!(patch(at + 8, [0x0f, 0x01, 0xfa], [0x0f, 0x1f, 0x00]));
                assert!(!patch(at + 22, [0x0f, 0x01, 0xfa], [0x0f, 0x1f, 0x00]), "crosses its eight bytes");
            }
            assert_eq!(std::slice::from_raw_parts(page.add(22), 3), [0x0f, 0x01, 0xfa]);
            let mut m: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
            VirtualQuery(page.cast(), &mut m, size_of_val(&m));
            assert_eq!(m.Protect, PAGE_EXECUTE_READ);
            VirtualFree(page.cast(), 0, MEM_RELEASE);
        }
    }
}
