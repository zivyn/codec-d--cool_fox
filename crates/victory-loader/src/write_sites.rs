//! Places in the title's code that set a bit of a byte which is nearly always set already, changed
//! at load to look first (`VICTORY_WRITE_SITES=<file>`; off without it).
//!
//! The title's threads raise flags in their objects at every use ("drawn this frame"): `or byte
//! [object+n], bit`. The objects lie in memory that copies for the card are kept of, and each such
//! write was a fault on a guarded page, a copy of its block and a guard again, for a byte that
//! stayed as it was (a fifth of a frame's write faults at one place, run faults-3). Here the
//! instruction becomes a jump to code of ours that reads the byte, leaves the flags the instruction
//! leaves, and runs the instruction only if the byte would change.
//!
//! The places are not ours to know: `victory-gpu` (`titlewrite`) finds them while the title runs,
//! by their faults, and writes them into that file, a line each: the image file's name, the place's
//! offset in it (hexadecimal). Nothing of the title's code is kept. What is not such an instruction
//! is left alone.

use std::path::Path;

/// An `or byte [m], constant` at the start of `code`: its length, the bytes that name its address
/// (prefix or 0, ModRM without its register field, what follows up to the constant), the constant.
fn or_byte(code: &[u8]) -> Option<(usize, u8, u8, &[u8], u8)> {
    let mut at = 0;
    let mut rex = 0;
    if code.first()? & 0xf0 == 0x40 {
        rex = code[0];
        at = 1;
    }
    let (&op, &modrm) = (code.get(at)?, code.get(at + 1)?);
    if op != 0x80 || modrm >> 3 & 7 != 1 {
        return None;
    }
    let (mode, rm) = (modrm >> 6, modrm & 7);
    // Not a register, nor the instruction pointer's neighbourhood.
    if mode == 3 || mode == 0 && rm == 5 {
        return None;
    }
    let rest = at + 2;
    let mut end = rest;
    if rm == 4 {
        let sib = *code.get(end)?;
        // Not the stack, whose pointer is another in our code; a plain address is left too.
        if sib & 7 == 4 && rex & 1 == 0 || sib & 7 == 5 && mode == 0 {
            return None;
        }
        end += 1;
    }
    end += match mode {
        1 => 1,
        2 => 4,
        _ => 0,
    };
    let constant = *code.get(end)?;
    Some((end + 1, rex, modrm, &code[rest..end], constant))
}

/// The code at `stub` for the instruction at `at` (`code` from there).
fn stub(code: &[u8], at: usize, stub: usize) -> Option<(usize, Vec<u8>)> {
    let (len, rex, modrm, rest, constant) = or_byte(code)?;
    if len < 5 {
        return None;
    }
    let mut out = vec![0x50, 0x51]; // push rax, push rcx
    // movzx eax, byte [m] (the prefix's register field is none of it)
    if rex & 0x43 != 0 {
        out.push(rex & 0x43);
    }
    out.extend([0x0f, 0xb6, modrm & 0xc7]);
    out.extend(rest);
    out.extend([0x89, 0xc1]); // mov ecx, eax
    out.extend([0xf7, 0xd1]); // not ecx
    out.extend([0x81, 0xe1, constant, 0, 0, 0]); // and ecx, constant: the bits to be set
    out.extend([0x0c, constant]); // or al, constant: the flags
    let back = |out: &mut Vec<u8>| {
        let from = stub + out.len();
        out.extend(crate::console_insn::jump(from, at + len)?);
        Some(())
    };
    let mut write = vec![0x59, 0x58]; // pop rcx, pop rax
    write.extend(&code[..len]);
    out.extend([0x67, 0xe3, u8::try_from(write.len() + 5).ok()?]); // jecxz past it
    out.extend(write);
    back(&mut out)?;
    out.extend([0x59, 0x58]);
    back(&mut out)?;
    Some((len, out))
}

