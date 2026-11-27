//! A feedback delay network standing in for the DSP's convolution reverb
//! (`VICTORY_DSP_REVERB`): eight delay lines mixed by a Hadamard matrix,
//! each loop damped like air, two allpass diffusers in front. The title's
//! impulse response is in the hardware's own layout and is not read, so the
//! tail's length comes from `VICTORY_DSP_REVERB_RT60` (seconds, 2.0) and its
//! level from `VICTORY_DSP_REVERB_LEVEL` (1.0): a stand-in for dry returns,
//! not the title's room.
//!
//! About 25 flops a sample per channel: well under 1 ms per 10 ms of audio.

const LINES: usize = 8;
/// Mutually prime delays at 48 kHz (21 to 64 ms).
const DELAYS: [usize; LINES] = [1031, 1327, 1523, 1789, 2053, 2381, 2749, 3067];
const DIFFUSERS: [usize; 2] = [142, 107];
const SAMPLE_RATE: f32 = 48_000.0;
/// One-pole low pass in each loop: the high frequencies die first.
const DAMPING: f32 = 0.35;
/// White noise in comes out at about 0.4 of its level (calibrated by
/// simulation, checked by the test).
const INJECTION: f32 = 0.35;
const ALLPASS_GAIN: f32 = 0.5;
/// An output this large (or not a number) means the network ran away.
const RUNAWAY: f32 = 1.0e6;
/// Below this a float is flushed to zero, out of the denormals.
const FLOOR: f32 = 1e-18;

struct Allpass {
    buffer: Vec<f32>,
    at: usize,
}

impl Allpass {
    fn new(len: usize) -> Self {
        Allpass {
            buffer: vec![0.0; len],
            at: 0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let delayed = self.buffer[self.at];
        let v = x - ALLPASS_GAIN * delayed;
        self.buffer[self.at] = v;
        self.at += 1;
        if self.at == self.buffer.len() {
            self.at = 0;
        }
        delayed + ALLPASS_GAIN * v
    }
}

pub struct Reverb {
    lines: [Vec<f32>; LINES],
    at: [usize; LINES],
    damped: [f32; LINES],
    gain: [f32; LINES],
    diffusers: [Allpass; 2],
    level: f32,
}

impl Reverb {
    pub fn new(rt60: f32, level: f32) -> Self {
        let rt60 = rt60.clamp(0.1, 20.0);
        Reverb {
            lines: DELAYS.map(|d| vec![0.0; d]),
            at: [0; LINES],
            damped: [0.0; LINES],
            // -60 dB after `rt60` seconds for the whole loop.
            gain: DELAYS.map(|d| 10f32.powf(-3.0 * d as f32 / (rt60 * SAMPLE_RATE))),
            diffusers: DIFFUSERS.map(Allpass::new),
            level,
        }
    }

    /// Forgets everything the network holds.
    fn reset(&mut self) {
        self.lines.iter_mut().for_each(|l| l.fill(0.0));
        self.damped = [0.0; LINES];
        for d in &mut self.diffusers {
            d.buffer.fill(0.0);
        }
    }

    /// From the environment: `None` when `VICTORY_DSP_REVERB` is not set
    /// (or is 0); the second is whether blocks are interleaved
    /// (`VICTORY_DSP_REVERB=interleaved`) rather than one channel after the
    /// other.
    pub fn from_env() -> Option<(Self, bool)> {
        let switch = std::env::var("VICTORY_DSP_REVERB").ok()?;
        if switch.is_empty() || switch == "0" {
            return None;
        }
        let number = |name: &str, default: f32| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(default)
        };
        let reverb = Reverb::new(
            number("VICTORY_DSP_REVERB_RT60", 2.0),
            number("VICTORY_DSP_REVERB_LEVEL", 1.0),
        );
        Some((reverb, switch == "interleaved"))
    }

