//! The DSP's convolution, computed on the CPU.
//!
//! The title hands over each impulse response already in the coprocessor's
//! layout, read by trustno1-coder/VIctory-NVIDIA#77 (Yve) from the responses
//! the retail title passes to `XDspActivate`: blocks of 2048 floats, each the
//! unscaled 2048-point FFT of a 2048-sample window of the response, window k
//! covering samples (k-1)*1024 .. (k+1)*1024 (zeros before 0). The bins are
//! interleaved re/im, 0 to 1023 in natural order; bin 0's imaginary slot is 0
//! (the Nyquist bin is not kept). A last block that starts with the word
//! 0xFFFFACE1 holds the hardware's schedule and is not response data.
//!
//! An input block zero-padded to 2048 samples and multiplied with window k
//! gives, in the second half of the inverse FFT, that block's part of the
//! output k blocks later: each output block is the second half of
//! IFFT(sum over k of X[t-k] * H[k]) / 2048. How the hardware scales its
//! result is not known; the caller's gain makes up for it.
//!
//! What this version does differently from the first one ([`reference`],
//! the test oracle and the cost to beat):
//! - The 2048-point transforms of real signals run as 1024-point complex ones
//!   (half the work), on tables made once, with the stages written so that the
//!   compiler vectorizes them; with AVX2 on the machine the same code is
//!   compiled for it and picked at run time (`VICTORY_DSP_CONVOLVE_SIMD=0`:
//!   not).
//! - The sum over the response's windows is blocked by 32 bins: the
//!   accumulators stay in registers while the windows stream by in memory
//!   order (spectra are stored bin chunk first, window second), and a window
//!   whose input block was silent is skipped.
//! - Streams that play the same response share its spectra (one copy per
//!   distinct response, found by a hash of its floats); a stream's ring of
//!   input spectra, as large as the response, exists only while its input
//!   has not been silent for a whole response.
//! - A tail whose energy is below 1e-9 of the response's (-90 dB) is dropped.
//! - A response that does not look like the layout above (the windows do
//!   not agree with their neighbours in the time domain) is refused, with
//!   a line in the log, so a wrong guess gives silence and not noise.

use std::sync::{Arc, Mutex, OnceLock, Weak};

#[cfg(test)]
pub(super) mod reference;

/// The window, in samples, and the block, in samples and in bins kept.
const N: usize = 2048;
const B: usize = 1024;
/// The complex transform that does a real window: half as long.
const M: usize = N / 2;
/// Bins in the unit of the sum over windows, and floats a window of one
/// chunk takes (re then im).
const CH: usize = 32;
const CH2: usize = 2 * CH;
const CHUNKS: usize = B / CH;
const TRAILER: u32 = 0xFFFF_ACE1;
/// The tail of a response whose energy adds up to this much of the whole is
/// dropped.
const TAIL_FLOOR: f64 = 1e-9;
/// A window and its neighbour must agree to this much (relative to the
/// larger of the two) over the samples they share to count as the layout
/// above; a wrong guess is off by about 1.
const AGREEMENT: f32 = 0.25;

/// Tables, made once.
struct Plan {
    /// Bit reversal of a 1024-point index.
    rev: Vec<u16>,
    /// Twiddles of the stage that combines runs of `h` (a power of two):
    /// `exp(-2 pi i k / 2h)` for k < h at offset h - 1.
    tw_re: Vec<f32>,
    tw_im: Vec<f32>,
    /// cos and sin of 2 pi k / 2048, k < 1024: the step between the complex
    /// transform and the real one.
    cos: Vec<f32>,
    sin: Vec<f32>,
}