/// Changes the places of `list` that are in the image called `name`, mapped at `base` with `size`
/// bytes: how many.
///
/// # Safety
/// The image stays mapped, and no thread runs its code yet.
pub unsafe fn apply(name: &str, base: usize, size: usize, list: &str) -> usize {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut room: Option<(usize, usize)> = None;
    let mut done = 0;
    let mut seen = std::collections::BTreeSet::new();
    for line in list.lines() {
        let mut words = line.split_whitespace();
        let (Some(image), Some(offset)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(offset) = usize::from_str_radix(offset.trim_start_matches("0x"), 16) else {
            continue;
        };
        if !image.eq_ignore_ascii_case(name) || offset + 16 > size || !seen.insert(offset) {
            continue;
        }
        let at = base + offset;
        if room.is_none() {
            room = crate::console_insn::near(at).map(|place| (place, 0));
        }
        let Some((place, used)) = room.as_mut() else {
            return done;
        };
        // SAFETY: sixteen bytes inside the image; what is not mapped in it is not named.
        let code: [u8; 16] = unsafe { std::ptr::read_unaligned(at as *const _) };
        let to = *place + *used;
        let Some((len, body)) = stub(&code, at, to) else {
            tracing::warn!(image, offset = format_args!("{offset:#x}"), "not a write of the kind that can be looked at first");
            continue;
        };
        let Some(jump) = crate::console_insn::jump(at, to) else {
            continue;
        };
        if *used + body.len() > crate::console_insn::STUBS_LEN {
            return done;
        }
        let mut old = 0;
        // SAFETY: our own code memory, which nothing runs yet; then the instruction, whose page is
        // made writable for it and put back, and which nothing runs either.
        unsafe {
            std::ptr::copy_nonoverlapping(body.as_ptr(), to as *mut u8, body.len());
            if VirtualProtect(at as *const _, len, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
                continue;
            }
            std::ptr::write_bytes(at as *mut u8, 0x90, len);
            std::ptr::copy_nonoverlapping(jump.as_ptr(), at as *mut u8, jump.len());
            VirtualProtect(at as *const _, len, old, &mut old);
            FlushInstructionCache(GetCurrentProcess(), at as *const _, len);
        }
        *used += body.len().next_multiple_of(16);
        done += 1;
    }
    done
}

/// [`apply`] with the file `VICTORY_WRITE_SITES` names.
///
/// # Safety
/// As [`apply`].
pub unsafe fn apply_asked(name: &str, base: usize, size: usize) {
    let Some(file) = std::env::var_os("VICTORY_WRITE_SITES") else {
        return;
    };
    let Ok(list) = std::fs::read_to_string(Path::new(&file)) else {
        return;
    };
    // SAFETY: by the caller.
    let places = unsafe { apply(name, base, size, &list) };
    tracing::info!(image = name, places, "writes of the title's that look first (VICTORY_WRITE_SITES)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_can_be_looked_at_first() {
        // or byte [r9+9eh], 1
        let (len, rex, modrm, rest, constant) = or_byte(&[0x41, 0x80, 0x89, 0x9e, 0, 0, 0, 1, 0, 0]).unwrap();
        assert_eq!((len, rex, modrm, rest, constant), (8, 0x41, 0x89, &[0x9e, 0, 0, 0][..], 1));
        // or byte [r9+31h], 80h; [rax+rcx*8+5]; [r12+5] (a base of its own)
        assert_eq!(or_byte(&[0x41, 0x80, 0x49, 0x31, 0x80]).unwrap().0, 5);
        assert_eq!(or_byte(&[0x80, 0x4c, 0xc8, 5, 1]).unwrap().0, 5);
        assert_eq!(or_byte(&[0x41, 0x80, 0x4c, 0x24, 5, 1]).unwrap().0, 6);
        // Of the stack, beside the instruction, a register's, another operation.
        assert!(or_byte(&[0x80, 0x4c, 0x24, 5, 1]).is_none());
        assert!(or_byte(&[0x80, 0x0d, 1, 0, 0, 0, 1]).is_none());
        assert!(or_byte(&[0x80, 0xc9, 1]).is_none());
        assert!(or_byte(&[0x80, 0x61, 5, 1, 0]).is_none());
    }

    /// The instruction in code of a test's, changed, run: the byte, the registers and the flags
    /// are what it makes of them unchanged.
    #[test]
    fn a_changed_write_does_what_it_did() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualProtect};
        type F = unsafe extern "system" fn(*mut u8, *mut [u64; 3]);
        for (constant, with) in [(0x80u8, 0x49u8), (1, 0x49), (0x7f, 0x49), (0xff, 0x48), (0x10, 0x49)] {
            // mov rax, rcx ; mov r9, rcx ; mov rcx, 1234h ; xor r10, r10 ; or byte [r9+31h | rax+31h], constant ;
            // pushfq ; pop r10 ; mov [rdx], r10 ; mov [rdx+8], rax ; mov [rdx+16], rcx ; ret
            let mut code = vec![0x48, 0x89, 0xc8, 0x49, 0x89, 0xc9, 0x48, 0xc7, 0xc1, 0x34, 0x12, 0, 0, 0x4d, 0x31, 0xd2];
            let at = code.len();
            if with == 0x49 {
                code.extend([0x41, 0x80, 0x49, 0x31, constant]);
            } else {
                // (With a long address, for the five bytes.)
                code.extend([0x80, 0x88, 0x31, 0, 0, 0, constant]);
            }
            code.extend([0x9c, 0x41, 0x5a, 0x4c, 0x89, 0x12, 0x48, 0x89, 0x42, 0x08, 0x48, 0x89, 0x4a, 0x10, 0xc3]);
            // SAFETY: fresh memory for code, twice, one of them changed before it runs.
            unsafe {
                let place = VirtualAlloc(std::ptr::null(), 0x10000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as usize;
                assert_ne!(place, 0);
                std::ptr::copy_nonoverlapping(code.as_ptr(), place as *mut u8, code.len());
                let second = place + 0x1000;
                std::ptr::copy_nonoverlapping(code.as_ptr(), second as *mut u8, code.len());
                let mut old = 0;
                VirtualProtect(second as *const _, 0x1000, PAGE_EXECUTE_READ, &mut old);
                let list = format!("other.dll {:x}\nT.dll 0x{at:x}\nt.dll {at:x}\nt.dll 5\nt.dll\n", at);
                assert_eq!(apply("t.dll", second, 0x1000, &list), 1);
                assert_eq!(std::ptr::read((second + at) as *const u8), 0xe9);
                let (plain, changed): (F, F) = (std::mem::transmute(place), std::mem::transmute(second));
                for was in 0..=255u8 {
                    let mut bytes = [[0u8; 0x40]; 2];
                    bytes[0][0x31] = was;
                    bytes[1][0x31] = was;
                    let mut out = [[0u64; 3]; 2];
                    plain(bytes[0].as_mut_ptr(), &mut out[0]);
                    changed(bytes[1].as_mut_ptr(), &mut out[1]);
                    assert_eq!(bytes[0], bytes[1], "{was:x} | {constant:x}");
                    out[0][1] = out[0][1].wrapping_sub(bytes[0].as_ptr() as u64);
                    out[1][1] = out[1][1].wrapping_sub(bytes[1].as_ptr() as u64);
                    // (AF is left as it likes.)
                    out[0][0] &= 0x8c5;
                    out[1][0] &= 0x8c5;
                    assert_eq!(out[0], out[1], "{was:x} | {constant:x}");
                }
            }
        }
    }

    /// A byte that would not change is not written.
    #[test]
    fn a_byte_that_stays_is_not_written() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, PAGE_READONLY, VirtualAlloc, VirtualProtect};
        // mov r9, rcx ; or byte [r9+31h], 80h ; ret
        let code = [0x49, 0x89, 0xc9, 0x41, 0x80, 0x49, 0x31, 0x80, 0xc3];
        // SAFETY: fresh memory for code and for a byte, which is read-only when the code runs.
        unsafe {
            let place = VirtualAlloc(std::ptr::null(), 0x10000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as usize;
            std::ptr::copy_nonoverlapping(code.as_ptr(), place as *mut u8, code.len());
            assert_eq!(apply("t.dll", place, 0x10000, "t.dll 3"), 1);
            let data = VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as *mut u8;
            data.add(0x31).write(0x83);
            let mut old = 0;
            VirtualProtect(data.cast(), 4096, PAGE_READONLY, &mut old);
            let f: unsafe extern "system" fn(*mut u8) = std::mem::transmute(place);
            f(data);
            assert_eq!(data.add(0x31).read(), 0x83);
        }
    }
}
