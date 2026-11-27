//! The title sizes its memory pools by the device class, and a Series S gets
//! the small ones.
//!
//! The title reads the device type once (the `ConsoleMode` value, see the
//! console's HLE) and keeps a "this is a Series S" byte that some seventy
//! places of its code ask: render scale, counts, pool sizes. One routine of
//! its start-up sets the size of the pools the streaming draws on, a Series S
//! with about 3.9 GiB for the largest and a Series X with about 6.1 GiB, and
//! smaller ones in the same proportion for the others. A run as a Series S
//! (the fork's launcher: 720p internal, the card's 8 GB) streams within those.
//!
//! `VICTORY_TITLE_POOLS=series-x` makes that one routine size the pools as on
//! a Series X while the device stays what `--console` says (so the render
//! scale and everything else that asks the byte stay a Series S's). The
//! routine is found by a short byte pattern of its code, not by an offset, so
//! a build that has the same code is covered and a build that does not is left
//! untouched; what is not found, or not found once, is not touched. The one
//! call to the "is this a Series S" routine in it is replaced by an instruction
//! that clears the register the title tests next (five bytes, so the title's
//! own flow is unchanged).

use crate::watchdog::{Pattern, matches, pattern};

/// The switch.
pub const ENV: &str = "VICTORY_TITLE_POOLS";

/// What the switch names: the pools of a Series X.
pub fn wanted(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "series-x" | "seriesx" | "x"
        )
    })
}

/// The routine's head: the stack, the call that says whether this is a Series
/// S (its offset is not part of the pattern), the test of the answer, and the
/// first three sizes loaded before the choice between the two sets.
const SIZES: Pattern = pattern![
    0x48, 0x83, 0xec, 0x28, 0xe8, _, _, _, _, 0x84, 0xc0, 0xb8, 0x00, 0x00, 0x50, 0x40, 0xb9, 0x00,
    0x00, 0xf0, 0x73, 0xba, 0x00, 0x00, 0x60, 0x0f
];

/// Where the call is in [`SIZES`], and what it is.
const CALL_AT: usize = 4;
pub const CALL: usize = 5;

/// What the call becomes: `xor eax, eax` and a three-byte `nop`. The title
/// tests `al` next, so the answer is "not a Series S".
pub const NOT_SERIES_S: [u8; CALL] = [0x31, 0xc0, 0x0f, 0x1f, 0x00];

/// The offset from the image of the call, from the executable sections
/// `(rva, bytes)`; `None` unless the routine is found exactly once.
pub fn locate(sections: &[(u32, &[u8])]) -> Option<u32> {
    let mut found = None;
    let mut n = 0;
    for &(rva, code) in sections {
        for at in matches(code, SIZES) {
            n += 1;
            found = rva.checked_add(u32::try_from(at + CALL_AT).ok()?);
        }
    }
    found.filter(|_| n == 1)
}

/// What [`apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The switch is not set.
    Off,
    /// The routine is not in this build's code (or is there twice).
    NotFound,
    /// The write was refused.
    Refused(u32),
    /// The call at this offset from the image now answers "not a Series S".
    Patched(u32),
}

