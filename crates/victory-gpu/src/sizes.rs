//! The compacted sizes the title reads from the structure builder
//! (`VICTORY_BVH_SIZES`, stacked on `VICTORY_BVH_BUILDER=1`).
//!
//! The title asks the builder for a new structure's compacted size (a
//! program of one thread stores it into a slot of a small title buffer), reads
//! the slot a frame or two later without looking whether it is there, takes
//! memory of that size and has the structure copied into it. On the console
//! the card has finished by then. Here the submission lands up to two frames
//! late, the title reads what the slot held before (some other structure's
//! size) and the copy runs over the neighbours: 8 of 65 dynamic structures
//! were whole in run light-10, and `VICTORY_BVH_SIZES_WAIT=1` did not mend it.
//!
//! The title's own slot is the only place the number can be made right before
//! it is read, so it is written here, in title memory, when the ask is
//! recorded:
//!
//! * `predict`: the size the structure had when it was last asked for. A
//!   structure the title builds every frame (a skinned mesh, refitted) has the
//!   same number of triangles and so the same size: the first ask of a
//!   structure learns it (from the slot, once the card has landed it), every
//!   later one is answered at once. What the card lands afterwards is the same
//!   number; if it is not (the title changed the structure) it is learned, and
//!   the next ask is answered with it, never with less than a sixteenth under
//!   what was seen ([`Book::learn`]). A landing of an older submission can put
//!   the slot's old content back before the title reads it: the prediction is
//!   written again after every landing ([`landed`]) while the slot still has
//!   what it had before the ask.
//! * `wait`: the submission that asks is waited for and landed before the title
//!   goes on (`VICTORY_BVH_SIZES_WAIT=1` of old): exact, and a card wait on every
//!   frame.
//! * `both`: `predict`, and `wait` for a structure that has no history yet.
//!
//! What it counts ([`Stats`], a line every ten seconds in the log) tells which
//! of the guesses about the title is right: asks answered, asks that could not
//! be, predictions the card's number disagreed with (`under`: the card's number
//! was bigger, which is an overlap if the title read the prediction), asks
//! waited for whose slot had changed once they landed (`wait_landed`) and
//! those whose slot had not (`wait_unchanged`: the wait does not reach what the
//! title reads, and the slot is read from somewhere else or at another time).

use std::collections::HashMap;

/// What is done about the sizes (`VICTORY_BVH_SIZES`: `off`, `wait`,
/// `predict`, `both`; `VICTORY_BVH_SIZES_WAIT=1` is `wait`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    Wait,
    Predict,
    Both,
}

impl Mode {
    pub(crate) fn parse(sizes: Option<&str>, wait: bool) -> Self {
        match sizes.map(str::trim) {
            Some("wait") => Self::Wait,
            Some("predict") => Self::Predict,
            Some("both") => Self::Both,
            Some("off") => Self::Off,
            _ if wait => Self::Wait,
            _ => Self::Off,
        }
    }

    fn predicts(self) -> bool {
        matches!(self, Self::Predict | Self::Both)
    }

    /// Whether an ask for a structure with (`known`) or without a history is
    /// waited for.
    fn waits(self, known: bool) -> bool {
        match self {
            Self::Off | Self::Predict => false,
            Self::Wait => true,
            Self::Both => !known,
        }
    }

    pub(crate) fn on(self) -> bool {
        self != Self::Off
    }
}

/// The slots of a small title buffer: dwords the card writes and the title
/// reads.
pub(crate) trait Slots {
    fn read(&self, at: u64) -> u64;
    fn write(&mut self, at: u64, value: u64);
}

/// What the book counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    /// Asks, those answered with a prediction, those not.
    pub asks: u64,
    pub predicted: u64,
    pub unknown: u64,
    /// A slot given its prediction again after a landing put the old content back.
    pub reasserted: u64,
    /// Predictions the card's number disagreed with: bigger (an overlap for
    /// the title if it read the prediction), smaller.
    pub under: u64,
    pub over: u64,
    /// Asks waited for: the slot had changed by the next look, had not.
    pub wait_landed: u64,
    pub wait_unchanged: u64,
    /// Numbers learned, numbers refused (zero, or not a size).
    pub learned: u64,
    pub refused: u64,
}

struct Ask {
    structure: u64,
    slot: u64,
    frame: u64,
    /// What the slot held before the ask.
    was: u64,
    predicted: Option<u64>,
    waited: bool,
}

