//! What the XMA decoder did, counted for the log: `VICTORY_AUDIO_STATS=<n>`
//! logs a line every `n` seconds (info level) with the counts of that
//! window. Without it nothing is logged and the counters cost an atomic add.
//!
//! The numbers answer, without ears, whether the audio keeps real time:
//! how late the rings were refilled after the title read from them (the
//! title's mixer wakes every 5.3 ms), how far apart the decoding passes ran
//! (the thread shares the machine with a title that keeps every core
//! busy), how often a stream's input ran out, and how much the title took
//! per second of the wall clock.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// A ring refilled later than this after a read could not feed the next
/// mixer block (256 samples at 48 kHz).
pub const MIXER_BLOCK: Duration = Duration::from_micros(5_333);
/// Decoding passes are asked for every 2 ms; this many apart is a stall.
pub const PASS_STALL: Duration = Duration::from_millis(8);

#[derive(Default)]
pub struct Counters {
    enabled: AtomicU64,
    disabled: AtomicU64,
    frames: AtomicU64,
    bad_frames: AtomicU64,
    input_ended: AtomicU64,
    blocks_out: AtomicU64,
    silent_blocks: AtomicU64,
    blocks_taken: AtomicU64,
    /// Seconds of audio the title took, in microseconds.
    taken_us: AtomicU64,
    refills: AtomicU64,
    refill_late: AtomicU64,
    refill_max_us: AtomicU64,
    passes: AtomicU64,
    pass_stalls: AtomicU64,
    gap_max_us: AtomicU64,
    pass_max_us: AtomicU64,
    peak: AtomicU64,
    unknown_commands: AtomicU64,
    reverb_blocks: AtomicU64,
    reverb_bad_blocks: AtomicU64,
    reverb_in_peak_milli: AtomicU64,
    reverb_out_peak_milli: AtomicU64,
    reverb_us: AtomicU64,
    reverb_max_us: AtomicU64,
    reverb_gain_ramps: AtomicU64,
    reverb_gain_steps: AtomicU64,
    convolve_blocks: AtomicU64,
    convolve_skipped: AtomicU64,
    convolve_bad_blocks: AtomicU64,
    convolve_in_peak_milli: AtomicU64,
    convolve_out_peak_milli: AtomicU64,
    convolve_us: AtomicU64,
    convolve_max_us: AtomicU64,
    convolve_windows: AtomicU64,
    convolve_gain_ramps: AtomicU64,
    convolve_gain_steps: AtomicU64,
    opus_packets: AtomicU64,
    opus_errors: AtomicU64,
    opus_pitched: AtomicU64,
}

/// The counters of one window, plain.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Window {
    pub enabled: u64,
    pub disabled: u64,
    pub frames: u64,
    pub bad_frames: u64,
    pub input_ended: u64,
    pub blocks_out: u64,
    pub silent_blocks: u64,
    pub blocks_taken: u64,
    pub taken_us: u64,
    pub refills: u64,
    pub refill_late: u64,
    pub refill_max_us: u64,
    pub passes: u64,
    pub pass_stalls: u64,
    pub gap_max_us: u64,
    pub pass_max_us: u64,
    pub peak: u64,
    pub unknown_commands: u64,
    pub reverb_blocks: u64,
    pub reverb_bad_blocks: u64,
    pub reverb_in_peak_milli: u64,
    pub reverb_out_peak_milli: u64,
    pub reverb_us: u64,
    pub reverb_max_us: u64,
    pub reverb_gain_ramps: u64,
    pub reverb_gain_steps: u64,
    pub convolve_blocks: u64,
    pub convolve_skipped: u64,
    pub convolve_bad_blocks: u64,
    pub convolve_in_peak_milli: u64,
    pub convolve_out_peak_milli: u64,
    pub convolve_us: u64,
    pub convolve_max_us: u64,
    pub convolve_windows: u64,
    pub convolve_gain_ramps: u64,
    pub convolve_gain_steps: u64,
    pub opus_packets: u64,
    pub opus_errors: u64,
    pub opus_pitched: u64,
}

pub static COUNTERS: Counters = Counters::new();