/// Looks for the routine in the image at `base` (executable sections
/// `(rva, bytes)`) when the switch is set, and has `write(address, was, new)`
/// replace the call. `write` says whether it did; it is given the five bytes
/// that must be there and the five that replace them.
///
/// # Safety
/// The sections are mapped bytes of the image at `base`, which stays mapped,
/// and `write` is for code nothing has run yet.
pub unsafe fn apply(
    value: Option<&str>,
    base: usize,
    code: &[(u32, u32)],
    write: impl FnOnce(usize, [u8; CALL], [u8; CALL]) -> bool,
) -> Applied {
    if !wanted(value) {
        return Applied::Off;
    }
    let sections: Vec<(u32, &[u8])> = code
        .iter()
        // SAFETY: a mapped section of the image, by this function's contract.
        .map(|&(rva, len)| {
            (rva, unsafe {
                std::slice::from_raw_parts((base + rva as usize) as *const u8, len as usize)
            })
        })
        .collect();
    let Some(rva) = locate(&sections) else {
        return Applied::NotFound;
    };
    // SAFETY: inside the section the pattern was found in.
    let was: [u8; CALL] =
        unsafe { std::ptr::read_unaligned((base + rva as usize) as *const [u8; CALL]) };
    if was[0] != 0xe8 || !write(base + rva as usize, was, NOT_SERIES_S) {
        return Applied::Refused(rva);
    }
    Applied::Patched(rva)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A routine like the title's: our own bytes, with a call of any offset.
    fn routine(call: [u8; 4]) -> Vec<u8> {
        let mut code = vec![0x48, 0x83, 0xec, 0x28, 0xe8];
        code.extend(call);
        code.extend([
            0x84, 0xc0, 0xb8, 0x00, 0x00, 0x50, 0x40, 0xb9, 0x00, 0x00, 0xf0, 0x73, 0xba, 0x00,
            0x00, 0x60, 0x0f,
        ]);
        code.extend([0x41, 0xb9, 0x00, 0x00, 0x00, 0x01, 0xc3]);
        code
    }

    /// Writes into the test's buffer, which is the section.
    fn poke(at: usize, new: [u8; CALL]) -> bool {
        // SAFETY: `at` is five bytes inside the buffer, by the caller's address.
        unsafe { std::ptr::copy_nonoverlapping(new.as_ptr(), at as *mut u8, CALL) };
        true
    }

    fn image(pieces: &[&[u8]]) -> Vec<u8> {
        let mut code = vec![0xcc; 16];
        for piece in pieces {
            code.extend_from_slice(piece);
            code.extend([0xcc; 24]);
        }
        code
    }

    #[test]
    fn the_switch_names_the_series_x() {
        assert!(wanted(Some("series-x")));
        assert!(wanted(Some(" Series-X ")));
        assert!(wanted(Some("x")));
        assert!(!wanted(Some("series-s")));
        assert!(!wanted(Some("")));
        assert!(!wanted(None));
    }

    #[test]
    fn the_call_is_found_once_whatever_its_offset() {
        let a = image(&[&routine([1, 2, 3, 4])]);
        assert_eq!(locate(&[(0x1000, &a)]), Some(0x1000 + 16 + CALL_AT as u32));
        let b = image(&[&routine([0xfe, 0xff, 0xff, 0x7f])]);
        assert_eq!(locate(&[(0x2000, &b)]), Some(0x2000 + 16 + CALL_AT as u32));
    }

    #[test]
    fn a_build_without_the_routine_or_with_two_is_not_touched() {
        assert_eq!(
            locate(&[(0, &image(&[&[0x48, 0x83, 0xec, 0x28, 0xc3]]))]),
            None
        );
        let twice = image(&[&routine([1, 2, 3, 4]), &routine([5, 6, 7, 8])]);
        assert_eq!(locate(&[(0, &twice)]), None);
        // One in each of two sections is two as well.
        let one = image(&[&routine([1, 2, 3, 4])]);
        assert_eq!(locate(&[(0, &one), (0x10000, &one)]), None);
        // A change in the loaded sizes is another routine.
        let mut other = routine([1, 2, 3, 4]);
        other[11] = 0x41;
        assert_eq!(locate(&[(0, &image(&[&other]))]), None);
    }

    #[test]
    fn the_call_becomes_the_answer_not_a_series_s() {
        let mut code = image(&[&routine([0x10, 0x20, 0x30, 0x40])]);
        let base = code.as_mut_ptr() as usize;
        let len = code.len() as u32;
        let mut seen = None;
        // SAFETY: the buffer is the section; the closure writes into it.
        let done = unsafe {
            apply(Some("series-x"), base, &[(0, len)], |at, was, new| {
                seen = Some((at - base, was));
                poke(at, new)
            })
        };
        let at = 16 + CALL_AT;
        assert_eq!(done, Applied::Patched(at as u32));
        assert_eq!(seen, Some((at, [0xe8, 0x10, 0x20, 0x30, 0x40])));
        assert_eq!(code[at..at + CALL], NOT_SERIES_S);
        // The test of the answer and the sizes after it are as they were.
        assert_eq!(code[at + CALL..at + CALL + 2], [0x84, 0xc0]);
        // Applied twice, nothing is found any more: the call is gone.
        // SAFETY: as above; the closure must not run.
        let again = unsafe {
            apply(
                Some("series-x"),
                base,
                &[(0, len)],
                |_, _, _| unreachable!(),
            )
        };
        assert_eq!(again, Applied::NotFound);
    }

    #[test]
    fn nothing_is_read_or_written_unless_asked() {
        let code = image(&[&routine([1, 2, 3, 4])]);
        // SAFETY: the closure must not run, and no section is read.
        let off = unsafe {
            apply(
                None,
                code.as_ptr() as usize,
                &[(0, code.len() as u32)],
                |_, _, _| unreachable!(),
            )
        };
        assert_eq!(off, Applied::Off);
        let s = unsafe {
            apply(
                Some("series-s"),
                code.as_ptr() as usize,
                &[(0, code.len() as u32)],
                |_, _, _| unreachable!(),
            )
        };
        assert_eq!(s, Applied::Off);
    }

    #[test]
    fn a_refused_write_is_reported() {
        let code = image(&[&routine([1, 2, 3, 4])]);
        // SAFETY: the section is the buffer; the closure refuses.
        let refused = unsafe {
            apply(
                Some("x"),
                code.as_ptr() as usize,
                &[(0, code.len() as u32)],
                |_, _, _| false,
            )
        };
        assert_eq!(refused, Applied::Refused(16 + CALL_AT as u32));
    }
}
