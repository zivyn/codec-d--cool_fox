//! Copies between our memory and the title's that fail instead of faulting.
//!
//! Title memory may be unmapped, remapped or made read-only by the title's
//! other threads while we copy it, so copies went through the kernel
//! (`ReadProcessMemory`, `NtWriteVirtualMemory`), which reports a missing
//! page instead of faulting: some 35,000 calls a frame, each of which locks
//! the pages it copies, 39 ms of kernel time on the submitting thread alone.
//!
//! [`copy`] is one `rep movsb`. A fault in it is taken by the handler of
//! [`crate::watch`] (`on_write`, the first in the process), which opens a
//! block we guard ourselves and lets the copy go on, and otherwise sends it
//! to its failing exit. `VICTORY_SAFE_COPY=0`: through the kernel, as before.

use std::sync::OnceLock;

std::arch::global_asm!(
    ".globl victory_safe_copy",
    ".globl victory_safe_copy_at",
    ".globl victory_safe_copy_after",
    ".globl victory_safe_copy_failed",
    // rcx destination, rdx source, r8 bytes; eax: whether all were copied.
    // A leaf that leaves the stack alone (the two registers it must give
    // back are kept in volatile ones), so a stack walk through it works.
    "victory_safe_copy:",
    "mov r10, rsi",
    "mov r11, rdi",
    "mov rdi, rcx",
    "mov rsi, rdx",
    "mov rcx, r8",
    "victory_safe_copy_at:",
    "rep movsb",
    "victory_safe_copy_after:",
    "mov eax, 1",
    "mov rsi, r10",
    "mov rdi, r11",
    "ret",
    "victory_safe_copy_failed:",
    "xor eax, eax",
    "mov rsi, r10",
    "mov rdi, r11",
    "ret",
    ".globl victory_safe_put",
    ".globl victory_safe_put_at",
    ".globl victory_safe_put_after",
    ".globl victory_safe_put_failed",
    // rcx destination, rdx what was there, r8 what is to be there, r9 blocks
    // of eight dwords: stores the dwords in which the two differ, and no
    // other byte. eax: whether all could be stored.
    "victory_safe_put:",
    "test r9, r9",
    "jz 4f",
    "vpcmpeqd ymm3, ymm3, ymm3",
    "2:",
    "vmovdqu ymm1, ymmword ptr [r8]",
    "vpcmpeqd ymm2, ymm1, ymmword ptr [rdx]",
    "vmovmskps eax, ymm2",
    "cmp eax, 255",
    "je 3f",
    "vpxor ymm2, ymm2, ymm3",
    "victory_safe_put_at:",
    "vpmaskmovd ymmword ptr [rcx], ymm2, ymm1",
    "victory_safe_put_after:",
    "nop",
    "3:",
    "add rcx, 32",
    "add rdx, 32",
    "add r8, 32",
    "dec r9",
    "jnz 2b",
    "vzeroupper",
    "4:",
    "mov eax, 1",
    "ret",
    "victory_safe_put_failed:",
    "vzeroupper",
    "xor eax, eax",
    "ret",
);

unsafe extern "system" {
    fn victory_safe_copy(dst: *mut u8, src: *const u8, len: usize) -> u32;
    fn victory_safe_copy_at();
    fn victory_safe_copy_after();
    fn victory_safe_copy_failed();
    fn victory_safe_put(dst: *mut u8, was: *const u8, now: *const u8, blocks: usize) -> u32;
    fn victory_safe_put_at();
    fn victory_safe_put_after();
    fn victory_safe_put_failed();
}

/// Puts `now` at `dst` where it differs from `was`, whole dwords (as `dst`
/// is aligned) and not a byte beside them: other threads may be writing
/// those. At the ends, single bytes of a dword only partly in the stretch.
/// False if a page could not be written; then some may have been.
///
/// # Safety
/// As [`copy`]'s.
pub(crate) unsafe fn put_changed(dst: *mut u8, was: &[u8], now: &[u8]) -> bool {
    assert_eq!(was.len(), now.len());
    let n = now.len();
    let head = dst.addr().wrapping_neg() & 3;
    let head = head.min(n);
    let blocks = (n - head) / 32;
    let tail = head + blocks * 32;
    let mut all = true;
    if blocks > 0 {
        // SAFETY: the two slices hold the blocks; the destination's faults
        // leave through the failing exit.
        all &= unsafe { victory_safe_put(dst.add(head), was[head..].as_ptr(), now[head..].as_ptr(), blocks) != 0 };
    }
    let single = |from: usize, to: usize| {
        let mut all = true;
        let mut i = from;
        while i < to {
            // A whole aligned dword, or the bytes there are of one.
            let len = (4 - (dst.addr() + i) % 4).min(to - i);
            if was[i..i + len] != now[i..i + len] {
                // SAFETY: inside the stretch.
                all &= unsafe { copy(dst.add(i), now[i..].as_ptr(), len) };
            }
            i += len;
        }
        all
    };
    all &= single(0, head);
    all &= single(tail, n);
    all
}