fn plan() -> &'static Plan {
    static PLAN: OnceLock<Plan> = OnceLock::new();
    PLAN.get_or_init(|| {
        let bits = M.trailing_zeros();
        let rev = (0..M)
            .map(|i| (i.reverse_bits() >> (usize::BITS - bits)) as u16)
            .collect();
        let (mut tw_re, mut tw_im) = (Vec::with_capacity(M), Vec::with_capacity(M));
        let mut h = 1;
        while h < M {
            for k in 0..h {
                let a = -std::f64::consts::PI * k as f64 / h as f64;
                tw_re.push(a.cos() as f32);
                tw_im.push(a.sin() as f32);
            }
            h *= 2;
        }
        let (cos, sin) = (0..M)
            .map(|k| {
                let a = 2.0 * std::f64::consts::PI * k as f64 / N as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .unzip();
        Plan {
            rev,
            tw_re,
            tw_im,
            cos,
            sin,
        }
    })
}

/// A complex FFT of 1024 points, in place, forward, unscaled, of a signal
/// that is already in bit-reversed order. The inverse is the same call with
/// the two arrays swapped.
#[inline(always)]
fn fft(plan: &Plan, re: &mut [f32; M], im: &mut [f32; M]) {
    for (r, i) in re.chunks_exact_mut(2).zip(im.chunks_exact_mut(2)) {
        let (ar, br, ai, bi) = (r[0], r[1], i[0], i[1]);
        r[0] = ar + br;
        r[1] = ar - br;
        i[0] = ai + bi;
        i[1] = ai - bi;
    }
    // Runs of 2: the twiddles are 1 and -i.
    for (r, i) in re.chunks_exact_mut(4).zip(im.chunks_exact_mut(4)) {
        let (a0r, a0i, a1r, a1i) = (r[0], i[0], r[1], i[1]);
        let (t0r, t0i) = (r[2], i[2]);
        let (t1r, t1i) = (i[3], -r[3]);
        r[0] = a0r + t0r;
        i[0] = a0i + t0i;
        r[2] = a0r - t0r;
        i[2] = a0i - t0i;
        r[1] = a1r + t1r;
        i[1] = a1i + t1i;
        r[3] = a1r - t1r;
        i[3] = a1i - t1i;
    }
    let mut h = 4;
    while h < M {
        let (wr, wi) = (&plan.tw_re[h - 1..2 * h - 1], &plan.tw_im[h - 1..2 * h - 1]);
        for (rc, ic) in re.chunks_exact_mut(2 * h).zip(im.chunks_exact_mut(2 * h)) {
            let (ra, rb) = rc.split_at_mut(h);
            let (ia, ib) = ic.split_at_mut(h);
            let (ra, rb, ia, ib) = (&mut ra[..h], &mut rb[..h], &mut ia[..h], &mut ib[..h]);
            for k in 0..h {
                let tr = rb[k] * wr[k] - ib[k] * wi[k];
                let ti = rb[k] * wi[k] + ib[k] * wr[k];
                rb[k] = ra[k] - tr;
                ib[k] = ia[k] - ti;
                ra[k] += tr;
                ia[k] += ti;
            }
        }
        h *= 2;
    }
}

/// Bins 0 to 1023 of the 2048-point transform of `block` (1024 samples,
/// then zeros), into `xr` and `xi`. `re` and `im` are scratch.
#[inline(always)]
fn spectrum(
    plan: &Plan,
    block: &[f32; B],
    re: &mut [f32; M],
    im: &mut [f32; M],
    xr: &mut [f32; B],
    xi: &mut [f32; B],
) {
    // z[n] = x[2n] + i x[2n+1]: the second half of the window is zeros.
    re.fill(0.0);
    im.fill(0.0);
    for n in 0..B / 2 {
        let r = plan.rev[n] as usize;
        re[r] = block[2 * n];
        im[r] = block[2 * n + 1];
    }
    fft(plan, re, im);
    // Even and odd samples apart, then joined with the twiddle of the bin.
    for k in 0..M {
        let j = (M - k) & (M - 1);
        let (zr, zi, qr, qi) = (re[k], im[k], re[j], im[j]);
        let (er, ei) = ((zr + qr) * 0.5, (zi - qi) * 0.5);
        let (or, oi) = ((zi + qi) * 0.5, (qr - zr) * 0.5);
        let (c, s) = (plan.cos[k], plan.sin[k]);
        xr[k] = er + c * or + s * oi;
        xi[k] = ei + c * oi - s * or;
    }
}

/// The inverse of the real transform of 2048 points of the spectrum
/// `yr`, `yi` (bins 0 to 1023; bin 1024 is zero; the rest mirrors those),
/// unscaled: left in `re` and `im`, so that sample 2m is `im[m]` and
/// sample 2m+1 is `re[m]`.
#[inline(always)]
fn unspectrum(plan: &Plan, yr: &[f32; B], yi: &[f32; B], re: &mut [f32; M], im: &mut [f32; M]) {
    // Bin 0 and its mirror (bin 1024, zero) are real: z[0] = (y0, y0).
    // The arrays go in swapped, which makes the forward transform an
    // inverse one.
    re[0] = yr[0];
    im[0] = yr[0];
    for k in 1..M {
        let (a, b) = (yr[k], yi[k]);
        let (cr, ci) = (yr[M - k], yi[M - k]);
        let (ar, ai) = (a + cr, b - ci);
        let (dr, di) = (a - cr, b + ci);
        let (c, s) = (plan.cos[k], plan.sin[k]);
        let (br, bi) = (dr * c - di * s, dr * s + di * c);
        let r = plan.rev[k] as usize;
        re[r] = ai + br;
        im[r] = ar - bi;
    }
    fft(plan, re, im);
}

/// What a response is: its windows' spectra, as the sum wants them.
pub(super) struct Spectra {
    parts: usize,
    /// Per chunk of 32 bins, per window, 32 re then 32 im:
    /// `h[(chunk * parts + window) * 64 + ..]`.
    h: Vec<f32>,
    /// Windows dropped from the end as inaudible.
    pruned: usize,
}

impl Spectra {
    /// Why not, if the response is not in the layout above.
    fn new(ir: &[f32]) -> Result<Self, &'static str> {
        if ir.is_empty() || ir.len() % N != 0 {
            return Err("not a whole number of windows");
        }
        let mut blocks = ir.len() / N;
        if ir[(blocks - 1) * N].to_bits() == TRAILER {
            blocks -= 1;
        }
        // trailing windows of zeros add nothing
        while blocks > 0 && ir[(blocks - 1) * N..blocks * N].iter().all(|&v| v == 0.0) {
            blocks -= 1;
        }
        if blocks == 0 {
            return Err("a response of zeros");
        }
        let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
        // `VICTORY_DSP_CONVOLVE_CHECK=0`: take the response as it is.
        let check = std::env::var_os("VICTORY_DSP_CONVOLVE_CHECK").is_none_or(|v| v != "0");
        if check && !agrees(plan(), &ir[..blocks * N], blocks) {
            return Err("the windows do not agree with their neighbours");
        }
        let energy: Vec<f64> = (0..blocks)
            .map(|k| {
                ir[k * N..(k + 1) * N]
                    .iter()
                    .map(|&v| f64::from(finite(v)).powi(2))
                    .sum()
            })
            .collect();
        let total: f64 = energy.iter().sum();
        let (mut parts, mut dropped) = (blocks, 0.0);
        while parts > 1 && dropped + energy[parts - 1] <= total * TAIL_FLOOR {
            parts -= 1;
            dropped += energy[parts];
        }
        let mut h = vec![0f32; parts * B * 2];
        for k in 0..parts {
            let block = &ir[k * N..(k + 1) * N];
            for bin in 0..B {
                let (c, b) = (bin / CH, bin % CH);
                let at = (c * parts + k) * CH2 + b;
                h[at] = finite(block[2 * bin]);
                h[at + CH] = finite(block[2 * bin + 1]);
            }
        }
        Ok(Self {
            parts,
            h,
            pruned: blocks - parts,
        })
    }

    /// Bytes held.
    fn bytes(&self) -> usize {
        self.h.len() * 4
    }
}

