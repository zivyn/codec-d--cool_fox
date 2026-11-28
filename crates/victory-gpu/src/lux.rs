//! A test lever for the title's "lux compression" block (EXPO3).
//!
//! The title keeps a 16-byte block of one number, `lux`, that its luminance passes and its composite
//! read (the composite scales its bloom by it). A 1x1 compute pass moves it each frame towards
//! `s1 + (s0 - s1) * sqrt(saturate((mean - s2) / (s3 - s2)))`, with the range `[s1, s0]` and the mean's
//! range `[s2, s3]` read from its constants (dwords 16 to 19) and the mean measured with `lux` itself,
//! so `lux` follows the scene's mean luminance and sits where `lux = F(lux * L)` holds.
//!
//! `VICTORY_LUX=<value>` (unset: the title's own) writes `value` into both ends of that range, in the
//! title's constants, before the pass runs: the target is the value whatever the mean, and the block
//! follows it at the title's own rate (about 1 - 2^-0.096 per frame). It is live in the A/B
//! (`VICTORY_AB=12,VICTORY_LUX=200`): A is the title's, B the value. It changes nothing else.

/// The program's name, as its container has it.
pub(crate) fn program(container: &[u8]) -> bool {
    const NAME: &[u8] = b"ComputeLuxCompressionInfoSmooth";
    container.windows(NAME.len()).any(|w| w == NAME)
}

/// `VICTORY_LUX`: a value from 1 to 1024; anything else (or unset) leaves the title's range.
pub(crate) fn parse(text: Option<&str>) -> f32 {
    text.and_then(|t| t.trim().parse().ok())
        .filter(|v: &f32| v.is_finite() && (1.0..=1024.0).contains(v))
        .unwrap_or(0.0)
}

/// The value now, or `None` for the title's own (live in the A/B: read at each dispatch of the program).
pub(crate) fn forced() -> Option<f32> {
    let value = crate::ab::live_value!("VICTORY_LUX", f32, parse);
    (value > 0.0).then_some(value)
}

/// Dwords of the program's constants that hold the ends of the lux range (max, min).
const MAX_DWORD: usize = 16;
const MIN_DWORD: usize = 17;
/// Bytes the constants must have for both.
pub(crate) const NEEDS: u32 = 4 * (MIN_DWORD as u32 + 1);

/// Writes `value` into both ends of the range of the constants at `address` (title memory of at least [`NEEDS`] bytes).
///
/// # Safety
/// `address` must be committed, writable title memory of at least [`NEEDS`] bytes.
pub(crate) unsafe fn force(address: u64, value: f32) {
    let words = address as *mut u32;
    // SAFETY: by the caller.
    unsafe {
        words.add(MAX_DWORD).write_unaligned(value.to_bits());
        words.add(MIN_DWORD).write_unaligned(value.to_bits());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_value_is_a_lux_from_1_to_1024_or_the_titles_own() {
        assert_eq!(parse(None), 0.0);
        assert_eq!(parse(Some("200")), 200.0);
        assert_eq!(parse(Some(" 1 ")), 1.0);
        assert_eq!(parse(Some("1024")), 1024.0);
        for odd in ["", "0", "0.5", "1025", "-3", "nan", "inf", "more"] {
            assert_eq!(parse(Some(odd)), 0.0, "{odd:?}");
        }
    }

    #[test]
    fn the_program_is_found_by_its_name() {
        assert!(program(
            b"xx avgLuminanceCompute:ComputeLuxCompressionInfoSmooth_5b8554dd yy"
        ));
        assert!(!program(b"ComputeLuxCompressionInfo_5b8554dd"));
        assert!(!program(b""));
    }

    #[test]
    fn both_ends_of_the_range_are_written_and_nothing_else() {
        let mut words = [7u32; 24];
        // SAFETY: a buffer of 96 bytes.
        unsafe { force(words.as_mut_ptr() as u64, 200.0) };
        for (i, w) in words.iter().enumerate() {
            let want = if i == 16 || i == 17 {
                200f32.to_bits()
            } else {
                7
            };
            assert_eq!(*w, want, "dword {i}");
        }
    }
}
