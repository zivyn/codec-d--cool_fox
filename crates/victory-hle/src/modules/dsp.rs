//! The console's audio DSP (`chad.dll`): streams that convolve blocks of
//! float samples with an impulse response (reverb), or run other block
//! transforms, on a coprocessor.
//!
//! A title connects once with a memory block for the DSP, activates
//! streams, and submits one command per block (gains, an input and an
//! output buffer). Activation hands back a status block whose second word is
//! the ticket of the last completed command and whose fourth is its result;
//! the title checks it one block later and treats a late command as an
//! overrun. Commands complete when submitted here.
//!
//! Mono convolutions of 1024-frame blocks, the only kind the title has been
//! seen to use, are computed with `VICTORY_DSP_CONVOLVE=1` ([`convolve`]):
//! reverb, and the speakers in the world (TVs, phone calls), whose sound
//! goes through the DSP alone. Without it outputs are silence, so reverb
//! returns are dry, unless `VICTORY_DSP_REVERB=1` (or `interleaved`) runs
//! convolution streams through an algorithmic reverb of ours instead (see
//! [`reverb`]).
// shortcut: stereo and 512-frame convolutions and the other block
// transforms are not computed; their outputs are zeros (or the stand-in
// reverb's). `VICTORY_DSP_IR_DUMP=<dir>` saves each stream's response.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::Mutex;

use super::audio::live::{live_flag, live_value};

mod convolve;
mod reverb;

use super::xasync::E_INVALIDARG;
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleModule, hle_export};

/// What the computed convolutions are scaled by: `VICTORY_DSP_CONVOLVE_GAIN`,
/// else 2. The hardware's own scaling is not known. With the textbook 1/2048
/// of the inverse FFT, TVs and phone calls sounded quiet next to the rest of
/// the mix; twice that sounded right by ear.
const GAIN: f32 = 2.0;

fn gain() -> f32 {
    live_value!("VICTORY_DSP_CONVOLVE_GAIN", f32, |t| t
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(GAIN))
}

const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;

const CONVOLUTION: u32 = 1;
/// Option bits that need a stereo stream, and one convolution refuses.
const STEREO_OPTIONS: u32 = 2 | 8;
const NOT_FOR_CONVOLUTION: u32 = 4;

pub struct Dsp {
    max_streams: u32,
    active: Mutex<u32>,
}

/// What activation describes.
#[repr(C, packed)]
pub struct StreamDesc {
    kind: u32,
    options: u32,
    frames: u32,
    channels: u32,
    ir_floats: u32,
    ir: *const f32,
}

/// The status block a stream's owner polls.
#[repr(C)]
pub struct Status {
    submitted: u32,
    completed: u32,
    reserved: u32,
    result: HRESULT,
}

pub struct Stream {
    dsp: *mut Dsp,
    frames: u32,
    channels: u32,
    convolution: bool,
    status: Box<Status>,
    /// The stand-in reverb of a convolution stream (made at its first block
    /// with the switch on), whether its blocks are interleaved
    /// (`VICTORY_DSP_REVERB_LAYOUT=interleaved`), and whether it runs whatever
    /// the switch says (tests).
    reverb: Option<reverb::Reverb>,
    interleaved: bool,
    always: bool,
    /// Set for a mono stream of 1024-frame blocks whose response is in the
    /// layout [`convolve`] reads.
    convolver: Option<convolve::Convolver>,
}

/// One block of work.
#[repr(C)]
pub struct Command {
    gains: [f32; 2],
    input: *const f32,
    output: *mut f32,
    reserved: u64,
}

pub extern "system" fn XDspConnectWithMaximumStreamLimit(
    base: *mut c_void,
    base_length: u32,
    ir_seconds: u32,
    max_streams: u32,
    max_channels: u32,
    out: *mut *mut Dsp,
) -> HRESULT {
    if base.is_null()
        || !(0x1_0000..=0x1F40_0000).contains(&base_length)
        || ir_seconds > 0x80
        || max_streams > 0x100
        || max_channels > 2
    {
        return E_INVALIDARG;
    }
    if out.is_null() {
        return E_POINTER;
    }
    tracing::info!(
        base_length,
        ir_seconds,
        max_streams,
        max_channels,
        "audio DSP connected"
    );
    let dsp = Box::new(Dsp {
        max_streams,
        active: Mutex::new(0),
    });
    // SAFETY: checked non-null.
    unsafe { out.write(Box::into_raw(dsp)) };
    S_OK
}