    /// One sample in; the two outputs (decorrelated: the output signs
    /// differ).
    #[inline]
    fn step(&mut self, input: f32) -> (f32, f32) {
        let mut x = input * INJECTION;
        for d in &mut self.diffusers {
            x = d.process(x);
        }
        let mut taps = [0f32; LINES];
        let (mut left, mut right) = (0.0, 0.0);
        for i in 0..LINES {
            let d = self.lines[i][self.at[i]];
            left += if i % 2 == 0 { d } else { -d };
            right += if i / 2 % 2 == 0 { d } else { -d };
            let damped = &mut self.damped[i];
            *damped += DAMPING * (d - *damped);
            if damped.abs() < FLOOR {
                *damped = 0.0;
            }
            taps[i] = *damped * self.gain[i];
        }
        // Fast Walsh-Hadamard transform, scaled to keep the energy.
        let mut h = 1;
        while h < LINES {
            for block in (0..LINES).step_by(h * 2) {
                for j in block..block + h {
                    let (a, b) = (taps[j], taps[j + h]);
                    taps[j] = a + b;
                    taps[j + h] = a - b;
                }
            }
            h *= 2;
        }
        let norm = 1.0 / (LINES as f32).sqrt();
        for i in 0..LINES {
            self.lines[i][self.at[i]] = taps[i] * norm + x;
            self.at[i] += 1;
            if self.at[i] == DELAYS[i] {
                self.at[i] = 0;
            }
        }
        (left * norm, right * norm)
    }

    /// Fills `output` (one block of `frames` samples per channel,
    /// `channels` of them, in the layout asked for) with the reverb of
    /// `input` (silence when null): the mean of the channels in, a channel
    /// of the network's output per channel out, scaled by the command's
    /// `gains`.
    ///
    /// # Safety
    /// `output` holds `frames * channels` floats; `input`, if not null,
    /// holds as many.
    pub unsafe fn block(
        &mut self,
        input: *const f32,
        output: *mut f32,
        frames: usize,
        channels: usize,
        interleaved: bool,
        gains: [f32; 2],
    ) {
        let index = |n: usize, c: usize| {
            if interleaved {
                n * channels + c
            } else {
                c * frames + n
            }
        };
        // A sample that is not a number would stay in the network for good.
        let (mut input_peak, mut output_peak, mut repaired) = (0f32, 0f32, false);
        for n in 0..frames {
            let mut sum = 0.0;
            if !input.is_null() {
                for c in 0..channels {
                    // SAFETY: inside the block, per the contract.
                    let v = unsafe { input.add(index(n, c)).read() };
                    if v.is_finite() {
                        input_peak = input_peak.max(v.abs());
                        sum += v;
                    } else {
                        repaired = true;
                    }
                }
            }
            let (mut left, mut right) = self.step(sum / channels as f32);
            // Passive: the loop gain is below one. Larger is a fault.
            if !(left.abs() <= RUNAWAY && right.abs() <= RUNAWAY) {
                self.reset();
                (left, right) = (0.0, 0.0);
                repaired = true;
            }
            output_peak = output_peak.max(left.abs()).max(right.abs());
            // SAFETY: as above; every input of this frame is read already.
            unsafe {
                output.add(index(n, 0)).write(left * gains[0] * self.level);
                if channels > 1 {
                    output.add(index(n, 1)).write(right * gains[1] * self.level);
                }
            }
        }
        crate::modules::audio::stats::COUNTERS.reverb_block(input_peak, output_peak, repaired);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(reverb: &mut Reverb, input: &[f32]) -> Vec<f32> {
        let mut out = vec![0f32; input.len()];
        // SAFETY: both hold `input.len()` floats, one channel.
        unsafe {
            reverb.block(
                input.as_ptr(),
                out.as_mut_ptr(),
                input.len(),
                1,
                false,
                [1.0, 1.0],
            );
        }
        out
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// The tail of an impulse decays: each half second holds less energy
    /// than the one before, far below -60 dB at twice the reverb time, and
    /// the network never grows.
    #[test]
    fn the_tail_decays() {
        let mut reverb = Reverb::new(1.5, 1.0);
        let mut impulse = vec![0f32; 48_000 * 4];
        impulse[0] = 1.0;
        let out = run(&mut reverb, &impulse);
        let energy: Vec<f32> = out
            .chunks(24_000)
            .map(|c| c.iter().map(|v| v * v).sum())
            .collect();
        assert!(energy[0] > 0.0);
        for pair in energy.windows(2).skip(1) {
            assert!(pair[1] < pair[0], "{energy:?}");
        }
        assert!(energy[6] < energy[0] * 1e-6, "{energy:?}");
        assert!(out.iter().all(|v| v.is_finite() && v.abs() < 2.0));
    }

    /// Noise in comes out as a return of about 0.4 of its level, and the
    /// network is silent when its input is null.
    #[test]
    fn noise_returns_at_the_calibrated_level() {
        let mut reverb = Reverb::new(2.0, 1.0);
        let mut seed = 0x2545_f491_u32;
        let noise: Vec<f32> = (0..48_000 * 4)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1 << 23) as f32 - 1.0
            })
            .collect();
        let out = run(&mut reverb, &noise);
        let ratio = rms(&out[96_000..]) / rms(&noise[96_000..]);
        assert!((0.25..0.6).contains(&ratio), "{ratio}");

