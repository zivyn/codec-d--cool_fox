//! Accessibility: speech synthesis for the title's narration. No voice is
//! installed here, so the list of voices is empty, and every text becomes a
//! valid WAVE stream of silence as long as the speech would take: the title
//! plays it and waits for it as it would for a voice, and hears nothing.
// shortcut: silent speech. The host's engine (SAPI through COM, or WinRT
// speech synthesis) needs COM bindings this crate has only in diagnostics
// builds, plus voice enumeration and SSML the title sends (slot 0xa8); the
// title then plays the silence and goes on.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

use super::xasync::{E_INVALIDARG, E_NOT_SUFFICIENT_BUFFER};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, text,
};

/// Closed captions, high contrast, speech to text and synthesis.
pub const PROVIDER: Guid = Guid::new(
    0x3e24_1977,
    0x6237,
    0x41e9,
    [0x85, 0x59, 0x10, 0x2c, 0x2d, 0x99, 0x83, 0xf1],
);
/// Unchanged since 2020.
pub const IID: Guid = Guid::new(
    0xd722_b373,
    0x8c4d,
    0x4692,
    [0x9e, 0x51, 0xd6, 0xad, 0x9b, 0x37, 0xaa, 0x7d],
);

pub static OBJECT: Object = Object::new("accessibility", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID, &OBJECT)];

/// The stream's format: 16 kHz mono 16-bit PCM.
const RATE: usize = 16_000;
const HEADER: usize = 44;
/// A voice speaks about fifteen characters a second; a stream is at least
/// a quarter of a second and at most half a minute long.
const MS_PER_CHAR: usize = 67;
const MIN_MS: usize = 250;
const MAX_MS: usize = 30_000;

/// Samples of silence for `words`: its characters, without SSML markup
/// (`<...>`) and without leading, trailing or repeated white space.
fn silence_samples(words: &str) -> usize {
    let mut spoken = 0;
    let (mut in_tag, mut space) = (false, true);
    for c in words.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            c if c.is_whitespace() => {
                if !space {
                    spoken += 1;
                }
                space = true;
            }
            _ => {
                spoken += 1;
                space = false;
            }
        }
    }
    // A trailing space was counted before the word that never came.
    let spoken = if space {
        spoken.saturating_sub(1)
    } else {
        spoken
    };
    let ms = (spoken * MS_PER_CHAR).clamp(MIN_MS, MAX_MS);
    ms * RATE / 1000
}