pub extern "system" fn XDspDisconnect(dsp: *mut Dsp) -> HRESULT {
    if dsp.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: made by `XDspConnectWithMaximumStreamLimit`; disconnected
    // once, after its streams.
    drop(unsafe { Box::from_raw(dsp) });
    S_OK
}

fn valid(desc: &StreamDesc) -> bool {
    let (kind, options, frames, channels) = (desc.kind, desc.options, desc.frames, desc.channels);
    let shape = if kind == CONVOLUTION {
        (1..=2).contains(&channels)
            && matches!(frames, 0x200 | 0x400)
            && options & NOT_FOR_CONVOLUTION == 0
    } else {
        channels == 1 && matches!(frames, 0x200 | 0x400 | 0x800)
    };
    shape && (options & STEREO_OPTIONS == 0 || channels == 2)
}

pub extern "system" fn XDspActivate(
    dsp: *mut Dsp,
    desc: *const StreamDesc,
    status: *mut *const Status,
    stream: *mut *mut Stream,
) -> HRESULT {
    if status.is_null() || stream.is_null() {
        return E_POINTER;
    }
    if dsp.is_null() || desc.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes its description.
    let desc = unsafe { &*desc };
    if !valid(desc) {
        return E_INVALIDARG;
    }
    let (kind, options, frames, channels) = (desc.kind, desc.options, desc.frames, desc.channels);
    tracing::debug!(
        kind,
        options,
        frames,
        channels,
        "audio DSP stream activated"
    );
    note_stream(desc);
    let (ir, ir_floats) = (desc.ir, desc.ir_floats);
    let convolver = if kind == CONVOLUTION && channels == 1 && frames == 0x400 && !ir.is_null() {
        // SAFETY: the title's description says the impulse response holds this many floats.
        convolve::Convolver::new(unsafe { std::slice::from_raw_parts(ir, ir_floats as usize) })
    } else {
        None
    };
    {
        // SAFETY: a connection from `XDspConnectWithMaximumStreamLimit`.
        let dsp = unsafe { &*dsp };
        let mut active = dsp.active.lock().unwrap_or_else(|e| e.into_inner());
        if *active >= dsp.max_streams {
            return E_OUTOFMEMORY;
        }
        *active += 1;
    }
    let new = Box::new(Stream {
        dsp,
        frames: desc.frames,
        channels: desc.channels,
        convolution: kind == CONVOLUTION,
        status: Box::new(Status {
            submitted: 0,
            completed: 0,
            reserved: 0,
            result: S_OK,
        }),
        reverb: None,
        interleaved: std::env::var("VICTORY_DSP_REVERB_LAYOUT").is_ok_and(|v| v == "interleaved"),
        always: false,
        convolver,
    });
    // SAFETY: checked non-null.
    unsafe {
        status.write(&*new.status);
        stream.write(Box::into_raw(new));
    }
    S_OK
}

/// Logs each kind of stream the title activates once, and with
/// `VICTORY_DSP_IR_DUMP=<dir>` saves the impulse response of the first 16
/// (`<n>-<kind>-<frames>x<channels>.ir`, as the title gave it).
fn note_stream(desc: &StreamDesc) {
    static SEEN: Mutex<Vec<[u32; 5]>> = Mutex::new(Vec::new());
    let key = [
        desc.kind,
        desc.options,
        desc.frames,
        desc.channels,
        desc.ir_floats,
    ];
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.contains(&key) {
        return;
    }
    seen.push(key);
    let n = seen.len();
    drop(seen);
    let [kind, options, frames, channels, ir_floats] = key;
    tracing::info!(
        kind,
        options,
        frames,
        channels,
        ir_floats,
        "audio DSP: a new kind of stream"
    );
    if let Some(dir) = std::env::var_os("VICTORY_DSP_IR_DUMP")
        && n <= 16
        && !desc.ir.is_null()
        && ir_floats <= 0x100_0000
    {
        // SAFETY: the title's response of `ir_floats` floats.
        let ir =
            unsafe { std::slice::from_raw_parts(desc.ir.cast::<u8>(), ir_floats as usize * 4) };
        let dir = std::path::PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join(format!("{n:02}-{kind}-{frames}x{channels}.ir")),
            ir,
        );
    }
}

pub extern "system" fn XDspDeactivate(stream: *mut Stream) -> HRESULT {
    if stream.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: made by `XDspActivate`; deactivated once.
    let stream = unsafe { Box::from_raw(stream) };
    // SAFETY: the connection outlives its streams.
    let dsp = unsafe { &*stream.dsp };
    *dsp.active.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
    S_OK
}