/// The samples of a window of the response (bins as the title lays them
/// out), the first and the second half.
fn window(plan: &Plan, block: &[f32]) -> ([f32; B], [f32; B]) {
    let (mut yr, mut yi) = ([0f32; B], [0f32; B]);
    for bin in 0..B {
        let (r, i) = (block[2 * bin], block[2 * bin + 1]);
        yr[bin] = if r.is_finite() { r } else { 0.0 };
        yi[bin] = if i.is_finite() && bin > 0 { i } else { 0.0 };
    }
    let (mut re, mut im) = ([0f32; M], [0f32; M]);
    unspectrum(plan, &yr, &yi, &mut re, &mut im);
    let (mut first, mut second) = ([0f32; B], [0f32; B]);
    for m in 0..M {
        let n = 2 * m;
        let (even, odd) = (im[m], re[m]);
        if n < B {
            (first[n], first[n + 1]) = (even, odd);
        } else {
            (second[n - B], second[n - B + 1]) = (even, odd);
        }
    }
    (first, second)
}

/// Whether the windows read as one response in the time domain, as the
/// layout says: the second half of a window is the first half of the next
/// (checked at the start, the middle and the end), and the first half of the
/// first one is zeros.
fn agrees(plan: &Plan, ir: &[f32], blocks: usize) -> bool {
    let energy = |x: &[f32; B]| x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
    let tolerance = f64::from(AGREEMENT).powi(2);
    let (first, second) = window(plan, &ir[..N]);
    if energy(&first) > tolerance * energy(&second) + 1e-12 {
        return false;
    }
    let mut at = vec![0, blocks / 2, blocks.saturating_sub(2)];
    at.dedup();
    for k in at.into_iter().filter(|&k| k + 1 < blocks) {
        let (_, tail) = window(plan, &ir[k * N..(k + 1) * N]);
        let (head, _) = window(plan, &ir[(k + 1) * N..(k + 2) * N]);
        let off: f64 = tail
            .iter()
            .zip(&head)
            .map(|(a, b)| f64::from(a - b).powi(2))
            .sum();
        if off > tolerance * energy(&tail).max(energy(&head)) + 1e-12 {
            return false;
        }
    }
    true
}

