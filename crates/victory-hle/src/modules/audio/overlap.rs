//! The XMA overlap buffer (`VICTORY_XMA_OVERLAP=1`).
//!
//! The decoder's frames overlap by half: the second half of each frame is
//! added to the first half of the next. The hardware keeps what carries over
//! in a buffer the title gives each context (word 8) and copies from one
//! context to the next of the same voice, so that a stream goes on without a
//! seam. We keep the decoder itself instead (its overlap and the scale
//! factors it predicts from), in a table here, and stamp a 16-byte token in
//! the title's buffer: copied with the buffer, it names the state in the
//! context that gets the copy. A buffer without a token (a stream's start,
//! a buffer cleared) starts cold, as before.
//!
//! With the switch on, a context whose input runs out does not flush its
//! tail either (the tail belongs to the frame that follows); a stream that
//! really ends loses the last 5 ms of its fade.
//!
//! A state is used once and only soon after it was kept
//! (`VICTORY_XMA_OVERLAP_MS`, 500): a title reuses overlap buffers and
//! contexts for other sounds, and joining the start of another sound to an
//! old tail is a click, not a seam.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use super::stats::{COUNTERS, Overlap};

use victory_formats::xma::Decoder;

const MAGIC: u32 = 0x4c56_564f;
/// Bytes of the token at the start of the overlap buffer.
pub const TOKEN_BYTES: usize = 16;
/// States kept: the voices in flight, a few more.
const KEPT: usize = 64;

pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("VICTORY_XMA_OVERLAP").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

/// How long after it was kept a state may be used, in milliseconds.
pub fn max_age_ms() -> u128 {
    static AGE: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
    *AGE.get_or_init(|| {
        std::env::var("VICTORY_XMA_OVERLAP_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(500)
    })
}

/// Whether `age_ms` is too long after a state was kept.
pub fn stale(age_ms: u128) -> bool {
    age_ms > max_age_ms()
}

struct Saved {
    at: Instant,
    id: u64,
    rate: u32,
    decoder: Decoder,
}

struct Table {
    next: u64,
    saved: VecDeque<Saved>,
}

static TABLE: Mutex<Table> = Mutex::new(Table {
    next: 1,
    saved: VecDeque::new(),
});

fn check(id: u64) -> u32 {
    (id as u32) ^ ((id >> 32) as u32) ^ MAGIC
}

fn read_id(buffer: &[u8]) -> Option<u64> {
    let word = |at: usize| Some(u32::from_le_bytes(buffer.get(at..at + 4)?.try_into().ok()?));
    let id = u64::from(word(4)?) | u64::from(word(8)?) << 32;
    (word(0)? == MAGIC && word(12)? == check(id)).then_some(id)
}

/// Keeps `decoder` as the state of the voice whose overlap buffer starts
/// with `buffer`, and stamps the buffer with the token that names it. The
/// token this buffer held before is forgotten.
pub fn save(buffer: &mut [u8], decoder: &Decoder, rate: u32) {
    if buffer.len() < TOKEN_BYTES {
        return;
    }
    let old = read_id(buffer);
    let mut table = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = old {
        table.saved.retain(|s| s.id != old);
    }
    let id = table.next;
    table.next += 1;
    if table.saved.len() >= KEPT {
        table.saved.pop_front();
    }
    table.saved.push_back(Saved {
        at: Instant::now(),
        id,
        rate,
        decoder: decoder.clone(),
    });
    drop(table);
    COUNTERS.overlap(Overlap::Saved);
    buffer[..4].copy_from_slice(&MAGIC.to_le_bytes());
    buffer[4..12].copy_from_slice(&id.to_le_bytes());
    buffer[12..16].copy_from_slice(&check(id).to_le_bytes());
}

/// The decoder a buffer's token names, when it matches the stream
/// (channels and sample rate) and was kept recently enough. It is used up:
/// the next context to find the same token (a buffer left in place and
/// reused) starts cold.
pub fn restore(buffer: &[u8], channels: usize, rate: u32) -> Option<Decoder> {
    restore_within(buffer, channels, rate, max_age_ms())
}

fn restore_within(buffer: &[u8], channels: usize, rate: u32, max_age_ms: u128) -> Option<Decoder> {
    let id = read_id(buffer)?;
    let mut table = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    let at = table.saved.iter().position(|s| s.id == id)?;
    let saved = table.saved.remove(at)?;
    drop(table);
    let fresh = saved.at.elapsed().as_millis() <= max_age_ms;
    let same = saved.decoder.channels() == channels && saved.rate == rate;
    COUNTERS.overlap(if fresh && same {
        Overlap::Restored
    } else {
        Overlap::Refused
    });
    (fresh && same).then_some(saved.decoder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_carries_a_decoder_from_a_buffer_to_its_copy() {
        let decoder = Decoder::new(2, 48_000);
        let mut buffer = vec![0u8; 64];
        // A cold buffer restores nothing.
        assert!(restore(&buffer, 2, 48_000).is_none());
        save(&mut buffer, &decoder, 48_000);
        // The title copies the buffer to another context's.
        let copy = buffer.clone();
        assert!(restore(&copy, 2, 48_000).is_some());
        // Used up: a buffer left in place and reused starts cold.
        assert!(restore(&copy, 2, 48_000).is_none());
        // Another stream's shape does not match (and uses the state up).
        save(&mut buffer, &decoder, 48_000);
        assert!(restore(&buffer, 1, 48_000).is_none());
        save(&mut buffer, &decoder, 48_000);
        assert!(restore(&buffer, 2, 44_100).is_none());
        // A damaged token does not either.
        save(&mut buffer, &decoder, 48_000);
        let mut damaged = buffer.clone();
        damaged[5] ^= 1;
        assert!(restore(&damaged, 2, 48_000).is_none());
        // Too long after it was kept, it is another sound's buffer.
        save(&mut buffer, &decoder, 48_000);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(restore_within(&buffer, 2, 48_000, 5).is_none());
        save(&mut buffer, &decoder, 48_000);
        assert!(restore_within(&buffer, 2, 48_000, 5_000).is_some());
        // Saving again names a new state and forgets the old.
        save(&mut buffer, &decoder, 48_000);
        let (first, copy) = (read_id(&buffer), buffer.clone());
        save(&mut buffer, &decoder, 48_000);
        assert_ne!(read_id(&buffer), first);
        assert!(restore(&copy, 2, 48_000).is_none());
        // Too small a buffer is left alone.
        let mut tiny = [0u8; 8];
        save(&mut tiny, &decoder, 48_000);
        assert_eq!(tiny, [0u8; 8]);
    }
}