/// Completes the command at once: its output block is silence.
pub extern "system" fn XDspSubmitCommand(
    stream: *mut Stream,
    command: *const Command,
    ticket: *mut u32,
) -> HRESULT {
    if stream.is_null() || command.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a stream from `XDspActivate`, the title's command.
    let (stream, command) = unsafe { (&mut *stream, &*command) };
    if !command.output.is_null() {
        let (frames, channels) = (stream.frames as usize, stream.channels as usize);
        let gains = command.gains;
        // The block is computed here, on the title's own thread: it checks
        // the status one block later and takes a late command for an
        // overrun, and the network costs a few microseconds a block.
        // With the switch on (and for a convolution stream; `VICTORY_AB` may
        // flip it between blocks) the block is the reverb's; off, silence,
        // and the reverb is emptied so that a tail does not come back later.
        let computed =
            stream.convolver.is_some() && (stream.always || live_flag!("VICTORY_DSP_CONVOLVE"));
        let on = stream.convolution && (stream.always || live_flag!("VICTORY_DSP_REVERB"));
        if computed && !command.input.is_null() {
            let samples = frames * channels;
            // SAFETY: the title's buffers hold one block of the stream each.
            let (input, output) = unsafe {
                (
                    std::slice::from_raw_parts(command.input, samples),
                    std::slice::from_raw_parts_mut(command.output, samples),
                )
            };
            if let Some(convolver) = stream.convolver.as_mut() {
                convolver.process(input, output, gains[0] * gain());
            }
        } else if on {
            let reverb = stream
                .reverb
                .get_or_insert_with(|| reverb::Reverb::new(reverb::Reverb::rt60_from_env(), 1.0));
            reverb.level = live_value!("VICTORY_DSP_REVERB_LEVEL", f32, |t| t
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(1.0));
            // SAFETY: the title's buffers hold one block of the stream each.
            unsafe {
                reverb.block(
                    command.input,
                    command.output,
                    frames,
                    channels,
                    stream.interleaved,
                    gains,
                );
            }
        } else {
            if let Some(reverb) = &mut stream.reverb {
                reverb.reset();
            }
            // SAFETY: the title's output buffer holds one block of the stream.
            unsafe { std::ptr::write_bytes(command.output, 0, frames * channels) };
        }
    }
    let status = &mut *stream.status;
    let next = status.submitted.wrapping_add(1);
    // SAFETY: the status block is read by title code on other threads.
    unsafe {
        std::ptr::write_volatile(&mut status.submitted, next);
        std::ptr::write_volatile(&mut status.result, S_OK);
        std::ptr::write_volatile(&mut status.completed, next);
    }
    if !ticket.is_null() {
        // SAFETY: optional out parameter.
        unsafe { ticket.write(next) };
    }
    S_OK
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "dsp",
        dlls: &["chad.dll"],
        exports: vec![
            hle_export!(XDspConnectWithMaximumStreamLimit),
            hle_export!(XDspDisconnect),
            hle_export!(XDspActivate),
            hle_export!(XDspDeactivate),
            hle_export!(XDspSubmitCommand),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convolution_stream_as_the_title_drives_it() {
        let mut base = vec![0u8; 0x10_0000];
        let mut dsp = std::ptr::null_mut();
        assert_eq!(
            XDspConnectWithMaximumStreamLimit(
                base.as_mut_ptr().cast(),
                0x10_0000,
                24,
                0x80,
                1,
                &mut dsp
            ),
            S_OK
        );
        let ir = vec![0f32; 0x800];
        // Stereo convolution with option 2, as the title asks for.
        let desc = StreamDesc {
            kind: CONVOLUTION,
            options: 2,
            frames: 0x400,
            channels: 2,
            ir_floats: 0x800,
            ir: ir.as_ptr(),
        };
        let (mut status, mut stream) = (std::ptr::null(), std::ptr::null_mut());
        assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut stream), S_OK);
        let mut output = vec![1f32; 0x800];
        let input = vec![0.5f32; 0x800];
        let command = Command {
            gains: [1.0, 1.0],
            input: input.as_ptr(),
            output: output.as_mut_ptr(),
            reserved: 0,
        };
        let mut ticket = 0;
        assert_eq!(XDspSubmitCommand(stream, &command, &mut ticket), S_OK);
        // SAFETY: the stream's status block.
        let (completed, result) = unsafe { ((*status).completed, (*status).result) };
        assert_eq!((completed, result), (ticket, S_OK));
        assert!(output.iter().all(|&s| s == 0.0));
        assert_eq!(XDspDeactivate(stream), S_OK);

        // Mono streams refuse stereo options; transforms are mono only.
        let bad = StreamDesc {
            channels: 1,
            ..desc
        };
        assert_eq!(
            XDspActivate(dsp, &bad, &mut status, &mut stream),
            E_INVALIDARG
        );
        let transform = StreamDesc {
            kind: 2,
            options: 0,
            channels: 1,
            ir_floats: 0,
            ir: std::ptr::null(),
            ..desc
        };
        assert_eq!(
            XDspActivate(dsp, &transform, &mut status, &mut stream),
            S_OK
        );
        assert_eq!(XDspDeactivate(stream), S_OK);
        assert_eq!(XDspDisconnect(dsp), S_OK);
    }

    /// With the stand-in reverb on, a stream's blocks carry a tail: an
    /// impulse in the first block returns sound in the blocks after it,
    /// less each time, and the status is completed as before.
    #[test]
    fn a_convolution_stream_with_the_stand_in_reverb_rings_and_decays() {
        let mut base = vec![0u8; 0x10_0000];
        let mut dsp = std::ptr::null_mut();
        assert_eq!(
            XDspConnectWithMaximumStreamLimit(
                base.as_mut_ptr().cast(),
                0x10_0000,
                24,
                0x80,
                1,
                &mut dsp
            ),
            S_OK
        );
        let desc = StreamDesc {
            kind: CONVOLUTION,
            options: 0,
            frames: 0x400,
            channels: 1,
            ir_floats: 0,
            ir: std::ptr::null(),
        };
        let (mut status, mut stream) = (std::ptr::null(), std::ptr::null_mut());
        assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut stream), S_OK);
        // SAFETY: just made; the test is its only user.
        unsafe { (*stream).always = true };
        let mut input = vec![0f32; 0x400];
        input[0] = 1.0;
        let mut energies = Vec::new();
        for block in 0..96 {
            let mut output = vec![9f32; 0x400];
            let command = Command {
                gains: [1.0, 1.0],
                input: if block == 0 {
                    input.as_ptr()
                } else {
                    std::ptr::null()
                },
                output: output.as_mut_ptr(),
                reserved: 0,
            };
            let mut ticket = 0;
            assert_eq!(XDspSubmitCommand(stream, &command, &mut ticket), S_OK);
            // SAFETY: the stream's status block.
            assert_eq!(unsafe { (*status).completed }, ticket);
            energies.push(output.iter().map(|v| v * v).sum::<f32>());
        }
        // Blocks of 21 ms: the tail has sound at 0.5 s and is much weaker at 2 s.
        assert!(energies[23] > 0.0);
        assert!(
            energies[93] < energies[23] * 1e-3,
            "{:?}",
            &energies[20..30]
        );
        assert_eq!(XDspDeactivate(stream), S_OK);
        assert_eq!(XDspDisconnect(dsp), S_OK);
    }
}