/// The sizes of the structures asked for, and the asks not yet landed.
#[derive(Default)]
pub(crate) struct Book {
    sizes: HashMap<u64, u64>,
    pending: Vec<Ask>,
    pub(crate) stats: Stats,
}

/// Asks older than this many frames (landed or not) are let go.
const STALE: u64 = 60;
/// A prediction that still shows after so many frames needs no more care.
const HELD: u64 = 6;
/// The most structures remembered.
const MOST: usize = 1 << 16;
/// The biggest number taken for a size (bytes or nodes: 1 TiB).
const BIGGEST: u64 = 1 << 40;

impl Book {
    /// The size remembered of `structure`.
    pub(crate) fn known(&self, structure: u64) -> Option<u64> {
        self.sizes.get(&structure).copied()
    }

    /// The title's builder asks for the compacted size of `structure` into
    /// `slot` (in `frame`): with a prediction the slot is written now. Whether
    /// the submission that asks is to be waited for.
    pub(crate) fn asked(
        &mut self,
        mode: Mode,
        frame: u64,
        structure: u64,
        slot: u64,
        memory: &mut impl Slots,
    ) -> bool {
        self.stats.asks += 1;
        let was = memory.read(slot);
        self.pending.retain(|a| a.slot != slot);
        let predicted = mode.predicts().then(|| self.known(structure)).flatten();
        match predicted {
            Some(size) => {
                memory.write(slot, size);
                self.stats.predicted += 1;
            }
            None => self.stats.unknown += 1,
        }
        let waited = mode.waits(predicted.is_some());
        self.pending.push(Ask {
            structure,
            slot,
            frame,
            was,
            predicted,
            waited,
        });
        waited
    }

    /// What the card landed in the slots asked about: learned; and a
    /// prediction a landing of something older put out of the slot written again.
    pub(crate) fn look(&mut self, frame: u64, memory: &mut impl Slots) {
        let pending = std::mem::take(&mut self.pending);
        for ask in pending {
            let age = frame.saturating_sub(ask.frame);
            let now = memory.read(ask.slot);
            let keep = match ask.predicted {
                Some(size) if now == size => age <= HELD,
                Some(size) if now == ask.was => {
                    memory.write(ask.slot, size);
                    self.stats.reasserted += 1;
                    age <= HELD
                }
                Some(size) => {
                    self.learn(ask.structure, now, Some(size));
                    false
                }
                None if now != ask.was => {
                    if ask.waited {
                        self.stats.wait_landed += 1;
                    }
                    self.learn(ask.structure, now, None);
                    false
                }
                None if ask.waited && age >= 1 => {
                    self.stats.wait_unchanged += 1;
                    false
                }
                None => age <= STALE,
            };
            if keep {
                self.pending.push(ask);
            }
        }
    }

    /// `value` is what the card says the size of `structure` is; `predicted`
    /// what was written for it. Never less than a sixteenth under what was
    /// predicted: a size that went down is believed slowly, one that went up at once.
    fn learn(&mut self, structure: u64, value: u64, predicted: Option<u64>) {
        if value == 0 || value > BIGGEST {
            self.stats.refused += 1;
            return;
        }
        self.stats.learned += 1;
        let size = match predicted {
            Some(p) if value > p => {
                self.stats.under += 1;
                value
            }
            Some(p) if value < p => {
                self.stats.over += 1;
                value.max(p - p / 16)
            }
            _ => value,
        };
        if self.sizes.len() >= MOST {
            self.sizes.clear();
        }
        self.sizes.insert(structure, size);
    }
}

#[cfg(windows)]
mod real {
    use super::*;
    use std::sync::Mutex;

    /// Title memory, where the slots are.
    struct Title;

    impl Slots for Title {
        fn read(&self, at: u64) -> u64 {
            let mut b = [0u8; 8];
            crate::host::copy_mapped(b.as_mut_ptr(), at as *const u8, 8, false);
            u64::from_le_bytes(b)
        }

        fn write(&mut self, at: u64, value: u64) {
            let b = value.to_le_bytes();
            crate::host::copy_mapped(at as *mut u8, b.as_ptr(), 8, true);
        }
    }

    static BOOK: Mutex<Option<Book>> = Mutex::new(None);

