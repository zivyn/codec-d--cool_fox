//! Which of the title's waits for shader writes a run of commands needs, from
//! what each command reads and writes (`VICTORY_PRECISE_WAITS`, stage 1: the
//! model and its proof; no command is given an access yet).
//!
//! The host turns every barrier group and pipeline sync of the title into a
//! UAV barrier on every resource: about 700 a frame at the window spot, and
//! 316 more in our own clamps. A wait costs the card the drain of what runs
//! before it. Most of them stand between dispatches that touch other bytes
//! (66 instances of one op chain, a skinning pass per character). A wait can
//! be left out when nothing after it reads or writes what something before it
//! wrote, or writes what something before it read: the three hazards a UAV
//! barrier orders.
//!
//! The waits are moved, not dropped: [`Lazy`] holds a title wait until a
//! command conflicts with what stood before it, and the host emits the
//! barrier right before that command (or at the end of the list, where the
//! next list may touch anything). Every command that is not described says
//! [`Access::default`], which is "anywhere" and conflicts with everything that
//! is in front of a wait. What a command touches is [`Span`]s of resources:
//! the writes come from the translator's store footprint
//! (`victory-shader::footprint`, evaluated with the dispatch's extents and the
//! root constants: JOIN2), the reads from the same analysis of loads, and where
//! it knows nothing, the whole of every view bound.

// (Stage 1: the model and its tests; the host's commands do not describe themselves yet.)
#![allow(dead_code)]

/// Bytes `start..end` of one resource (a host copy of a title buffer, by its
/// address in title memory, or a texture by its host address).
pub(crate) type Span = (usize, u64, u64);

/// What a command reads and writes. `None`: anywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Access {
    pub(crate) reads: Option<Vec<Span>>,
    pub(crate) writes: Option<Vec<Span>>,
}

impl Default for Access {
    /// Not described: reads and writes anywhere.
    fn default() -> Self {
        Access {
            reads: None,
            writes: None,
        }
    }
}

impl Access {
    /// Reads and writes of known spans.
    pub(crate) fn of(reads: Vec<Span>, writes: Vec<Span>) -> Self {
        Access {
            reads: Some(reads),
            writes: Some(writes),
        }
    }
}

/// The most spans a set keeps before it is "anywhere".
const MOST: usize = 256;

/// Whether two sets share a byte. `None` meets everything but the empty set.
fn meet(a: &Option<Vec<Span>>, b: &Option<Vec<Span>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (None, Some(s)) | (Some(s), None) => !s.is_empty(),
        (Some(a), Some(b)) => a
            .iter()
            .any(|x| b.iter().any(|y| x.0 == y.0 && x.1 < y.2 && y.1 < x.2)),
    }
}

/// A set of spans, or anywhere.
fn join(set: &mut Option<Vec<Span>>, more: &Option<Vec<Span>>) {
    match (set.as_mut(), more) {
        (Some(have), Some(more)) => {
            have.extend(more.iter().copied());
            if have.len() > MOST {
                *set = None;
            }
        }
        (None, _) => {}
        (Some(_), None) => *set = None,
    }
}

/// What a group of commands touched.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Touched {
    reads: Option<Vec<Span>>,
    writes: Option<Vec<Span>>,
}

impl Touched {
    fn none() -> Self {
        Touched {
            reads: Some(Vec::new()),
            writes: Some(Vec::new()),
        }
    }

    fn is_empty(&self) -> bool {
        self.reads.as_ref().is_some_and(Vec::is_empty)
            && self.writes.as_ref().is_some_and(Vec::is_empty)
    }

    fn add(&mut self, a: &Access) {
        join(&mut self.reads, &a.reads);
        join(&mut self.writes, &a.writes);
    }

    fn merge(&mut self, other: Touched) {
        join(&mut self.reads, &other.reads);
        join(&mut self.writes, &other.writes);
    }

    /// A read after a write, a write after a write, a write after a read.
    fn conflicts(&self, next: &Access) -> bool {
        meet(&self.writes, &next.reads)
            || meet(&self.writes, &next.writes)
            || meet(&self.reads, &next.writes)
    }
}