impl Counters {
    pub const fn new() -> Self {
        macro_rules! zero {
            ($($f:ident),*) => { Counters { $($f: AtomicU64::new(0)),* } };
        }
        zero!(
            enabled,
            disabled,
            frames,
            bad_frames,
            input_ended,
            blocks_out,
            silent_blocks,
            blocks_taken,
            taken_us,
            refills,
            refill_late,
            refill_max_us,
            passes,
            pass_stalls,
            gap_max_us,
            pass_max_us,
            peak,
            unknown_commands,
            reverb_blocks,
            reverb_bad_blocks,
            reverb_in_peak_milli,
            reverb_out_peak_milli,
            reverb_us,
            reverb_max_us,
            reverb_gain_ramps,
            reverb_gain_steps,
            convolve_blocks,
            convolve_skipped,
            convolve_bad_blocks,
            convolve_in_peak_milli,
            convolve_out_peak_milli,
            convolve_us,
            convolve_max_us,
            convolve_windows,
            convolve_gain_ramps,
            convolve_gain_steps,
            opus_packets,
            opus_errors,
            opus_pitched
        )
    }

    /// A block through the stand-in reverb (`VICTORY_DSP_REVERB`): the
    /// largest sample in and out, and whether it had to be repaired
    /// (a sample that was not a number, or a network that ran away).
    pub fn reverb_block(&self, input_peak: f32, output_peak: f32, repaired: bool, took: Duration) {
        let milli = |v: f32| (v * 1000.0).min(1e12) as u64;
        let us = took.as_micros() as u64;
        self.reverb_us.fetch_add(us, Relaxed);
        self.reverb_max_us.fetch_max(us, Relaxed);
        self.reverb_blocks.fetch_add(1, Relaxed);
        self.reverb_bad_blocks
            .fetch_add(u64::from(repaired), Relaxed);
        self.reverb_in_peak_milli
            .fetch_max(milli(input_peak), Relaxed);
        self.reverb_out_peak_milli
            .fetch_max(milli(output_peak), Relaxed);
    }

    /// The gains of a reverb block: whether they differ within it (a ramp),
    /// and whether they start somewhere other than where the stream's last
    /// block ended.
    pub fn reverb_gains(&self, ramp: bool, step: bool) {
        self.reverb_gain_ramps.fetch_add(u64::from(ramp), Relaxed);
        self.reverb_gain_steps.fetch_add(u64::from(step), Relaxed);
    }

    /// A block through the computed convolution (`VICTORY_DSP_CONVOLVE`):
    /// whether there was nothing to compute (silence in the ring), whether
    /// the input had to be repaired, the largest sample in and out, the
    /// windows of the response that were summed, the gains (whether they
    /// differ within the block, and whether they start somewhere other than
    /// where the stream's last block ended) and the time it took.
    #[allow(clippy::too_many_arguments)]
    pub fn convolve_block(
        &self,
        skipped: bool,
        repaired: bool,
        input_peak: f32,
        output_peak: f32,
        windows: usize,
        gains: (bool, bool),
        took: Duration,
    ) {
        let milli = |v: f32| (v * 1000.0).min(1e12) as u64;
        let us = took.as_micros() as u64;
        self.convolve_us.fetch_add(us, Relaxed);
        self.convolve_max_us.fetch_max(us, Relaxed);
        self.convolve_blocks.fetch_add(1, Relaxed);
        self.convolve_skipped.fetch_add(u64::from(skipped), Relaxed);
        self.convolve_bad_blocks
            .fetch_add(u64::from(repaired), Relaxed);
        self.convolve_windows.fetch_add(windows as u64, Relaxed);
        self.convolve_in_peak_milli
            .fetch_max(milli(input_peak), Relaxed);
        self.convolve_out_peak_milli
            .fetch_max(milli(output_peak), Relaxed);
        self.convolve_gain_ramps
            .fetch_add(u64::from(gains.0), Relaxed);
        self.convolve_gain_steps
            .fetch_add(u64::from(gains.1), Relaxed);
    }

    /// An Opus packet decoded for the title (`xapu`): whether it failed, and
    /// whether the title asked for a pitch other than 1, which we do not
    /// apply.
    pub fn opus_packet(&self, failed: bool, pitched: bool) {
        self.opus_packets.fetch_add(1, Relaxed);
        self.opus_errors.fetch_add(u64::from(failed), Relaxed);
        self.opus_pitched.fetch_add(u64::from(pitched), Relaxed);
    }

    /// A coprocessor command no unit of ours carries out.
    pub fn unknown_command(&self) {
        self.unknown_commands.fetch_add(1, Relaxed);
    }

    pub fn voice_enabled(&self) {
        self.enabled.fetch_add(1, Relaxed);
    }

    pub fn voice_disabled(&self) {
        self.disabled.fetch_add(1, Relaxed);
    }