#[cfg(test)]
mod convolve_tests {
    use super::*;

    #[test]
    fn a_mono_convolution_is_computed() {
        let mut base = vec![0u8; 0x10_0000];
        let mut dsp = std::ptr::null_mut();
        assert_eq!(
            XDspConnectWithMaximumStreamLimit(
                base.as_mut_ptr().cast(),
                0x10_0000,
                24,
                0x80,
                1,
                &mut dsp
            ),
            S_OK
        );
        // a response of one sample, 0.5, two samples late
        let ir = convolve::layout(&[0.0, 0.0, 0.5]);
        let desc = StreamDesc {
            kind: CONVOLUTION,
            options: 0,
            frames: 0x400,
            channels: 1,
            ir_floats: ir.len() as u32,
            ir: ir.as_ptr(),
        };
        let (mut status, mut stream) = (std::ptr::null(), std::ptr::null_mut());
        assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut stream), S_OK);
        // SAFETY: just made; the test is its only user.
        unsafe { (*stream).always = true };
        let input: Vec<f32> = (0..0x400).map(|i| (i % 7) as f32 / 7.0).collect();
        let mut output = vec![0f32; 0x400];
        let command = Command {
            gains: [1.0, 1.0],
            input: input.as_ptr(),
            output: output.as_mut_ptr(),
            reserved: 0,
        };
        let mut ticket = 0;
        assert_eq!(XDspSubmitCommand(stream, &command, &mut ticket), S_OK);
        for n in 2..0x400 {
            let want = input[n - 2] * 0.5 * gain();
            assert!(
                (output[n] - want).abs() < 1e-4,
                "sample {n}: {} vs {want}",
                output[n]
            );
        }
        assert_eq!(XDspDeactivate(stream), S_OK);
        assert_eq!(XDspDisconnect(dsp), S_OK);
    }
}