        let mut quiet = Reverb::new(2.0, 1.0);
        let mut out = vec![1f32; 1024];
        // SAFETY: 1024 floats, one channel; no input.
        unsafe {
            quiet.block(
                std::ptr::null(),
                out.as_mut_ptr(),
                1024,
                1,
                false,
                [1.0, 1.0],
            );
        }
        assert!(out.iter().all(|&v| v == 0.0));
    }

    /// Stereo: the two channels differ, the gains scale them, and the two
    /// layouts agree on the same samples.
    #[test]
    fn stereo_channels_are_decorrelated_and_gained() {
        let frames = 4096;
        let mono: Vec<f32> = (0..frames).map(|n| (n as f32 * 0.05).sin()).collect();
        let planar: Vec<f32> = mono.iter().chain(&mono).copied().collect();
        let inter: Vec<f32> = mono.iter().flat_map(|&v| [v, v]).collect();
        let (mut a, mut b) = (Reverb::new(2.0, 1.0), Reverb::new(2.0, 1.0));
        let (mut pa, mut pb) = (vec![0f32; 2 * frames], vec![0f32; 2 * frames]);
        // SAFETY: two channels of `frames` floats each.
        unsafe {
            a.block(
                planar.as_ptr(),
                pa.as_mut_ptr(),
                frames,
                2,
                false,
                [1.0, 0.5],
            );
            b.block(inter.as_ptr(), pb.as_mut_ptr(), frames, 2, true, [1.0, 0.5]);
        }
        let (left, right) = pa.split_at(frames);
        assert_ne!(left, right);
        assert!(rms(left) > 0.0 && rms(right) > 0.0);
        for n in 0..frames {
            assert!((pa[n] - pb[2 * n]).abs() < 1e-6);
            assert!((pa[frames + n] - pb[2 * n + 1]).abs() < 1e-6);
        }
    }

    /// Cost: a second of stereo blocks of 1024 frames takes far less than
    /// the budget of 1 ms per 10 ms of audio (100 ms a second), even in a
    /// test build.
    #[test]
    fn a_second_of_audio_is_cheap() {
        let mut reverb = Reverb::new(2.0, 1.0);
        let input = vec![0.1f32; 2048];
        let mut output = vec![0f32; 2048];
        let start = std::time::Instant::now();
        for _ in 0..47 {
            // SAFETY: 2 x 1024 floats.
            unsafe {
                reverb.block(
                    input.as_ptr(),
                    output.as_mut_ptr(),
                    1024,
                    2,
                    false,
                    [1.0, 1.0],
                );
            }
        }
        let took = start.elapsed();
        eprintln!("reverb: {took:?} per second of stereo audio");
        let budget = if cfg!(debug_assertions) { 1000 } else { 100 };
        assert!(took.as_millis() < budget, "{took:?}");
    }

    /// A sample that is not a number in the input does not stay in the
    /// network: the output is finite after it, and the block counts as
    /// repaired.
    #[test]
    fn a_sample_that_is_not_a_number_is_not_kept() {
        let mut reverb = Reverb::new(2.0, 1.0);
        let mut input = vec![0.1f32; 4096];
        input[100] = f32::NAN;
        input[200] = f32::INFINITY;
        let out = run(&mut reverb, &input);
        assert!(out.iter().all(|v| v.is_finite()));
        assert!(rms(&out[2048..]) > 0.0);
        // Gone from the network too: silence in, silence out afterwards.
        let quiet = run(&mut reverb, &vec![0f32; 48_000 * 6]);
        assert!(quiet.iter().all(|v| v.is_finite()));
        assert!(rms(&quiet[48_000 * 5..]) < 1e-6);
    }
}