/// Whether copies are made here and not by the kernel.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_SAFE_COPY").is_none_or(|v| v != "0");
        if on {
            crate::watch::install();
        }
        on
    })
}

/// Copies `len` bytes. False if a page of the source cannot be read or one
/// of the destination cannot be written; then some of the bytes may have
/// been copied.
///
/// # Safety
/// Of the two ranges, what is not title memory must be valid, and no Rust
/// reference may cover the destination. [`on`] must have been asked.
pub(crate) unsafe fn copy(dst: *mut u8, src: *const u8, len: usize) -> bool {
    // SAFETY: a fault on either side leaves through the failing exit
    // ([`failing_exit`]); the rest is the caller's.
    unsafe { victory_safe_copy(dst, src, len) != 0 }
}

/// For the fault handler: where a fault at instruction `at` is to go on, if
/// it is [`copy`]'s.
pub(crate) fn failing_exit(at: usize) -> Option<usize> {
    let (from, to) = (victory_safe_copy_at as *const () as usize, victory_safe_copy_after as *const () as usize);
    if (from..to).contains(&at) {
        return Some(victory_safe_copy_failed as *const () as usize);
    }
    let (from, to) = (victory_safe_put_at as *const () as usize, victory_safe_put_after as *const () as usize);
    (from..to).contains(&at).then_some(victory_safe_put_failed as *const () as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, VirtualAlloc,
        VirtualFree, VirtualProtect,
    };

    struct Pages(*mut u8, usize);

    impl Pages {
        fn new(len: usize) -> Pages {
            // SAFETY: plain allocation.
            let at = unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
            assert!(!at.is_null());
            Pages(at.cast(), len)
        }
        fn fill(&self) {
            for i in 0..self.1 {
                // SAFETY: inside the allocation.
                unsafe { self.0.add(i).write((i * 31 + 7) as u8) };
            }
        }
    }

    impl Drop for Pages {
        fn drop(&mut self) {
            // SAFETY: our allocation.
            unsafe { VirtualFree(self.0.cast(), 0, MEM_RELEASE) };
        }
    }

    #[test]
    fn copies_every_length_and_alignment() {
        assert!(on());
        let (a, b) = (Pages::new(1 << 16), Pages::new(1 << 16));
        a.fill();
        for len in [0usize, 1, 3, 4, 8, 31, 64, 255, 4095, 4096, 4097, 60_000] {
            for (da, sa) in [(0usize, 0usize), (1, 0), (0, 3), (5, 2)] {
                // SAFETY: inside the two allocations.
                unsafe {
                    std::ptr::write_bytes(b.0, 0xEE, b.1);
                    assert!(copy(b.0.add(da), a.0.add(sa), len));
                    let (to, from) = (std::slice::from_raw_parts(b.0, b.1), std::slice::from_raw_parts(a.0, a.1));
                    assert_eq!(to[da..da + len], from[sa..sa + len]);
                    assert!(to[..da].iter().chain(&to[da + len..]).all(|&x| x == 0xEE));
                }
            }
        }
    }

    /// The dwords that changed are stored, and no byte of any other, at
    /// every alignment and length; a page that is not there fails.
    #[test]
    fn only_changed_dwords_are_put() {
        assert!(on());
        let pages = Pages::new(3 << 12);
        for at in [0usize, 1, 2, 3, 4, 13, 4090] {
            for len in [0usize, 1, 3, 4, 5, 31, 32, 33, 64, 100, 1000, 4096] {
                let was: Vec<u8> = (0..len).map(|i| (i * 13 + 1) as u8).collect();
                for step in [1usize, 5, 17, 64, 10_000] {
                    let mut now = was.clone();
                    for i in (0..len).step_by(step) {
                        now[i] ^= 0x5A;
                    }
                    // Other threads changed everything meanwhile.
                    // SAFETY: inside the allocation.
                    unsafe {
                        std::ptr::write_bytes(pages.0, 0xC3, pages.1);
                        assert!(put_changed(pages.0.add(at), &was, &now));
                        let there = std::slice::from_raw_parts(pages.0, pages.1);
                        let base = pages.0.addr();
                        for (k, &byte) in there.iter().enumerate() {
                            let expect = if (at..at + len).contains(&k) {
                                let dword = (base + k) / 4;
                                let changed = (0..len).any(|i| now[i] != was[i] && (base + at + i) / 4 == dword);
                                if changed { now[k - at] } else { 0xC3 }
                            } else {
                                0xC3
                            };
                            if byte != expect {
                                panic!("at {at} len {len} step {step} byte {k}: {byte:#x}, not {expect:#x}");
                            }
                            if k > at + len + 64 {
                                break;
                            }
                        }
                    }
                }
            }
        }
        // SAFETY: pages of our allocation.
        unsafe {
            VirtualFree(pages.0.add(4096).cast(), 4096, MEM_DECOMMIT);
            let (was, now) = (vec![0u8; 8192], vec![1u8; 8192]);
            assert!(!put_changed(pages.0, &was, &now));
            // Nothing differs on the page that is not there: nothing fails.
            let mut now = was.clone();
            now[0] = 1;
            assert!(put_changed(pages.0, &was, &now));
        }
    }

    /// What the copies cost (ignored: a measurement): 64 KiB pieces of
    /// scattered memory into a buffer the card reads, by the kernel and here.
    #[test]
    #[ignore]
    fn copy_costs() {
        use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        assert!(on());
        let Some(host) = crate::host::host() else { return };
        const N: usize = 128 << 20;
        let from = Pages::new(N);
        from.fill();
        let (_buffer, shared) = host.shared_buffer(N as u64).expect("a shared buffer");
        let plain = Pages::new(N);
        plain.fill();
        for (what, to) in [("a buffer the card reads", shared), ("plain memory", plain.0)] {
            for piece in [256usize, 4 << 10, 64 << 10] {
                let count = (N / piece).min(4096);
                // A different, scattered piece each time.
                let at = |i: usize| (i * 7919 % (N / piece)) * piece;
                let time = |f: &dyn Fn(usize) -> bool| {
                    let mut best = f64::MAX;
                    for _ in 0..5 {
                        let t = std::time::Instant::now();
                        for i in 0..count {
                            assert!(f(at(i)));
                        }
                        best = best.min(t.elapsed().as_secs_f64());
                    }
                    (best * 1e6 / count as f64, count as f64 * piece as f64 / best / 1e9)
                };
                // SAFETY: inside the buffers.
                let kernel = time(&|o| unsafe {
                    let mut done = 0;
                    ReadProcessMemory(GetCurrentProcess(), from.0.add(o).cast(), to.add(o).cast(), piece, &mut done) != 0
                });
                // SAFETY: as above.
                let here = time(&|o| unsafe { copy(to.add(o), from.0.add(o), piece) });
                // SAFETY: as above.
                let std_copy = time(&|o| unsafe {
                    std::ptr::copy_nonoverlapping(from.0.add(o), to.add(o), piece);
                    true
                });
                println!(
                    "{piece:>6} bytes into {what}: kernel {:.2} us ({:.1} GB/s), rep movsb {:.2} us ({:.1} GB/s), memcpy {:.2} us ({:.1} GB/s)",
                    kernel.0, kernel.1, here.0, here.1, std_copy.0, std_copy.1
                );
            }
        }
    }

    /// Missing, forbidden and read-only pages: the copy fails, at any byte,
    /// and the process goes on.
    #[test]
    fn faults_are_failures() {
        assert!(on());
        let (a, b) = (Pages::new(3 << 12), Pages::new(3 << 12));
        a.fill();
        let mut old = 0;
        // SAFETY: pages of our allocations.
        unsafe {
            assert!(copy(b.0, a.0, b.1));
            VirtualProtect(a.0.add(4096).cast(), 4096, PAGE_NOACCESS, &mut old);
            assert!(!copy(b.0, a.0, b.1));
            assert!(copy(b.0, a.0, 4096));
            assert!(!copy(b.0, a.0.add(8191), 1));
            assert!(copy(b.0, a.0.add(8192), 4096));
            VirtualProtect(a.0.add(4096).cast(), 4096, PAGE_READWRITE, &mut old);
            VirtualFree(a.0.add(8192).cast(), 4096, MEM_DECOMMIT);
            assert!(!copy(b.0, a.0, b.1));
            VirtualProtect(b.0.add(4096).cast(), 4096, PAGE_READONLY, &mut old);
            assert!(!copy(b.0, a.0, 8192));
            assert!(!copy(std::ptr::without_provenance_mut(0x10), a.0, 4));
            assert!(!copy(b.0, std::ptr::without_provenance(usize::MAX - 7), 4));
            VirtualProtect(b.0.add(4096).cast(), 4096, PAGE_READWRITE, &mut old);
        }
        // Many faults, on several threads at once.
        let at = a.0 as usize;
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(move || {
                    let mut to = [0u8; 64];
                    for _ in 0..500 {
                        // SAFETY: a decommitted page and our own buffer.
                        assert!(!unsafe { copy(to.as_mut_ptr(), (at + 8192) as *const u8, 64) });
                    }
                });
            }
        });
    }
}
