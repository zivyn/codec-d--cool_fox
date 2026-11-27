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

use std::collections::VecDeque;
use std::sync::Mutex;

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

struct Saved {
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
        id,
        rate,
        decoder: decoder.clone(),
    });
    drop(table);
    buffer[..4].copy_from_slice(&MAGIC.to_le_bytes());
    buffer[4..12].copy_from_slice(&id.to_le_bytes());
    buffer[12..16].copy_from_slice(&check(id).to_le_bytes());
}

/// The decoder a buffer's token names, when it matches the stream
/// (channels and sample rate). The state stays in the table: a title may
/// copy one buffer to several contexts.
pub fn restore(buffer: &[u8], channels: usize, rate: u32) -> Option<Decoder> {
    let id = read_id(buffer)?;
    let table = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    let saved = table.saved.iter().find(|s| s.id == id)?;
    (saved.decoder.channels() == channels && saved.rate == rate).then(|| saved.decoder.clone())
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
        // Another stream's shape does not match.
        assert!(restore(&copy, 1, 48_000).is_none());
        assert!(restore(&copy, 2, 44_100).is_none());
        // A damaged token does not either.
        let mut damaged = copy.clone();
        damaged[5] ^= 1;
        assert!(restore(&damaged, 2, 48_000).is_none());
        // Saving again names a new state and forgets the old.
        let first = read_id(&buffer);
        save(&mut buffer, &decoder, 48_000);
        assert_ne!(read_id(&buffer), first);
        assert!(restore(&copy, 2, 48_000).is_none());
        // Too small a buffer is left alone.
        let mut tiny = [0u8; 8];
        save(&mut tiny, &decoder, 48_000);
        assert_eq!(tiny, [0u8; 8]);
    }
}