    /// `VICTORY_BVH_SIZES`.
    pub(crate) fn mode() -> Mode {
        static MODE: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
        *MODE.get_or_init(|| {
            let mode = Mode::parse(
                std::env::var("VICTORY_BVH_SIZES").ok().as_deref(),
                std::env::var_os("VICTORY_BVH_SIZES_WAIT").is_some_and(|v| v == "1"),
            );
            if mode.on() {
                tracing::info!(?mode, "the compacted sizes the title reads from the structure builder are handled (VICTORY_BVH_SIZES)");
            }
            mode
        })
    }

    /// An ask of the builder for a structure's compacted size; whether the
    /// submission is waited for.
    pub(crate) fn asked(frame: u64, structure: u64, slot: u64) -> bool {
        let mode = mode();
        let mut book = BOOK.lock().unwrap_or_else(|e| e.into_inner());
        book.get_or_insert_with(Book::default)
            .asked(mode, frame, structure, slot, &mut Title)
    }

    /// At the start of a frame and after what the card wrote came back.
    pub(crate) fn look(frame: u64) {
        if !mode().on() {
            return;
        }
        if let Some(book) = BOOK.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            book.look(frame, &mut Title);
        }
    }

    /// What the card wrote has come back to title memory.
    pub(crate) fn landed() {
        look(crate::frame::current());
    }

    /// The counts, as a line of the log (every ten seconds from the memory's own report).
    pub(crate) fn report() {
        if !mode().on() {
            return;
        }
        if let Some(book) = BOOK.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let s = book.stats;
            tracing::info!(
                structures = book.sizes.len(),
                pending = book.pending.len(),
                asks = s.asks,
                predicted = s.predicted,
                unknown = s.unknown,
                reasserted = s.reasserted,
                under = s.under,
                over = s.over,
                wait_landed = s.wait_landed,
                wait_unchanged = s.wait_unchanged,
                learned = s.learned,
                refused = s.refused,
                "title memory by address: the compacted sizes"
            );
        }
    }
}

#[cfg(windows)]
pub(crate) use real::{asked, landed, look, mode, report};

#[cfg(test)]
mod tests {
    use super::*;
    use victory_shader::bvh_build::{self, Options};

    /// Title memory as a map; what the card lands waits for its frame.
    #[derive(Default)]
    struct Model {
        memory: HashMap<u64, u64>,
    }

    impl Slots for Model {
        fn read(&self, at: u64) -> u64 {
            self.memory.get(&at).copied().unwrap_or(0)
        }
        fn write(&mut self, at: u64, value: u64) {
            self.memory.insert(at, value);
        }
    }

    #[test]
    fn the_mode_is_read_from_its_two_switches() {
        assert_eq!(Mode::parse(None, false), Mode::Off);
        assert_eq!(Mode::parse(None, true), Mode::Wait);
        assert_eq!(Mode::parse(Some("predict"), false), Mode::Predict);
        assert_eq!(Mode::parse(Some(" both "), false), Mode::Both);
        assert_eq!(Mode::parse(Some("off"), true), Mode::Off);
        assert_eq!(Mode::parse(Some("nonsense"), false), Mode::Off);
        assert!(Mode::Wait.waits(true) && Mode::Both.waits(false) && !Mode::Both.waits(true));
        assert!(!Mode::Predict.waits(false) && !Mode::Off.waits(false));
    }

    #[test]
    fn the_first_ask_is_learned_from_the_card_and_the_next_one_answered() {
        let mut book = Book::default();
        let mut m = Model::default();
        m.write(0x100, 0xdead_0000);
        assert!(!book.asked(Mode::Predict, 1, 7, 0x100, &mut m));
        assert_eq!(m.read(0x100), 0xdead_0000, "nothing to predict from yet");
        // The card lands the number.
        m.write(0x100, 640);
        book.look(3, &mut m);
        assert_eq!(book.known(7), Some(640));
        // The next ask, into another slot that holds some other number.
        m.write(0x140, 999);
        book.asked(Mode::Predict, 4, 7, 0x140, &mut m);
        assert_eq!(m.read(0x140), 640);
        assert_eq!(book.stats.predicted, 1);
    }

