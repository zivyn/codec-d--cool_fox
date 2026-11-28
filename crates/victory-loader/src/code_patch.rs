//! Replaces a few bytes of the title's code before it runs.

use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Replaces the `N` bytes at `at`, which must be `was`, with `new`; the page's
/// protection is put back. Whether it did.
///
/// # Safety
/// `at` is in mapped code of the image and nothing runs it yet (or `new` does
/// for every thread what `was` did).
pub unsafe fn replace<const N: usize>(at: usize, was: [u8; N], new: [u8; N]) -> bool {
    let mut old = 0u32;
    // SAFETY: `N` bytes of mapped code, made writable for the copy.
    unsafe {
        if VirtualProtect(at as *const _, N, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return false;
        }
        let here = at as *mut [u8; N];
        let done = here.read_unaligned() == was;
        if done {
            here.write_unaligned(new);
            FlushInstructionCache(GetCurrentProcess(), at as *const _, N);
        }
        let mut ignored = 0u32;
        VirtualProtect(at as *const _, N, old, &mut ignored);
        done
    }
}