/// The title's waits for shader writes, held until something needs them.
#[derive(Debug)]
pub(crate) struct Lazy {
    /// What stood before a title wait and is not ordered yet.
    before: Touched,
    /// What came after the last title wait.
    after: Touched,
    /// No title wait yet in this list: what earlier lists left in the air is
    /// not known.
    fresh: bool,
    /// Waits emitted and waits of the title left out (for the line).
    pub(crate) emitted: u64,
    pub(crate) held: u64,
}

impl Lazy {
    /// The start of a list.
    pub(crate) fn new() -> Self {
        Lazy {
            before: Touched::none(),
            after: Touched::none(),
            fresh: true,
            emitted: 0,
            held: 0,
        }
    }

    /// The title waits for every earlier shader write.
    pub(crate) fn title_wait(&mut self) {
        if self.fresh {
            // What earlier lists may still be writing is ordered by this wait too.
            self.before.add(&Access::default());
            self.fresh = false;
        }
        let after = std::mem::replace(&mut self.after, Touched::none());
        self.before.merge(after);
        self.held += 1;
    }

    /// A command that touches `next` is recorded: whether a wait goes in front
    /// of it.
    pub(crate) fn command(&mut self, next: &Access) -> bool {
        let emit = self.before.conflicts(next);
        if emit {
            // The wait orders everything recorded before it, `after` too.
            self.before = Touched::none();
            self.after = Touched::none();
            self.emitted += 1;
        }
        self.after.add(next);
        emit
    }

    /// A command that is not described is recorded.
    pub(crate) fn opaque(&mut self) -> bool {
        self.command(&Access::default())
    }