/// The responses made so far (a stream asks again for the one it plays),
/// by a hash of their floats; `None` for one refused.
type Cache = Vec<([u64; 2], usize, Option<Weak<Spectra>>)>;

fn hash(ir: &[f32]) -> [u64; 2] {
    let (mut a, mut b) = (0x9E37_79B9_7F4A_7C15u64, 0xC2B2_AE3D_27D4_EB4Fu64);
    for &w in ir {
        let w = u64::from(w.to_bits());
        a = (a ^ w).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        a ^= a >> 29;
        b = (b.rotate_left(5) ^ w).wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        b ^= b >> 32;
    }
    [a, b]
}

/// What a block was, for the counters.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Block {
    /// Nothing to compute: the ring held only silence.
    pub skipped: bool,
    /// The input had a sample that was not a number.
    pub repaired: bool,
    pub in_peak: f32,
    pub out_peak: f32,
    /// Windows that took part in the sum.
    pub windows: usize,
}

/// The ring of input spectra of a stream: `parts` slots of a spectrum each,
/// laid out as the windows are.
struct Ring {
    data: Vec<f32>,
    /// Whether the slot's input was not silent.
    live: Vec<bool>,
}

/// One mono convolution stream's state.
pub(super) struct Convolver {
    spectra: Arc<Spectra>,
    ring: Option<Ring>,
    /// The newest slot of the ring.
    head: usize,
    /// Input blocks in a row that were silent.
    quiet: usize,
}

impl Convolver {
    /// `None` when the response is not in the layout described above.
    pub(super) fn new(ir: &[f32]) -> Option<Self> {
        static CACHE: Mutex<Cache> = Mutex::new(Vec::new());
        let (key, len) = (hash(ir), ir.len());
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|(_, _, w)| w.as_ref().is_none_or(|w| w.strong_count() > 0));
        let known = cache.iter().find(|(k, l, _)| *k == key && *l == len);
        let spectra = match known {
            Some((_, _, Some(w))) => w.upgrade(),
            Some((_, _, None)) => return None,
            None => None,
        };
        let spectra = match spectra {
            Some(s) => s,
            None => {
                let started = std::time::Instant::now();
                match Spectra::new(ir) {
                    Ok(s) => {
                        let s = Arc::new(s);
                        tracing::info!(
                            windows = s.parts,
                            pruned = s.pruned,
                            kib = s.bytes() / 1024,
                            ms = started.elapsed().as_secs_f32() * 1000.0,
                            "audio DSP: a response read for computed convolution"
                        );
                        cache.push((key, len, Some(Arc::downgrade(&s))));
                        s
                    }
                    Err(why) => {
                        tracing::info!(
                            ir_floats = len,
                            why,
                            "audio DSP: a response is not in the layout read, its stream stays as before"
                        );
                        cache.push((key, len, None));
                        return None;
                    }
                }
            }
        };
        Some(Self {
            spectra,
            ring: None,
            head: 0,
            quiet: usize::MAX,
        })
    }

    /// Forgets the input so far: no tail comes back later.
    pub(super) fn reset(&mut self) {
        self.ring = None;
        self.head = 0;
        self.quiet = usize::MAX;
    }

    /// Windows of the response.
    #[cfg(test)]
    pub(super) fn windows(&self) -> usize {
        self.spectra.parts
    }

    /// Bytes of the ring now.
    #[cfg(test)]
    pub(super) fn ring_bytes(&self) -> usize {
        self.ring.as_ref().map_or(0, |r| r.data.len() * 4)
    }

    /// One block: 1024 input samples in, 1024 output samples out, scaled by
    /// a gain that goes from `gains[0]` to `gains[1]` over the block.
    pub(super) fn process(&mut self, input: &[f32], output: &mut [f32], gains: [f32; 2]) -> Block {
        self.process_with(simd(), input, output, gains)
    }

    pub(super) fn process_with(
        &mut self,
        simd: bool,
        input: &[f32],
        output: &mut [f32],
        gains: [f32; 2],
    ) -> Block {
        let (mut block, mut in_peak, mut repaired) = ([0f32; B], 0f32, false);
        for (d, &s) in block.iter_mut().zip(input) {
            if s.is_finite() {
                *d = s;
                in_peak = in_peak.max(s.abs());
            } else {
                repaired = true;
            }
        }
        let silent = in_peak == 0.0;
        self.quiet = if silent {
            self.quiet.saturating_add(1)
        } else {
            0
        };
        let parts = self.spectra.parts;
        let output = &mut output[..B];
        if silent && self.quiet >= parts {
            // Nothing the ring holds reaches the output any more.
            self.ring = None;
            self.head = 0;
            output.fill(0.0);
            return Block {
                skipped: true,
                repaired,
                ..Block::default()
            };
        }
        self.head = (self.head + 1) % parts;
        let ring = self.ring.get_or_insert_with(|| Ring {
            data: vec![0.0; parts * B * 2],
            live: vec![false; parts],
        });
        let job = Job {
            plan: plan(),
            h: &self.spectra.h,
            parts,
            ring,
            head: self.head,
            block: &block,
            silent,
            output: &mut *output,
            gains,
        };
        let windows = run(simd, job);
        let out_peak = output.iter().fold(0f32, |p, v| p.max(v.abs()));
        Block {
            skipped: false,
            repaired,
            in_peak,
            out_peak,
            windows,
        }
    }
}