    pub fn frame(&self, decoded: bool) {
        self.frames.fetch_add(1, Relaxed);
        self.bad_frames.fetch_add(u64::from(!decoded), Relaxed);
    }

    pub fn input_ended(&self) {
        self.input_ended.fetch_add(1, Relaxed);
    }

    /// One 256-byte block written to a ring, whose largest sample was `peak`.
    pub fn block_out(&self, peak: u16) {
        self.blocks_out.fetch_add(1, Relaxed);
        self.silent_blocks.fetch_add(u64::from(peak == 0), Relaxed);
        self.peak.fetch_max(u64::from(peak), Relaxed);
    }

    /// The title took `blocks` blocks, `us` microseconds of audio.
    pub fn taken(&self, blocks: u64, us: u64) {
        self.blocks_taken.fetch_add(blocks, Relaxed);
        self.taken_us.fetch_add(us, Relaxed);
    }

    /// A ring was full again `after` its title made room.
    pub fn refilled(&self, after: Duration) {
        let us = after.as_micros() as u64;
        self.refills.fetch_add(1, Relaxed);
        self.refill_late
            .fetch_add(u64::from(after > MIXER_BLOCK), Relaxed);
        self.refill_max_us.fetch_max(us, Relaxed);
    }

    /// A decoding pass that started `gap` after the previous one and took
    /// `took`.
    pub fn pass(&self, gap: Duration, took: Duration) {
        self.passes.fetch_add(1, Relaxed);
        self.pass_stalls
            .fetch_add(u64::from(gap > PASS_STALL), Relaxed);
        self.gap_max_us.fetch_max(gap.as_micros() as u64, Relaxed);
        self.pass_max_us.fetch_max(took.as_micros() as u64, Relaxed);
    }

    /// The counts since the last call.
    pub fn take(&self) -> Window {
        let t = |c: &AtomicU64| c.swap(0, Relaxed);
        Window {
            enabled: t(&self.enabled),
            disabled: t(&self.disabled),
            frames: t(&self.frames),
            bad_frames: t(&self.bad_frames),
            input_ended: t(&self.input_ended),
            blocks_out: t(&self.blocks_out),
            silent_blocks: t(&self.silent_blocks),
            blocks_taken: t(&self.blocks_taken),
            taken_us: t(&self.taken_us),
            refills: t(&self.refills),
            refill_late: t(&self.refill_late),
            refill_max_us: t(&self.refill_max_us),
            passes: t(&self.passes),
            pass_stalls: t(&self.pass_stalls),
            gap_max_us: t(&self.gap_max_us),
            pass_max_us: t(&self.pass_max_us),
            peak: t(&self.peak),
            unknown_commands: t(&self.unknown_commands),
            reverb_blocks: t(&self.reverb_blocks),
            reverb_bad_blocks: t(&self.reverb_bad_blocks),
            reverb_in_peak_milli: t(&self.reverb_in_peak_milli),
            reverb_out_peak_milli: t(&self.reverb_out_peak_milli),
            reverb_us: t(&self.reverb_us),
            reverb_max_us: t(&self.reverb_max_us),
            reverb_gain_ramps: t(&self.reverb_gain_ramps),
            reverb_gain_steps: t(&self.reverb_gain_steps),
            convolve_blocks: t(&self.convolve_blocks),
            convolve_skipped: t(&self.convolve_skipped),
            convolve_bad_blocks: t(&self.convolve_bad_blocks),
            convolve_in_peak_milli: t(&self.convolve_in_peak_milli),
            convolve_out_peak_milli: t(&self.convolve_out_peak_milli),
            convolve_us: t(&self.convolve_us),
            convolve_max_us: t(&self.convolve_max_us),
            convolve_windows: t(&self.convolve_windows),
            convolve_gain_ramps: t(&self.convolve_gain_ramps),
            convolve_gain_steps: t(&self.convolve_gain_steps),
            opus_packets: t(&self.opus_packets),
            opus_errors: t(&self.opus_errors),
            opus_pitched: t(&self.opus_pitched),
        }
    }
}

/// The window length asked for with `VICTORY_AUDIO_STATS` (seconds).
fn period() -> Option<Duration> {
    static PERIOD: std::sync::OnceLock<Option<Duration>> = std::sync::OnceLock::new();
    *PERIOD.get_or_init(|| {
        let seconds: f32 = std::env::var("VICTORY_AUDIO_STATS").ok()?.parse().ok()?;
        (seconds > 0.0).then(|| Duration::from_secs_f32(seconds))
    })
}