    /// The list closes: whether a wait goes at its end (a title wait that
    /// nothing in the list needed may be needed by the lists after it).
    pub(crate) fn close(&mut self) -> bool {
        let emit = !self.before.is_empty();
        if emit {
            self.before = Touched::none();
            self.emitted += 1;
        }
        emit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(resource: usize, from: u64, to: u64) -> Access {
        Access::of(Vec::new(), vec![(resource, from, to)])
    }

    fn r(resource: usize, from: u64, to: u64) -> Access {
        Access::of(vec![(resource, from, to)], Vec::new())
    }

    /// A chain of dependent dispatches keeps every wait, independent ones
    /// need none.
    #[test]
    fn dependent_commands_keep_their_waits_and_independent_ones_lose_them() {
        // Each step reads what the one before wrote (an op chain).
        let mut lazy = Lazy::new();
        let mut emitted = 0;
        for step in 0..6usize {
            lazy.title_wait();
            let access = Access::of(
                vec![(1, step as u64 * 64, step as u64 * 64 + 64)],
                vec![(1, step as u64 * 64 + 64, step as u64 * 64 + 128)],
            );
            emitted += usize::from(lazy.command(&access));
        }
        // The first wait is for what earlier lists left (anywhere): it is
        // emitted before the first command; the rest are needed by the chain.
        assert_eq!(emitted, 6);
        // Independent instances: the same op on other bytes of the arena.
        let mut lazy = Lazy::new();
        let mut emitted = 0;
        for instance in 0..6usize {
            lazy.title_wait();
            let at = instance as u64 * 1000;
            emitted += usize::from(lazy.command(&Access::of(
                vec![(1, at, at + 64)],
                vec![(1, at + 64, at + 128)],
            )));
        }
        assert_eq!(emitted, 1, "only the one for what earlier lists left");
        assert_eq!(lazy.held, 6);
        // A wait of the title is still standing: the next list gets it.
        assert!(lazy.close());
        assert_eq!(lazy.emitted, 2);
    }

    #[test]
    fn each_hazard_needs_the_wait() {
        let cases = [
            (w(1, 0, 64), r(1, 32, 96), true, "read after write"),
            (w(1, 0, 64), w(1, 32, 96), true, "write after write"),
            (r(1, 0, 64), w(1, 32, 96), true, "write after read"),
            (r(1, 0, 64), r(1, 0, 64), false, "read after read"),
            (w(1, 0, 64), r(1, 64, 128), false, "next to it"),
            (w(1, 0, 64), r(2, 0, 64), false, "another resource"),
            (
                w(1, 0, 64),
                Access::default(),
                true,
                "anywhere after a write",
            ),
            (
                Access::default(),
                r(1, 0, 64),
                true,
                "a read after anywhere",
            ),
            (
                w(1, 0, 64),
                Access::of(Vec::new(), Vec::new()),
                false,
                "touches nothing",
            ),
        ];
        for (first, second, needs, what) in cases {
            let mut lazy = Lazy::new();
            // The first wait of a list is held for what earlier lists left in
            // the air: the first command that touches anything gets it.
            lazy.title_wait();
            assert!(lazy.command(&first), "{what}: the wait for earlier lists");
            // From here on the pair decides.
            lazy.title_wait();
            assert_eq!(lazy.command(&second), needs, "{what}");
        }
    }

    #[test]
    fn a_wait_nothing_needs_is_still_given_at_the_end_of_the_list() {
        let mut lazy = Lazy::new();
        lazy.title_wait();
        lazy.command(&w(1, 0, 64)); // emitted: earlier lists
        lazy.title_wait();
        assert!(!lazy.command(&w(1, 4096, 4160)));
        // The wait stands before a write that something after the list may read.
        assert!(lazy.close());
        assert!(!lazy.close(), "once");
    }

    /// Whatever a title does, every pair of commands with a title wait
    /// between them that conflict has an emitted wait between them, and a wait
    /// is only emitted after a title wait.
    #[test]
    fn no_hazard_the_title_waited_for_is_left_unordered() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let spans = |next: &mut dyn FnMut(u64) -> u64| -> Option<Vec<Span>> {
            if next(12) == 0 {
                return None;
            }
            let count = next(3);
            Some(
                (0..count)
                    .map(|_| {
                        let start = next(8) * 64;
                        (next(3) as usize, start, start + 64 * (1 + next(2)))
                    })
                    .collect(),
            )
        };
        let mut conflicting_pairs = 0;
        for _ in 0..300 {
            let mut lazy = Lazy::new();
            // (Commands, and for each the number of title waits before it and
            // whether a wait was emitted before it.)
            let mut commands: Vec<(Access, u64, bool)> = Vec::new();
            let mut waits = 0u64;
            let mut waits_since_emit = 0u64;
            for _ in 0..40 {
                if next(3) == 0 {
                    lazy.title_wait();
                    waits += 1;
                    waits_since_emit += 1;
                } else {
                    let access = Access {
                        reads: spans(&mut next),
                        writes: spans(&mut next),
                    };
                    let emit = lazy.command(&access);
                    if emit {
                        assert!(
                            waits_since_emit > 0,
                            "a wait is only emitted for a wait of the title"
                        );
                        waits_since_emit = 0;
                    }
                    commands.push((access, waits, emit));
                }
            }
            for j in 1..commands.len() {
                for i in 0..j {
                    let (a, wi, _) = &commands[i];
                    let (b, wj, _) = &commands[j];
                    if wj == wi {
                        continue; // no title wait between them
                    }
                    let hazard = meet(&a.writes, &b.reads)
                        || meet(&a.writes, &b.writes)
                        || meet(&a.reads, &b.writes);
                    if !hazard {
                        continue;
                    }
                    conflicting_pairs += 1;
                    assert!(
                        commands[i + 1..=j].iter().any(|c| c.2),
                        "commands {i} and {j} conflict with a title wait between them and no wait was emitted between them"
                    );
                }
            }
        }
        assert!(
            conflicting_pairs > 1000,
            "the test sees hazards ({conflicting_pairs})"
        );
    }

    /// With spans that never meet, nothing but the first wait is emitted.
    #[test]
    fn independent_work_is_waited_for_once() {
        let mut lazy = Lazy::new();
        let mut emitted = 0;
        for k in 0..100u64 {
            lazy.title_wait();
            emitted += u64::from(lazy.command(&Access::of(
                vec![(k as usize, 0, 64)],
                vec![(k as usize, 64, 128)],
            )));
        }
        assert_eq!(emitted, 1);
        assert_eq!(lazy.held, 100);
    }
}