/// A WAVE file of `samples` of silence: RIFF size, a 16-byte format chunk
/// (PCM, 1 channel, 16,000 Hz, 32,000 bytes/s, block 2, 16 bits), the data
/// chunk.
fn wave(samples: usize) -> Vec<u8> {
    let data = samples * 2;
    let mut out = Vec::with_capacity(HEADER + data);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((HEADER - 8 + data) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0");
    out.extend_from_slice(&(RATE as u32).to_le_bytes());
    out.extend_from_slice(&(RATE as u32 * 2).to_le_bytes());
    out.extend_from_slice(b"\x02\0\x10\0data");
    out.extend_from_slice(&(data as u32).to_le_bytes());
    out.resize(HEADER + data, 0);
    out
}

/// Synthesizers hold nothing, so their handle is a fixed address and closing
/// it is free.
static SYNTHESIZER: u8 = 0;

/// The live streams: handle (a number, never an address) to its samples.
static STREAMS: LazyLock<Mutex<HashMap<usize, usize>>> = LazyLock::new(Mutex::default);
static NEXT_STREAM: AtomicUsize = AtomicUsize::new(0x1000);

fn samples_of(stream: *const u8) -> Option<usize> {
    let streams = STREAMS.lock().unwrap_or_else(|e| e.into_inner());
    streams.get(&(stream as usize)).copied()
}

type This = *const Object;

/// No voice is installed, so the callback never runs.
extern "system" fn enumerate_voices(
    _: This,
    _context: *mut c_void,
    _callback: *const c_void,
) -> HRESULT {
    S_OK
}
extern "system" fn create(_: This, out: *mut *const u8) -> HRESULT {
    put(out, &raw const SYNTHESIZER)
}
extern "system" fn close(_: This, _: *const u8) -> HRESULT {
    S_OK
}
extern "system" fn set_default_voice(_: This, _: *const u8) -> HRESULT {
    S_OK
}
extern "system" fn set_custom_voice(_: This, _: *const u8, _voice: *const c_char) -> HRESULT {
    S_OK
}
extern "system" fn create_stream(
    _: This,
    _: *const u8,
    words: *const c_char,
    out: *mut *const u8,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let words = text(words).unwrap_or_default();
    let samples = silence_samples(&words);
    tracing::debug!(text = words, samples, "speech (silent)");
    let handle = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
    let mut streams = STREAMS.lock().unwrap_or_else(|e| e.into_inner());
    streams.insert(handle, samples);
    drop(streams);
    put(out, handle as *const u8)
}
extern "system" fn close_stream(_: This, stream: *const u8) -> HRESULT {
    let mut streams = STREAMS.lock().unwrap_or_else(|e| e.into_inner());
    streams.remove(&(stream as usize));
    S_OK
}
extern "system" fn stream_size(_: This, stream: *const u8, out: *mut usize) -> HRESULT {
    match samples_of(stream) {
        Some(samples) => put(out, HEADER + samples * 2),
        None => E_INVALIDARG,
    }
}
extern "system" fn stream_data(
    _: This,
    stream: *const u8,
    size: usize,
    buffer: *mut u8,
    used: *mut usize,
) -> HRESULT {
    let Some(samples) = samples_of(stream) else {
        return E_INVALIDARG;
    };
    let wave = wave(samples);
    if buffer.is_null() || size < wave.len() {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    // SAFETY: the buffer holds `size` bytes, checked above.
    unsafe { buffer.copy_from_nonoverlapping(wave.as_ptr(), wave.len()) };
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(wave.len()) };
    }
    S_OK
}

