//! The time of each presented frame, said as a line every 5 s: the 5-s lines
//! of the frame rate are means of frames, and a stall that is a few frames of
//! half a second in a stretch of thirty is no mean (`VICTORY_FRAME_STATS=0`:
//! not said). A cutscene is a fixed sequence of frames, so these are the
//! numbers to compare two runs of it by: the median and the 95th percentile
//! of a frame, the longest, and how many (and how much of the time) went in
//! frames over 200 ms.
//!
//! With `VICTORY_FRAME_STATS=<a file>` every frame is also written there, a
//! line of "when it was presented (ms since 1970, UTC, as the log's times are), ms of the frame" each
//! (flushed with the line of the window), for the exact percentiles of a
//! stretch of a run.

use std::io::Write;
use std::time::Instant;

/// Frames kept for a line (a window is some hundreds; a menu may give
/// thousands a second).
const MOST: usize = 1 << 16;

/// The frames since the last line.
pub(super) struct Times {
    /// When the first frame's interval began, and the clock then (ms since 1970).
    first: Instant,
    epoch_ms: u64,
    last: Instant,
    ms: Vec<f32>,
    /// Where each frame goes as well, if asked.
    file: Option<std::io::BufWriter<std::fs::File>>,
    /// The frames of the window for the file: when it was presented (ms since 1970) and its time.
    pending: Vec<(u64, f32)>,
}

/// What a window's frames come to.
#[derive(Debug, PartialEq)]
pub(super) struct Summary {
    pub frames: usize,
    pub p50: f32,
    pub p95: f32,
    pub p99: f32,
    pub max: f32,
    pub over_100: usize,
    pub over_200: usize,
    pub over_500: usize,
    /// Time in the frames over 200 ms, ms.
    pub in_over_200: f64,
    pub total: f64,
}

impl Times {
    pub(super) fn new() -> Self {
        let now = Instant::now();
        // (A value of one character, or 0, is a switch and not a file.)
        let file = std::env::var("VICTORY_FRAME_STATS")
            .ok()
            .filter(|v| v.len() > 1)
            .and_then(|path| std::fs::File::create(path).ok())
            .map(std::io::BufWriter::new);
        let epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        Times {
            first: now,
            epoch_ms,
            last: now,
            ms: Vec::with_capacity(1024),
            file,
            pending: Vec::new(),
        }
    }

    /// A frame was presented.
    pub(super) fn frame(&mut self) {
        let now = Instant::now();
        let ms = (now - self.last).as_secs_f32() * 1000.0;
        self.last = now;
        if self.ms.len() < MOST {
            self.ms.push(ms);
            if self.file.is_some() {
                self.pending
                    .push((self.epoch_ms + (now - self.first).as_millis() as u64, ms));
            }
        }
    }

    /// Says the line for the frames since the last one, and starts the next.
    pub(super) fn line(&mut self) {
        static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
            std::env::var_os("VICTORY_FRAME_STATS").is_none_or(|v| v != "0")
        });
        if let Some(file) = &mut self.file {
            for (at, ms) in self.pending.drain(..) {
                let _ = writeln!(file, "{at}	{ms:.2}");
            }
            let _ = file.flush();
        }
        let summary = summarize(&mut self.ms);
        self.ms.clear();
        if let (true, Some(s)) = (*ON, summary) {
            tracing::info!(
                "frame times, last 5 s (VICTORY_FRAME_STATS): {} frames, p50 {:.1} ms, p95 {:.1}, p99 {:.1}, max {:.1}; over 100 ms {}, over 200 ms {}, over 500 ms {}; \
                 {:.0} ms of the {:.0} ms went in frames over 200 ms",
                s.frames,
                s.p50,
                s.p95,
                s.p99,
                s.max,
                s.over_100,
                s.over_200,
                s.over_500,
                s.in_over_200,
                s.total
            );
        }
    }
}

/// The summary of `ms` (sorted by it); `None` for no frame.
pub(super) fn summarize(ms: &mut [f32]) -> Option<Summary> {
    if ms.is_empty() {
        return None;
    }
    ms.sort_unstable_by(f32::total_cmp);
    // Nearest rank: the smallest frame that at least `p` of them are not longer than.
    let rank = |p: f64| ms[((p * ms.len() as f64).ceil() as usize).clamp(1, ms.len()) - 1];
    let over = |limit: f32| ms.len() - ms.partition_point(|&m| m <= limit);
    Some(Summary {
        frames: ms.len(),
        p50: rank(0.50),
        p95: rank(0.95),
        p99: rank(0.99),
        max: ms[ms.len() - 1],
        over_100: over(100.0),
        over_200: over(200.0),
        over_500: over(500.0),
        in_over_200: ms
            .iter()
            .filter(|&&m| m > 200.0)
            .map(|&m| f64::from(m))
            .sum(),
        total: ms.iter().map(|&m| f64::from(m)).sum(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_of_frames_is_summed_by_rank() {
        assert_eq!(summarize(&mut []), None);
        // 1..=100 ms, in any order: the 95th percentile is the 95th frame.
        let mut frames: Vec<f32> = (1..=100).rev().map(|k| k as f32).collect();
        let s = summarize(&mut frames).unwrap();
        assert_eq!(
            (s.frames, s.p50, s.p95, s.p99, s.max),
            (100, 50.0, 95.0, 99.0, 100.0)
        );
        assert_eq!((s.over_100, s.over_200, s.over_500), (0, 0, 0));
        assert_eq!(s.total, 5050.0);
        // A few frames of half a second in a stretch of fast ones: no mean shows them, the rank and the counts do.
        let mut stall: Vec<f32> = vec![33.0; 40];
        stall.extend([250.0, 550.0, 600.0, 200.0, 100.0]);
        let s = summarize(&mut stall).unwrap();
        assert_eq!((s.frames, s.p50, s.max), (45, 33.0, 600.0));
        assert_eq!(
            (s.over_100, s.over_200, s.over_500),
            (4, 3, 2),
            "100 ms is not over 100, 200 is not over 200"
        );
        assert_eq!(s.in_over_200, 1400.0);
        // One frame is all its ranks.
        let s = summarize(&mut [7.0]).unwrap();
        assert_eq!((s.p50, s.p95, s.p99, s.max), (7.0, 7.0, 7.0, 7.0));
    }

    #[test]
    fn frames_are_kept_up_to_the_most_and_the_next_window_starts_empty() {
        let mut t = Times::new();
        for _ in 0..(MOST + 10) {
            t.frame();
        }
        assert_eq!(t.ms.len(), MOST);
        t.line();
        assert!(t.ms.is_empty() && t.pending.is_empty());
    }
}