/// Whether the vector code runs: AVX2 on this machine, unless
/// `VICTORY_DSP_CONVOLVE_SIMD=0`.
fn simd() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            std::arch::is_x86_feature_detected!("avx2")
                && std::env::var_os("VICTORY_DSP_CONVOLVE_SIMD").is_none_or(|v| v != "0")
        })
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

struct Job<'a> {
    plan: &'a Plan,
    h: &'a [f32],
    parts: usize,
    ring: &'a mut Ring,
    head: usize,
    block: &'a [f32; B],
    silent: bool,
    output: &'a mut [f32],
    gains: [f32; 2],
}

fn run(simd: bool, job: Job<'_>) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        if simd {
            // SAFETY: `simd` is only set where AVX2 was detected.
            return unsafe { run_avx2(job) };
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = simd;
    job.run()
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn run_avx2(job: Job<'_>) -> usize {
    job.run()
}

impl Job<'_> {
    /// The block, start to end; how many windows the sum took.
    #[inline(always)]
    fn run(self) -> usize {
        let Job {
            plan,
            h,
            parts,
            ring,
            head,
            block,
            silent,
            output,
            gains,
        } = self;
        let (mut re, mut im) = ([0f32; M], [0f32; M]);
        if silent {
            ring.live[head] = false;
        } else {
            let (mut xr, mut xi) = ([0f32; B], [0f32; B]);
            spectrum(plan, block, &mut re, &mut im, &mut xr, &mut xi);
            for c in 0..CHUNKS {
                let at = (c * parts + head) * CH2;
                ring.data[at..at + CH].copy_from_slice(&xr[c * CH..(c + 1) * CH]);
                ring.data[at + CH..at + CH2].copy_from_slice(&xi[c * CH..(c + 1) * CH]);
            }
            ring.live[head] = true;
        }
        // The sum over windows k of X[t-k] * H[k], 32 bins at a time.
        let (mut yr, mut yi) = ([0f32; B], [0f32; B]);
        let mut used = 0;
        for c in 0..CHUNKS {
            let (mut ar, mut ai) = ([0f32; CH], [0f32; CH]);
            let hc = &h[c * parts * CH2..(c + 1) * parts * CH2];
            let xc = &ring.data[c * parts * CH2..(c + 1) * parts * CH2];
            let mut slot = head;
            let mut taken = 0;
            for hk in hc.chunks_exact(CH2) {
                if ring.live[slot] {
                    let xk: &[f32; CH2] = (&xc[slot * CH2..(slot + 1) * CH2])
                        .try_into()
                        .expect("a window is 64 floats");
                    let hk: &[f32; CH2] = hk.try_into().expect("a window is 64 floats");
                    let (xr, xi) = xk.split_at(CH);
                    let (hr, hi) = hk.split_at(CH);
                    for b in 0..CH {
                        ar[b] += xr[b] * hr[b] - xi[b] * hi[b];
                        ai[b] += xr[b] * hi[b] + xi[b] * hr[b];
                    }
                    taken += 1;
                }
                slot = if slot == 0 { parts - 1 } else { slot - 1 };
            }
            used = taken;
            yr[c * CH..(c + 1) * CH].copy_from_slice(&ar);
            yi[c * CH..(c + 1) * CH].copy_from_slice(&ai);
        }
        // The spectrum of a real signal back to samples: the second half of
        // the window is this block's output.
        yi[0] = 0.0;
        unspectrum(plan, &yr, &yi, &mut re, &mut im);
        let scale = 1.0 / N as f32;
        let (g0, step) = (gains[0] * scale, (gains[1] - gains[0]) * scale / B as f32);
        for j in 0..B / 2 {
            let n = 2 * j;
            output[n] = im[M / 2 + j] * (g0 + step * n as f32);
            output[n + 1] = re[M / 2 + j] * (g0 + step * (n + 1) as f32);
        }
        used
    }
}