static VTABLE: Methods<22> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    missing::<3> as *const (),      // 0x18 closed captions
    missing::<4> as *const (),      // 0x20
    missing::<5> as *const (),      // 0x28 high contrast
    missing::<6> as *const (),      // 0x30 speech to text
    missing::<7> as *const (),      // 0x38
    enumerate_voices as *const (),  // 0x40
    create as *const (),            // 0x48
    close as *const (),             // 0x50
    set_default_voice as *const (), // 0x58
    set_custom_voice as *const (),  // 0x60
    create_stream as *const (),     // 0x68 from text
    close_stream as *const (),      // 0x70 a stream
    stream_size as *const (),       // 0x78
    stream_data as *const (),       // 0x80
    missing::<17> as *const (),     // 0x88 speech to text
    missing::<18> as *const (),     // 0x90
    missing::<19> as *const (),     // 0x98
    missing::<20> as *const (),     // 0xa0
    create_stream as *const (),     // 0xa8 from SSML
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    type Create = extern "system" fn(This, *mut *const u8) -> HRESULT;
    type Stream = extern "system" fn(This, *const u8, *const c_char, *mut *const u8) -> HRESULT;
    type Size = extern "system" fn(This, *const u8, *mut usize) -> HRESULT;
    type Data = extern "system" fn(This, *const u8, usize, *mut u8, *mut usize) -> HRESULT;
    type Close = extern "system" fn(This, *const u8) -> HRESULT;

    /// Says `words` the way a title does (`slot` 0x68 text, 0xa8 SSML), and
    /// returns the WAVE file it gets back.
    fn say(slot: usize, words: &std::ffi::CStr) -> Vec<u8> {
        let this: This = &OBJECT;
        let (mut synth, mut stream, mut size) = (std::ptr::null(), std::ptr::null(), 0);
        assert_eq!(method::<Create>(&OBJECT, 0x48)(this, &mut synth), S_OK);
        let from = method::<Stream>(&OBJECT, slot);
        assert_eq!(from(this, synth, words.as_ptr(), &mut stream), S_OK);
        assert_eq!(method::<Size>(&OBJECT, 0x78)(this, stream, &mut size), S_OK);
        let data = method::<Data>(&OBJECT, 0x80);
        // A buffer a byte short is refused, not truncated.
        let mut wav = vec![0xAAu8; size];
        let short = data(
            this,
            stream,
            size - 1,
            wav.as_mut_ptr(),
            std::ptr::null_mut(),
        );
        assert_eq!(short, E_NOT_SUFFICIENT_BUFFER);
        let mut used = 0;
        assert_eq!(data(this, stream, size, wav.as_mut_ptr(), &mut used), S_OK);
        assert_eq!(used, size);
        assert_eq!(method::<Close>(&OBJECT, 0x70)(this, stream), S_OK);
        // A closed stream is gone.
        assert_eq!(
            method::<Size>(&OBJECT, 0x78)(this, stream, &mut size),
            E_INVALIDARG
        );
        assert_eq!(method::<Close>(&OBJECT, 0x50)(this, synth), S_OK);
        wav
    }

    fn le32(wav: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(wav[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn streams_are_silent_wave_files() {
        let wav = say(0x68, c"Hi");
        assert_eq!(
            (&wav[..4], &wav[8..16], &wav[36..40]),
            (&b"RIFF"[..], &b"WAVEfmt "[..], &b"data"[..])
        );
        // PCM, mono, 16,000 Hz, 32,000 bytes a second, block 2, 16 bits.
        assert_eq!(wav[20..24], [1, 0, 1, 0]);
        assert_eq!((le32(&wav, 24), le32(&wav, 28)), (16_000, 32_000));
        assert_eq!(wav[32..36], [2, 0, 16, 0]);
        // The sizes say what is there, and what is there is silence.
        assert_eq!(le32(&wav, 4) as usize, wav.len() - 8);
        assert_eq!(le32(&wav, 40) as usize, wav.len() - 44);
        assert!(wav[44..].iter().all(|&b| b == 0));
        // Two letters are less than the shortest stream: a quarter second.
        assert_eq!(wav.len() - 44, 8_000);
    }

    /// A voice takes longer for longer text, so does the silence; SSML tags
    /// and extra white space say nothing.
    #[test]
    fn silence_is_as_long_as_the_speech() {
        assert_eq!(silence_samples(""), MIN_MS * RATE / 1000);
        let sentence = "Turn left at the end of the road and keep going";
        let long = silence_samples(sentence);
        assert_eq!(long, sentence.len() * MS_PER_CHAR * RATE / 1000);
        assert!(long > silence_samples("Turn left"));
        let marked =
            format!("<speak version='1.0'><prosody rate='slow'>{sentence}</prosody></speak>");
        assert_eq!(silence_samples(&marked), long);
        assert_eq!(
            silence_samples("  Turn   left \n "),
            silence_samples("Turn left")
        );
        // Never more than half a minute.
        assert_eq!(
            silence_samples(&"word ".repeat(10_000)),
            MAX_MS * RATE / 1000
        );

        let ssml = say(
            0xa8,
            c"<speak>Turn left at the end of the road and keep going</speak>",
        );
        assert_eq!(ssml.len() - 44, long * 2);
    }

    #[test]
    fn no_voice_is_installed_and_bad_handles_are_refused() {
        let this: This = &OBJECT;
        type Voices = extern "system" fn(This, *mut c_void, *const c_void) -> HRESULT;
        let mut calls = 0u32;
        let voices = method::<Voices>(&OBJECT, 0x40);
        assert_eq!(
            voices(this, (&raw mut calls).cast(), std::ptr::null()),
            S_OK
        );
        assert_eq!(calls, 0, "no voice was reported");
        let mut size = 0;
        assert_eq!(
            method::<Size>(&OBJECT, 0x78)(this, std::ptr::null(), &mut size),
            E_INVALIDARG
        );
        let mut out = std::ptr::null();
        let from = method::<Stream>(&OBJECT, 0x68);
        assert_eq!(
            from(this, std::ptr::null(), c"x".as_ptr(), std::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(from(this, std::ptr::null(), c"x".as_ptr(), &mut out), S_OK);
        assert_eq!(method::<Close>(&OBJECT, 0x70)(this, out), S_OK);
    }
}