/// Called by the decoding thread after each pass: logs the window when it
/// is over. `voices` is how many contexts are enabled now.
pub fn tick(since: &mut Instant, voices: usize) {
    let Some(period) = period() else { return };
    let elapsed = since.elapsed();
    // A window ends where an arm of `VICTORY_AB` changes, so that each one
    // counts one arm: it is logged with the arm it counted.
    static ARM: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let arm = ARM.load(Relaxed);
    let now = super::live::arm();
    if elapsed < period && now == arm {
        return;
    }
    ARM.store(now, Relaxed);
    *since = Instant::now();
    let w = COUNTERS.take();
    let ms = |us: u64| us as f32 / 1000.0;
    tracing::info!(
        window_s = elapsed.as_secs_f32(),
        arm,
        voices,
        enabled = w.enabled,
        disabled = w.disabled,
        frames = w.frames,
        bad_frames = w.bad_frames,
        input_ended = w.input_ended,
        blocks_out = w.blocks_out,
        silent_blocks = w.silent_blocks,
        blocks_taken = w.blocks_taken,
        // Voices' worth of audio the title took per second of the clock:
        // with `voices` all playing, about `voices`.
        taken_voices = w.taken_us as f32 / elapsed.as_micros().max(1) as f32,
        refills = w.refills,
        refill_late = w.refill_late,
        refill_max_ms = ms(w.refill_max_us),
        passes = w.passes,
        pass_stalls = w.pass_stalls,
        gap_max_ms = ms(w.gap_max_us),
        pass_max_ms = ms(w.pass_max_us),
        peak = w.peak,
        unknown_commands = w.unknown_commands,
        reverb_blocks = w.reverb_blocks,
        reverb_bad_blocks = w.reverb_bad_blocks,
        reverb_in_peak = w.reverb_in_peak_milli as f32 / 1000.0,
        reverb_out_peak = w.reverb_out_peak_milli as f32 / 1000.0,
        reverb_ms = w.reverb_us as f32 / 1000.0,
        reverb_max_block_ms = w.reverb_max_us as f32 / 1000.0,
        reverb_gain_ramps = w.reverb_gain_ramps,
        reverb_gain_steps = w.reverb_gain_steps,
        convolve_blocks = w.convolve_blocks,
        convolve_skipped = w.convolve_skipped,
        convolve_bad_blocks = w.convolve_bad_blocks,
        convolve_in_peak = w.convolve_in_peak_milli as f32 / 1000.0,
        convolve_out_peak = w.convolve_out_peak_milli as f32 / 1000.0,
        convolve_ms = w.convolve_us as f32 / 1000.0,
        convolve_max_block_ms = w.convolve_max_us as f32 / 1000.0,
        // windows summed per block that was computed
        convolve_windows = w.convolve_windows as f32
            / w.convolve_blocks.saturating_sub(w.convolve_skipped).max(1) as f32,
        convolve_gain_ramps = w.convolve_gain_ramps,
        convolve_gain_steps = w.convolve_gain_steps,
        opus_packets = w.opus_packets,
        opus_errors = w.opus_errors,
        opus_pitched = w.opus_pitched,
        "audio stats"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_holds_what_happened_since_the_last() {
        let c = Counters::new();
        c.voice_enabled();
        c.frame(true);
        c.frame(false);
        c.block_out(0);
        c.block_out(900);
        c.block_out(300);
        c.taken(4, 10_666);
        c.refilled(Duration::from_millis(1));
        c.refilled(Duration::from_millis(7));
        c.pass(Duration::from_millis(2), Duration::from_micros(300));
        c.pass(Duration::from_millis(20), Duration::from_micros(900));
        let w = c.take();
        assert_eq!(
            (w.enabled, w.disabled, w.frames, w.bad_frames),
            (1, 0, 2, 1)
        );
        assert_eq!((w.blocks_out, w.silent_blocks, w.peak), (3, 1, 900));
        assert_eq!((w.blocks_taken, w.taken_us), (4, 10_666));
        assert_eq!((w.refills, w.refill_late, w.refill_max_us), (2, 1, 7_000));
        assert_eq!((w.passes, w.pass_stalls), (2, 1));
        assert_eq!((w.gap_max_us, w.pass_max_us), (20_000, 900));
        // Taking resets: the next window starts empty.
        assert_eq!(c.take(), Window::default());
    }
}
