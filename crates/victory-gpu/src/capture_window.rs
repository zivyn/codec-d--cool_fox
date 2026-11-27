//! The window a capture trigger records (`crate::capture`): every operation
//! of every list of every queue from the present after the trigger until a
//! later present, so that one trigger yields one whole frame whichever
//! thread recorded which list. The title records a frame's lists ahead of
//! its present, on several threads; the number of presents that had run
//! when an operation was recorded (the old way of naming a frame) split a
//! frame's operations between files, and the captures came out partial.
//!
//! This part has no device and no files: the arming, the counting and the
//! judgement of whether the window holds a whole frame, so that they are
//! tested with made-up streams of operations.

use std::collections::HashSet;

/// What the window judged of what it recorded.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Complete,
    /// Why it looks partial.
    Partial(String),
}

/// The operations seen between two presents.
#[derive(Debug)]
pub(crate) struct Window {
    /// The first and the last present number counted (operations are
    /// recorded for `presented() + 1`, so the first is the trigger's present + 1).
    pub(crate) start: u64,
    pub(crate) end: u64,
    /// Presents one more try adds.
    width: u64,
    pub(crate) retries: u32,
    pub(crate) ops: u64,
    lists: HashSet<usize>,
    /// A bit for each queue kind a submission named.
    queues: u32,
    /// The passes (text of their hash) that must have run, and whether each did.
    required: Vec<(String, bool)>,
}

impl Window {
    /// Armed at the present `present`: counts from the next, for `width` presents.
    pub(crate) fn armed(present: u64, width: u64, required: &[String]) -> Window {
        let width = width.max(2);
        Window {
            start: present + 1,
            end: present + width,
            width,
            retries: 0,
            ops: 0,
            lists: HashSet::new(),
            queues: 0,
            required: required.iter().map(|r| (r.clone(), false)).collect(),
        }
    }

    /// Whether operations recorded for present `n` are counted.
    #[allow(dead_code)] // the capture reads the range from atomics; the tests use this
    pub(crate) fn contains(&self, n: u64) -> bool {
        (self.start..=self.end).contains(&n)
    }

    /// One operation: `list` is the list that records it, `text` what it did.
    pub(crate) fn note(&mut self, list: usize, text: &str) {
        self.ops += 1;
        if list != 0 {
            self.lists.insert(list);
        }
        if let Some(at) = text.find("queue kind ") {
            let digits: String = text[at + 11..].chars().take_while(char::is_ascii_digit).collect();
            if let Ok(kind) = digits.parse::<u32>() {
                self.queues |= 1 << kind.min(31);
            }
        }
        for (hash, found) in &mut self.required {
            if !*found && text.contains(hash.as_str()) {
                *found = true;
            }
        }
    }

    pub(crate) fn lists(&self) -> usize {
        self.lists.len()
    }

    pub(crate) fn queues(&self) -> u32 {
        self.queues.count_ones()
    }

    /// Whole enough: at least `min_ops` operations and every required pass.
    pub(crate) fn verdict(&self, min_ops: u64) -> Verdict {
        let mut why = Vec::new();
        if self.ops < min_ops {
            why.push(format!("{} operations, under {min_ops}", self.ops));
        }
        for (hash, found) in &self.required {
            if !found {
                why.push(format!("pass {hash} not seen"));
            }
        }
        if why.is_empty() { Verdict::Complete } else { Verdict::Partial(why.join("; ")) }
    }

    /// Another try: the window goes on for as many presents again, and
    /// keeps what it counted.
    pub(crate) fn extend(&mut self) {
        self.end += self.width;
        self.retries += 1;
    }

    /// The line for the log.
    pub(crate) fn summary(&self) -> String {
        format!(
            "{} ops, {} lists, {} queues (presents {}..={}, {} extensions)",
            self.ops,
            self.lists(),
            self.queues(),
            self.start,
            self.end,
            self.retries
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required() -> Vec<String> {
        vec!["6318f669".to_owned()]
    }

    #[test]
    fn the_window_starts_at_the_next_present_and_lasts_its_width() {
        let w = Window::armed(100, 4, &required());
        assert!(!w.contains(100), "the present of the trigger is already running");
        assert!(w.contains(101) && w.contains(104));
        assert!(!w.contains(105));
        assert_eq!((w.start, w.end), (101, 104));
    }

    #[test]
    fn a_stream_without_the_grade_pass_or_enough_operations_is_partial() {
        let mut w = Window::armed(10, 4, &required());
        for i in 0..50 {
            w.note(0x1000 + i % 3, "dispatch 1x1x1 [compute 1111111111111111]");
        }
        w.note(0x2000, "SUBMIT queue kind 0 lists [\"0x1000\"] to the line");
        w.note(0x2001, "SUBMIT queue kind 2 lists [\"0x1001\"] to the line");
        assert_eq!((w.lists(), w.queues()), (5, 2));
        match w.verdict(40) {
            Verdict::Partial(why) => assert!(why.contains("6318f669"), "{why}"),
            Verdict::Complete => panic!("the grade pass was never seen"),
        }
        w.note(0x1002, "draw 3x1 [vertex 7affbd5bbaa830f3, pixel 6318f669e4f17d65] targets []");
        assert_eq!(w.verdict(40), Verdict::Complete);
        match w.verdict(5000) {
            Verdict::Partial(why) => assert!(why.contains("operations"), "{why}"),
            Verdict::Complete => panic!("too few operations"),
        }
    }

    #[test]
    fn an_extension_keeps_the_count_and_widens_the_window() {
        let mut w = Window::armed(10, 4, &required());
        w.note(1, "copy");
        w.extend();
        assert_eq!((w.start, w.end, w.retries), (11, 18, 1));
        assert!(w.contains(18) && !w.contains(19));
        assert_eq!(w.ops, 1);
        assert!(w.summary().contains("1 ops") && w.summary().contains("1 extensions"));
    }

    #[test]
    fn a_window_is_at_least_two_presents_wide() {
        let w = Window::armed(7, 0, &[]);
        assert_eq!((w.start, w.end), (8, 9));
        assert_eq!(w.verdict(0), Verdict::Complete);
    }
}