#[cfg(test)]
mod tests {
    use super::reference::{Reference, layout};
    use super::*;

    /// Noise in [-1, 1) from a seed, the same in the offline reference.
    fn noise(seed: &mut u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (*seed >> 8) as f32 / (1 << 23) as f32 - 1.0
            })
            .collect()
    }

    /// A decaying response of `n` samples.
    fn decaying(seed: u32, n: usize, tau: f32) -> Vec<f32> {
        let mut seed = seed;
        noise(&mut seed, n)
            .iter()
            .enumerate()
            .map(|(i, v)| v * 0.5 * (-(i as f32) / tau).exp())
            .collect()
    }

    fn run_blocks(c: &mut Convolver, simd: bool, input: &[f32], gains: [f32; 2]) -> Vec<f32> {
        let mut out = vec![0f32; input.len()];
        for (i, o) in input.chunks(B).zip(out.chunks_mut(B)) {
            c.process_with(simd, i, o, gains);
        }
        out
    }

    fn direct(ir: &[f32], input: &[f32]) -> Vec<f32> {
        (0..input.len())
            .map(|n| {
                (0..=n)
                    .filter(|&m| n - m < ir.len())
                    .map(|m| f64::from(input[m]) * f64::from(ir[n - m]))
                    .sum::<f64>() as f32
            })
            .collect()
    }

    /// Both versions, with and without the vector code, give what a direct
    /// convolution gives, across the edges of five blocks.
    #[test]
    fn matches_a_direct_convolution_across_blocks() {
        let ir = decaying(7, 2500, 900.0);
        let mut seed = 99;
        let input = noise(&mut seed, B * 5);
        let want = direct(&ir, &input);
        let mut variants = vec![false];
        if simd() {
            variants.push(true);
        }
        for simd in variants {
            let mut c = Convolver::new(&layout(&ir)).unwrap();
            assert_eq!(c.windows(), 4, "two and a half blocks and the window after");
            let got = run_blocks(&mut c, simd, &input, [1.0, 1.0]);
            for n in 0..B * 5 {
                assert!(
                    (got[n] - want[n]).abs() < 2e-3,
                    "simd {simd}, sample {n}: {} vs {}",
                    got[n],
                    want[n]
                );
            }
        }
    }

    /// Known numbers, from an offline run of numpy (float64, `np.convolve`)
    /// on the same noise: a response of 3000 samples decaying with tau 700,
    /// five blocks of input; sums and energies of each output block, and
    /// some samples.
    #[test]
    fn matches_the_offline_numbers() {
        let ir = decaying(1, 3000, 700.0);
        let mut seed = 2;
        let input = noise(&mut seed, B * 5);
        let mut c = Convolver::new(&layout(&ir)).unwrap();
        let got = run_blocks(&mut c, simd(), &input, [1.0, 1.0]);
        let sums = GOLDEN_SUMS;
        let energies = GOLDEN_ENERGIES;
        for b in 0..5 {
            let block = &got[b * B..(b + 1) * B];
            let sum: f32 = block.iter().sum();
            let energy: f32 = block.iter().map(|v| v * v).sum();
            assert!(
                (sum - sums[b]).abs() < 0.05,
                "block {b}: sum {sum} vs {}",
                sums[b]
            );
            assert!(
                (energy / energies[b] - 1.0).abs() < 1e-3,
                "block {b}: energy {energy} vs {}",
                energies[b]
            );
        }
        for (n, want) in GOLDEN_SAMPLES {
            assert!(
                (got[n] - want).abs() < 5e-3,
                "sample {n}: {} vs {want}",
                got[n]
            );
        }
    }

    /// The faster version gives what the first one did, on a long response
    /// (100 windows, as the longest the title has), and so do the two gain
    /// forms.
    #[test]
    fn matches_the_first_version_on_a_long_response() {
        let ir = decaying(5, 100 * B - 700, 30_000.0);
        let mut seed = 17;
        let input = noise(&mut seed, B * 8);
        let mut fast = Convolver::new(&layout(&ir)).unwrap();
        let mut slow = Reference::new(&layout(&ir)).unwrap();
        assert!(fast.windows() > 90);
        let got = run_blocks(&mut fast, simd(), &input, [1.0, 1.0]);
        let mut want = vec![0f32; input.len()];
        for (i, o) in input.chunks(B).zip(want.chunks_mut(B)) {
            slow.process(i, o, 1.0);
        }
        let peak = want.iter().fold(0f32, |p, v| p.max(v.abs()));
        for n in 0..got.len() {
            assert!(
                (got[n] - want[n]).abs() < 1e-3 * peak.max(1.0),
                "sample {n}: {} vs {}",
                got[n],
                want[n]
            );
        }
    }

    /// The gain goes from the first value to the second over the block and
    /// scales it; a gain of zero is silence.
    #[test]
    fn the_gain_is_a_ramp() {
        let ir = vec![1.0];
        let mut seed = 3;
        let input = noise(&mut seed, B);
        let mut c = Convolver::new(&layout(&ir)).unwrap();
        let mut out = vec![0f32; B];
        c.process_with(false, &input, &mut out, [0.5, 1.5]);
        for n in [0, 1, 100, 511, 1023] {
            let g = 0.5 + n as f32 / B as f32;
            assert!((out[n] - input[n] * g).abs() < 1e-4, "sample {n}");
        }
        let mut c = Convolver::new(&layout(&ir)).unwrap();
        c.process_with(false, &input, &mut out, [0.0, 0.0]);
        assert!(out.iter().all(|&v| v == 0.0));
    }

    /// With silence in, the ring is let go after the response's length and
    /// the output is exactly zero; the tail of a sound is heard until then
    /// and its end does not leave a click.
    #[test]
    fn silence_costs_nothing_and_the_tail_ends() {
        let ir = decaying(11, 3 * B, 400.0);
        let mut c = Convolver::new(&layout(&ir)).unwrap();
        let parts = c.windows();
        let mut out = vec![0f32; B];
        let mut seed = 4;
        let loud = noise(&mut seed, B);
        c.process(&loud, &mut out, [1.0, 1.0]);
        assert!(c.ring_bytes() > 0);
        let mut heard = 0;
        for i in 0..parts + 2 {
            let b = c.process(&[0.0; B], &mut out, [1.0, 1.0]);
            if out.iter().any(|&v| v != 0.0) {
                heard = i + 1;
            }
            if i + 1 >= parts {
                assert!(b.skipped, "block {i} of {parts}");
            }
        }
        assert!(heard >= 2, "the tail is heard for {heard} blocks");
        assert!(heard < parts, "and not past the response: {heard}");
        assert_eq!(c.ring_bytes(), 0);
        assert!(out.iter().all(|&v| v == 0.0));
        // sound again: the ring is back and the output is the response's
        let b = c.process(&loud, &mut out, [1.0, 1.0]);
        assert!(!b.skipped && b.windows == 1);
        assert!(out.iter().any(|&v| v != 0.0));
    }

    /// Windows whose input was silent do not take part in the sum.
    #[test]
    fn silent_blocks_are_skipped_in_the_sum() {
        let ir = decaying(13, 40 * B, 20_000.0);
        let mut c = Convolver::new(&layout(&ir)).unwrap();
        let mut out = vec![0f32; B];
        let mut seed = 6;
        let loud = noise(&mut seed, B);
        let silence = [0f32; B];
        let mut live = 0;
        for i in 0..20 {
            let input = if i % 4 == 0 { &loud[..] } else { &silence[..] };
            live += usize::from(i % 4 == 0);
            let b = c.process(input, &mut out, [1.0, 1.0]);
            assert_eq!(b.windows, live);
        }
    }

    /// Streams that play one response share it.
    #[test]
    fn a_response_is_read_once() {
        let ir = layout(&decaying(21, 5000, 1500.0));
        let a = Convolver::new(&ir).unwrap();
        let b = Convolver::new(&ir).unwrap();
        assert!(Arc::ptr_eq(&a.spectra, &b.spectra));
        let mut other = ir.clone();
        other[10] += 1.0;
        // not the layout of any signal, but one that a neighbour check
        // may or may not accept: only that it is not shared
        if let Some(c) = Convolver::new(&other) {
            assert!(!Arc::ptr_eq(&a.spectra, &c.spectra));
        }
    }

    /// A response that is not in the layout is refused, whatever its size:
    /// noise, and windows that are not neighbours of each other.
    #[test]
    fn a_response_that_is_not_in_the_layout_is_refused() {
        let mut seed = 8;
        assert!(Convolver::new(&noise(&mut seed, 6 * N)).is_none());
        assert!(Convolver::new(&noise(&mut seed, 3 * N + 5)).is_none());
        assert!(Convolver::new(&[]).is_none());
        assert!(Convolver::new(&vec![0.0; 3 * N]).is_none());
        // windows of a response, in the wrong order
        let mut ir = layout(&decaying(31, 4 * B, 3000.0));
        let (a, rest) = ir.split_at_mut(N);
        a.swap_with_slice(&mut rest[N..2 * N]);
        assert!(Convolver::new(&ir).is_none());
    }

    /// A tail below -90 dB of the response is not summed.
    #[test]
    fn an_inaudible_tail_is_dropped() {
        let mut ir = decaying(41, 2 * B, 1e9);
        ir.resize(40 * B, 0.0);
        ir[40 * B - 1] = 1e-6;
        let c = Convolver::new(&layout(&ir)).unwrap();
        assert!(c.windows() < 6, "{} windows", c.windows());
        let ir = decaying(41, 20 * B, 1e9);
        assert!(Convolver::new(&layout(&ir)).unwrap().windows() > 20);
    }

    /// Not a number in the input leaves the output finite.
    #[test]
    fn garbage_in_does_not_stay() {
        let mut c = Convolver::new(&layout(&decaying(51, 2 * B, 800.0))).unwrap();
        let mut seed = 9;
        let mut input = noise(&mut seed, B);
        input[5] = f32::NAN;
        input[6] = f32::INFINITY;
        let mut out = vec![0f32; B];
        let b = c.process(&input, &mut out, [1.0, 1.0]);
        assert!(b.repaired && out.iter().all(|v| v.is_finite()));
        let b = c.process(&noise(&mut seed, B), &mut out, [1.0, 1.0]);
        assert!(!b.repaired && out.iter().all(|v| v.is_finite()));
    }

    /// The cost of a block of a long response: a small fraction of the
    /// 21 ms the block lasts, and well under the first version's.
    #[test]
    fn a_block_is_cheap() {
        let (blocks, budget) = if cfg!(debug_assertions) {
            (4, 200.0)
        } else {
            (60, 3.0)
        };
        let ir = decaying(61, 90 * B, 40_000.0);
        let mut c = Convolver::new(&layout(&ir)).unwrap();
        let mut seed = 10;
        let input = noise(&mut seed, B);
        let mut out = vec![0f32; B];
        for _ in 0..90 {
            c.process(&input, &mut out, [1.0, 1.0]);
        }
        let start = std::time::Instant::now();
        for _ in 0..blocks {
            c.process(&input, &mut out, [1.0, 1.0]);
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0 / blocks as f64;
        eprintln!(
            "computed convolution, {} windows: {ms:.3} ms a block",
            c.windows()
        );
        assert!(ms < budget, "{ms} ms a block");
    }

    /// `cargo test --release -p victory-hle -- --ignored --nocapture
    /// convolution_bench`: milliseconds a block of 1024 samples for responses
    /// of 10, 40 and 100 windows, the first version, ours without and with
    /// the vector code, and (as an ordinary signal) a silent input.
    #[test]
    #[ignore = "a measurement"]
    fn convolution_bench() {
        let mut seed = 77;
        let input = noise(&mut seed, B);
        let mut out = vec![0f32; B];
        let time = |f: &mut dyn FnMut()| {
            for _ in 0..30 {
                f();
            }
            let reps = 300;
            let start = std::time::Instant::now();
            for _ in 0..reps {
                f();
            }
            start.elapsed().as_secs_f64() * 1000.0 / f64::from(reps)
        };
        for windows in [10usize, 40, 100] {
            let ir = layout(&decaying(71, windows * B - 100, windows as f32 * 400.0));
            let mut old = Reference::new(&ir).unwrap();
            let first = time(&mut || old.process(&input, &mut out, 1.0));
            let mut c = Convolver::new(&ir).unwrap();
            let scalar = time(&mut || {
                c.process_with(false, &input, &mut out, [1.0, 1.0]);
            });
            let vector = if simd() {
                time(&mut || {
                    c.process_with(true, &input, &mut out, [1.0, 1.0]);
                })
            } else {
                f64::NAN
            };
            eprintln!(
                "convolution_bench: {} windows: first version {first:.3} ms, ours {scalar:.3} ms (no vector code), {vector:.3} ms (AVX2): {:.1}x",
                c.windows(),
                first / vector.min(scalar)
            );
        }
    }

    const GOLDEN_SUMS: [f32; 5] = [174.0077, 44.6818, -44.8700, -135.2235, 114.2358];
    const GOLDEN_ENERGIES: [f32; 5] = [6880.9967, 10912.8053, 10281.4411, 9460.5394, 9574.0909];
    const GOLDEN_SAMPLES: [(usize, f32); 12] = [
        (0, 0.13871),
        (1, 0.08980),
        (500, -1.19573),
        (1023, -3.51330),
        (1024, -1.98225),
        (1500, -2.74081),
        (2047, 7.71751),
        (2048, -0.93499),
        (3333, -1.38575),
        (4095, 1.27320),
        (4096, 1.50817),
        (5000, 4.05068),
    ];
}
