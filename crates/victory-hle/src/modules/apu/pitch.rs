//! The pitch of an Opus stream (`VICTORY_OPUS_PITCH=1`): the console's
//! decoder resamples what it decodes by a pitch the title sets per packet
//! (a start pitch at activation, then a target and a ramp). A windowed-sinc
//! resampler (Kaiser, 16 taps each side at pitch 1, wider below the Nyquist
//! rate when the pitch is above 1) that keeps its position and its last
//! input frames between packets, so that packet edges are as smooth as the
//! signal and nothing is added to them.
//!
//! Assumed, since nothing documents it: a pitch of `p` plays the sound `p`
//! times as fast (its frequency is multiplied by `p`); the ramp is the most
//! the pitch moves toward the target in one packet, and 0 means at once;
//! the frames asked for in a command (`frame_count`) bound the frames
//! given out. A pitch of 1 from the start costs nothing: the packet is
//! copied as before, until the first one that is not 1 (after which the
//! stream stays on the resampler, so that its delay does not come and go).

use std::sync::OnceLock;

/// Taps each side of the centre at pitch 1.
const HALF: usize = 16;
/// Table steps per input frame.
const RES: usize = 512;
const KAISER_BETA: f64 = 8.0;
pub const MIN_PITCH: f32 = 0.25;
pub const MAX_PITCH: f32 = 4.0;
/// Frames of input kept: enough for the widest kernel on both sides.
const HIST: usize = 2 * HALF * MAX_PITCH as usize + 4;

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..40 {
        term *= (x / 2.0 / k as f64).powi(2);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// The windowed sinc at `x` input frames from the centre, in steps of 1/`RES`.
fn table() -> &'static [f32] {
    static TABLE: OnceLock<Vec<f32>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let norm = bessel_i0(KAISER_BETA);
        (0..=HALF * RES + 1)
            .map(|i| {
                let x = i as f64 / RES as f64;
                let u = x / HALF as f64;
                if u >= 1.0 {
                    return 0.0;
                }
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                (sinc * bessel_i0(KAISER_BETA * (1.0 - u * u).sqrt()) / norm) as f32
            })
            .collect()
    })
}

fn sane(pitch: f32) -> f32 {
    if pitch.is_finite() && pitch > 0.0 {
        pitch.clamp(MIN_PITCH, MAX_PITCH)
    } else {
        1.0
    }
}

pub struct Pitcher {
    channels: usize,
    /// Where the pitch is (where the last packet ended).
    pub pitch: f32,
    engaged: bool,
    /// The next frame to give out, in input frames from the start of the
    /// next packet (negative: among the kept ones).
    pos: f64,
    /// The last `HIST` frames of input, interleaved.
    hist: Vec<f32>,
}

impl Pitcher {
    pub fn new(channels: usize, start_pitch: f32) -> Self {
        let channels = channels.clamp(1, 8);
        Pitcher {
            channels,
            pitch: sane(start_pitch),
            engaged: false,
            pos: 0.0,
            hist: vec![0.0; HIST * channels],
        }
    }

    /// A stream's reset: the signal starts over; the pitch stays.
    pub fn reset(&mut self) {
        self.hist.fill(0.0);
        self.pos = 0.0;
        self.engaged = false;
    }

    /// Where the pitch will be after a packet with this target and ramp.
    pub fn next_pitch(&self, target: f32, ramp: f32) -> f32 {
        let target = sane(target);
        if ramp > 0.0 {
            sane(self.pitch + (target - self.pitch).clamp(-ramp, ramp))
        } else {
            target
        }
    }

    /// Whether the packet has to go through [`Pitcher::process`]: the
    /// stream is on the resampler already, or this is its first move off 1.
    pub fn needed(&self, target: f32, ramp: f32) -> bool {
        self.engaged || self.pitch != 1.0 || self.next_pitch(target, ramp) != 1.0
    }