    #[test]
    fn a_landing_that_puts_the_old_content_back_is_answered_by_writing_the_prediction_again() {
        let mut book = Book::default();
        let mut m = Model::default();
        book.sizes.insert(7, 640);
        m.write(0x100, 111);
        book.asked(Mode::Predict, 1, 7, 0x100, &mut m);
        assert_eq!(m.read(0x100), 640);
        m.write(0x100, 111);
        book.look(1, &mut m);
        assert_eq!(m.read(0x100), 640);
        assert_eq!(book.stats.reasserted, 1);
        // The card's own number lands, and is the number: nothing is written over it.
        m.write(0x100, 640);
        book.look(2, &mut m);
        assert_eq!(book.stats.reasserted, 1);
        assert_eq!(book.stats.under + book.stats.over, 0);
    }

    #[test]
    fn a_bigger_size_is_believed_at_once_a_smaller_one_slowly() {
        let mut book = Book::default();
        let mut m = Model::default();
        book.sizes.insert(7, 640);
        m.write(0x100, 1);
        book.asked(Mode::Predict, 1, 7, 0x100, &mut m);
        m.write(0x100, 1280);
        book.look(2, &mut m);
        assert_eq!((book.known(7), book.stats.under), (Some(1280), 1));
        m.write(0x100, 1);
        book.asked(Mode::Predict, 3, 7, 0x100, &mut m);
        m.write(0x100, 64);
        book.look(4, &mut m);
        assert_eq!((book.known(7), book.stats.over), (Some(1280 - 80), 1));
    }

    #[test]
    fn a_wait_that_does_not_change_the_slot_is_counted() {
        let mut book = Book::default();
        let mut m = Model::default();
        m.write(0x100, 5);
        assert!(book.asked(Mode::Wait, 1, 7, 0x100, &mut m));
        book.look(2, &mut m);
        assert_eq!((book.stats.wait_unchanged, book.stats.wait_landed), (1, 0));
        assert!(book.asked(Mode::Wait, 3, 7, 0x100, &mut m));
        m.write(0x100, 64);
        book.look(4, &mut m);
        assert_eq!((book.stats.wait_unchanged, book.stats.wait_landed), (1, 1));
        // Zero is not a size.
        assert!(book.asked(Mode::Wait, 5, 8, 0x140, &mut m));
        m.write(0x140, 0);
        book.look(6, &mut m);
        assert_eq!(book.known(8), None);
    }

    // --- a title and a card, in frames --------------------------------