    /// A packet that goes through as it is (pitch 1): the stream's last
    /// frames are kept, so that the first packet to be resampled goes on
    /// from where the last one ended.
    pub fn pass(&mut self, input: &[f32]) {
        let ch = self.channels;
        let keep = HIST * ch;
        if input.len() >= keep {
            self.hist.copy_from_slice(&input[input.len() - keep..]);
        } else {
            self.hist.rotate_left(input.len());
            let at = keep - input.len();
            self.hist[at..].copy_from_slice(input);
        }
        self.pos = 0.0;
    }

    /// Resamples `input` (interleaved frames of the stream's channels): at
    /// most `limit` frames come out, from the pitch the stream is at to the
    /// one it ramps to over the packet.
    pub fn process(&mut self, input: &[f32], target: f32, ramp: f32, limit: usize) -> Vec<f32> {
        let ch = self.channels;
        let n = input.len() / ch;
        let (p0, p1) = (self.pitch, self.next_pitch(target, ramp));
        self.engaged = true;
        let mut work = Vec::with_capacity((HIST + n) * ch);
        work.extend_from_slice(&self.hist);
        work.extend_from_slice(&input[..n * ch]);
        let len = (HIST + n) as isize;
        let table = table();
        // How many frames the packet gives at its mean pitch: the ramp's length.
        let span = (n as f64 / f64::from((p0 + p1) / 2.0)).max(1.0);
        let mut out = Vec::with_capacity(((n as f32 / p0.min(p1)) as usize + 2).min(limit) * ch);
        let mut pos = self.pos;
        let mut acc = [0f32; 8];
        let mut i = 0usize;
        while out.len() / ch < limit {
            let p = p0 + (p1 - p0) * (i as f64 / span).min(1.0) as f32;
            // Above 1 the kernel is stretched, to keep what the new rate cannot hold out.
            let scale = (1.0 / p).min(1.0);
            let reach = (HALF as f32 / scale).ceil() as isize;
            let at = HIST as f64 + pos;
            let base = at.floor() as isize;
            if base + reach >= len {
                break;
            }
            if base - reach + 1 < 0 {
                // Past what is kept (only after a jump of pitch): move on to what is.
                pos += (reach - 1 - base) as f64;
                continue;
            }
            let first = base - reach + 1;
            let mut d = (first as f64 - at) as f32;
            let (mut weights, mut sum) = (0usize, 0f32);
            acc[..ch.min(8)].fill(0.0);
            for j in first..=base + reach {
                let x = (d * scale).abs() * RES as f32;
                let k = x as usize;
                if k < HALF * RES {
                    let frac = x - k as f32;
                    let w = table[k] + (table[k + 1] - table[k]) * frac;
                    sum += w;
                    weights += 1;
                    let frame = &work[j as usize * ch..(j as usize + 1) * ch];
                    for (a, &s) in acc.iter_mut().zip(frame) {
                        *a += w * s;
                    }
                }
                d += 1.0;
            }
            // Unity gain at DC whatever the phase.
            let norm = if weights > 0 && sum.abs() > 1e-6 {
                1.0 / sum
            } else {
                0.0
            };
            out.extend(acc[..ch].iter().map(|a| a * norm));
            pos += f64::from(p);
            i += 1;
        }
        // The next packet starts n frames on; the frames kept are the last ones.
        // At most as far back as the widest kernel can still be served from the kept frames.
        let furthest = (HIST - HALF * MAX_PITCH as usize - 2) as f64;
        self.pos = (pos - n as f64).max(-furthest);
        let keep = work.len() - HIST * ch;
        self.hist.copy_from_slice(&work[keep..]);
        self.pitch = p1;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn tone(hz: f32, from: usize, frames: usize, channels: usize) -> Vec<f32> {
        (from..from + frames)
            .flat_map(|n| {
                let v = 0.5 * (n as f32 * hz * std::f32::consts::TAU / RATE).sin();
                std::iter::repeat_n(v, channels)
            })
            .collect()
    }

    /// The tone's frequency from its positive-going zero crossings, with
    /// the crossing instants interpolated.
    fn frequency(mono: &[f32]) -> f32 {
        let mut crossings = Vec::new();
        for (n, w) in mono.windows(2).enumerate() {
            if w[0] < 0.0 && w[1] >= 0.0 {
                crossings.push(n as f32 + -w[0] / (w[1] - w[0]));
            }
        }
        let cycles = (crossings.len() - 1) as f32;
        cycles * RATE / (crossings[crossings.len() - 1] - crossings[0])
    }

    fn run(pitcher: &mut Pitcher, hz: f32, packets: usize, target: f32, ramp: f32) -> Vec<f32> {
        let frames = 960;
        let mut out = Vec::new();
        for b in 0..packets {
            let input = tone(hz, b * frames, frames, 1);
            out.extend(pitcher.process(&input, target, ramp, 10_000));
        }
        out
    }

    /// A tone through the resampler comes out at its frequency times the
    /// pitch, above and below 1.
    #[test]
    fn the_frequency_follows_the_pitch() {
        for (pitch, hz) in [
            (1.5f32, 1000.0f32),
            (0.75, 1000.0),
            (2.0, 1500.0),
            (0.5, 3000.0),
        ] {
            let mut p = Pitcher::new(1, pitch);
            let out = run(&mut p, hz, 20, pitch, 0.0);
            let got = frequency(&out[2000..]);
            let want = hz * pitch;
            assert!(
                (got - want).abs() < want * 0.002,
                "pitch {pitch}: {got} Hz, wanted {want}"
            );
        }
    }

    /// Packets of 960 frames: the output has no jump at the packets' edges
    /// beyond those inside them, at a fixed pitch and while it ramps. The
    /// frames given per packet follow the pitch.
    #[test]
    fn packet_edges_are_as_smooth_as_the_signal() {
        for (start, target, ramp) in [
            (1.5f32, 1.5f32, 0.0f32),
            (0.8, 0.8, 0.0),
            (1.0, 1.6, 0.05),
            (1.6, 0.7, 0.1),
        ] {
            let mut p = Pitcher::new(1, start);
            let frames = 960;
            let (mut out, mut edges): (Vec<f32>, Vec<usize>) = (Vec::new(), Vec::new());
            for b in 0..40 {
                let input = tone(700.0, b * frames, frames, 1);
                let got = p.process(&input, target, ramp, 10_000);
                out.extend(&got);
                edges.push(out.len());
            }
            let (mut inside, mut edge) = (0f32, 0f32);
            for (n, w) in out.windows(2).enumerate().skip(2000) {
                let jump = (w[1] - w[0]).abs();
                if edges.contains(&(n + 1)) {
                    edge = edge.max(jump);
                } else {
                    inside = inside.max(jump);
                }
            }
            assert!(
                inside > 0.0 && edge <= inside * 1.1,
                "{start}->{target}: edge {edge}, inside {inside}"
            );
            // About frames / pitch per packet once settled (the last packet's pitch).
            let last = p.pitch;
            let per_packet = (out.len() - edges[edges.len() - 2]) as f32;
            assert!(
                (per_packet - 960.0 / last).abs() < 3.0 + 960.0 * 0.05,
                "{start}->{target}: {per_packet} frames"
            );
        }
    }

    /// Above 1 the sound that the new rate cannot hold is left out, not
    /// folded back: a tone at 16 kHz at pitch 2 (32 kHz, past the Nyquist
    /// rate of 24 kHz) comes out far below the level of one that fits.
    #[test]
    fn nothing_folds_back_above_the_nyquist_rate() {
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        let mut fits = Pitcher::new(1, 2.0);
        let ok = run(&mut fits, 8_000.0, 20, 2.0, 0.0);
        let mut over = Pitcher::new(1, 2.0);
        let bad = run(&mut over, 16_000.0, 20, 2.0, 0.0);
        let (ok, bad) = (rms(&ok[2000..]), rms(&bad[2000..]));
        assert!(ok > 0.3, "{ok}");
        assert!(bad < ok * 0.02, "{bad} against {ok}");
    }

    /// Channels stay apart and carry the same filter: a stereo input with a
    /// tone on the left and silence on the right.
    #[test]
    fn channels_are_independent() {
        let mut p = Pitcher::new(2, 1.25);
        let frames = 960;
        let mut left = Vec::new();
        for b in 0..10 {
            let input: Vec<f32> = tone(500.0, b * frames, frames, 1)
                .into_iter()
                .flat_map(|v| [v, 0.0])
                .collect();
            let out = p.process(&input, 1.25, 0.0, 10_000);
            assert!(out.chunks(2).all(|f| f[1] == 0.0));
            left.extend(out.chunks(2).map(|f| f[0]));
        }
        let got = frequency(&left[1000..]);
        assert!((got - 625.0).abs() < 2.0, "{got}");
    }

    /// The output is bounded by `limit`, and a stream that never leaves
    /// pitch 1 does not need the resampler.
    #[test]
    fn the_frames_given_are_bounded_and_pitch_one_is_left_alone() {
        let mut p = Pitcher::new(1, 1.0);
        assert!(!p.needed(1.0, 0.0));
        assert!(!p.needed(1.0, 0.5));
        assert!(p.needed(1.2, 0.0));
        let out = p.process(&tone(500.0, 0, 960, 1), 1.2, 0.0, 100);
        assert_eq!(out.len(), 100);
        // On the resampler it stays, even back at 1.
        assert!(p.needed(1.0, 0.0));
        // A ramp moves the pitch by at most its size a packet.
        let mut q = Pitcher::new(1, 1.0);
        q.process(&tone(500.0, 0, 960, 1), 2.0, 0.25, 10_000);
        assert!((q.pitch - 1.25).abs() < 1e-6);
        // Nonsense is pitch 1.
        assert_eq!(Pitcher::new(1, f32::NAN).pitch, 1.0);
        assert_eq!(Pitcher::new(1, 0.0).pitch, 1.0);
    }

    /// Cost: a second of stereo audio at pitch 1.3 within a tenth of the
    /// budget of 1 ms per 10 ms (a test build allows ten times that).
    #[test]
    fn a_second_of_audio_is_cheap() {
        let mut p = Pitcher::new(2, 1.3);
        let start = std::time::Instant::now();
        for b in 0..50 {
            p.process(&tone(500.0, b * 960, 960, 2), 1.3, 0.0, 10_000);
        }
        let took = start.elapsed();
        eprintln!("opus pitch: {took:?} per second of stereo audio");
        let budget = if cfg!(debug_assertions) { 1000 } else { 100 };
        assert!(took.as_millis() < budget, "{took:?}");
    }

    /// A stream that goes through unchanged at pitch 1 and then moves off it
    /// has no seam where it does: the resampled packets go on from the
    /// last frame that came out as it was.
    #[test]
    fn leaving_pitch_one_leaves_no_seam() {
        let (frames, mut p) = (960, Pitcher::new(1, 1.0));
        let mut out: Vec<f32> = Vec::new();
        let mut edges = Vec::new();
        for b in 0..30 {
            let input = tone(700.0, b * frames, frames, 1);
            if p.needed(1.4, 0.2) && b >= 10 {
                out.extend(p.process(&input, 1.4, 0.2, 10_000));
            } else {
                p.pass(&input);
                out.extend(&input);
            }
            edges.push(out.len());
        }
        let (mut inside, mut edge) = (0f32, 0f32);
        for (n, w) in out.windows(2).enumerate().skip(100) {
            let jump = (w[1] - w[0]).abs();
            if edges.contains(&(n + 1)) {
                edge = edge.max(jump);
            } else {
                inside = inside.max(jump);
            }
        }
        assert!(edge <= inside * 1.1, "edge {edge}, inside {inside}");
    }
}