    /// The policies of the model.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Policy {
        Off,
        Mode(Mode),
    }

    /// An object the title builds every frame: its triangles, and so the
    /// compacted size the builder gives it.
    struct Object {
        id: u64,
        size: usize,
        triangles: Vec<bvh_build::Triangle>,
    }

    fn object(id: u64, count: usize) -> Object {
        let f = |k: u64| (((k * 2654435761 + id * 7919) >> 6) % 1000) as f32 / 50.0;
        let triangles: Vec<_> = (0..count as u64)
            .map(|i| {
                let c = [f(i * 3), f(i * 3 + 1), f(i * 3 + 2)];
                [c, [c[0] + 1.0, c[1], c[2]], [c[0], c[1] + 1.0, c[2] + 0.5]]
            })
            .collect();
        let size = bvh_build::build(&triangles, Options::default()).nodes() * 64;
        Object {
            id,
            size,
            triangles,
        }
    }

    /// Runs `frames` frames: each one every live object is built (its asked
    /// size lands `lag` frames after, or at once with a wait), the title reads
    /// the slot `read` frames after the ask, takes memory of that size from a
    /// bump allocator and has the structure copied there. A new object comes
    /// every `arrival` frames. Returns the structures that are whole in the
    /// memory at the end and how many there are.
    fn run(policy: Policy, lag: u64, read: u64, arrival: u64) -> (usize, usize, Stats) {
        let mut book = Book::default();
        let mut slots = Model::default();
        let mut live: Vec<Object> = Vec::new();
        // (frame it lands, slot, real size)
        let mut landing: Vec<(u64, u64, u64)> = Vec::new();
        // (frame it is read, slot, object index)
        let mut reading: Vec<(u64, u64, usize)> = Vec::new();
        let mut memory: Vec<u32> = Vec::new();
        let mut cursor = 0usize;
        let mut placed: Vec<(usize, usize)> = Vec::new();
        let mut next_slot = 0u64;
        // More slots than asks in the air (so a landing never puts the
        // content of an older ask over a newer one), and prime, so that
        // what a slot held before is some other object's size.
        const SLOTS: u64 = 61;
        for frame in 0..40u64 {
            if frame % arrival == 0 && live.len() < 8 {
                let n = 12 + 9 * live.len();
                live.push(object(live.len() as u64, n));
            }
            // What the card lands this frame comes first, then the title's reads.
            let mut still = Vec::new();
            for l in landing.drain(..) {
                if l.0 <= frame {
                    slots.write(l.1, l.2);
                } else {
                    still.push(l);
                }
            }
            landing = still;
            if let Policy::Mode(_) = policy {
                book.look(frame, &mut slots);
            }
            let mut later = Vec::new();
            for r in reading.drain(..) {
                if r.0 > frame {
                    later.push(r);
                    continue;
                }
                let (_, slot, index) = r;
                let o = &live[index];
                let allocated = (slots.read(slot) as usize).max(64);
                let at = cursor / 64;
                cursor += allocated;
                // The structure is copied in: all of its real size, whatever was allocated.
                let built = bvh_build::build(&o.triangles, Options::default());
                assert_eq!(built.nodes() * 64, o.size);
                let end = (at * 64 + o.size).max(cursor);
                if memory.len() < end / 4 {
                    memory.resize(end / 4, 0);
                }
                memory[at * 16..at * 16 + built.words.len()].copy_from_slice(&built.words);
                placed.push((at, built.root as usize));
            }
            reading = later;
            for (index, o) in live.iter().enumerate() {
                let slot = 0x1000 + 64 * (next_slot % SLOTS);
                next_slot += 1;
                let land_at = match policy {
                    Policy::Off => frame + lag,
                    Policy::Mode(mode) => {
                        let waits = book.asked(mode, frame, o.id, slot, &mut slots);
                        if waits { frame } else { frame + lag }
                    }
                };
                if land_at <= frame {
                    slots.write(slot, o.size as u64);
                } else {
                    landing.push((land_at, slot, o.size as u64));
                }
                reading.push((frame + read, slot, index));
            }
        }
        // Whole: from each place the structure's own header says it is one and passes the check.
        let mut whole = 0;
        for &(at, root) in &placed {
            let Some(h) = bvh_build::Header::read(&memory[at * 16..]) else {
                continue;
            };
            let words = &memory[at * 16..(at * 16 + h.nodes * 16).min(memory.len())];
            if words.len() == h.nodes * 16
                && bvh_build::check_strict(words, root as u32)
                    .is_ok_and(|r| r.reached + 1 == r.size)
            {
                whole += 1;
            }
        }
        (whole, placed.len(), book.stats)
    }

    #[test]
    fn with_the_card_late_and_no_handling_most_structures_are_written_over() {
        // The card lands two frames after the ask, the title reads one frame after.
        let (whole, all, _) = run(Policy::Off, 2, 1, 4);
        assert!(all >= 100, "{all}");
        assert!(whole * 2 < all, "{whole} of {all} whole without handling");
    }

    #[test]
    fn a_wait_makes_every_structure_whole() {
        for mode in [Mode::Wait, Mode::Both] {
            let (whole, all, stats) = run(Policy::Mode(mode), 2, 1, 4);
            assert_eq!(whole, all, "{mode:?}: {stats:?}");
        }
    }

    #[test]
    fn prediction_answers_everything_but_the_first_ask_of_each_structure() {
        let (off_whole, off_all, _) = run(Policy::Off, 2, 1, 4);
        let (whole, all, stats) = run(Policy::Mode(Mode::Predict), 2, 1, 4);
        // Eight objects, each unknown once: its first ask is read stale, and
        // the title reads one frame after, before the card has landed.
        assert!(all - whole <= 2 * 8, "{whole} of {all}: {stats:?}");
        assert!(
            whole * off_all > off_whole * all,
            "{whole}/{all} against {off_whole}/{off_all}"
        );
        assert!(stats.predicted > stats.unknown * 3, "{stats:?}");
        assert_eq!(stats.under, 0, "{stats:?}");
    }

    #[test]
    fn prediction_survives_a_card_that_lands_at_any_lag_or_read_time() {
        for lag in 0..4 {
            for read in 0..4 {
                let (whole, all, stats) = run(Policy::Mode(Mode::Both), lag, read, 3);
                assert_eq!(whole, all, "lag {lag}, read {read}: {stats:?}");
            }
        }
    }
}
