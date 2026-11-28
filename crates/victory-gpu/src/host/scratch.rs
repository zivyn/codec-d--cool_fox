//! Tables and lists that the preparing of a submission makes and drops, kept
//! by the thread that makes them from one submission to the next
//! (`VICTORY_LEAN_STEPS=1`).
//!
//! A frame is some fifteen submissions, and each of them makes, over its
//! steps, tables by copy of several thousand entries (21,000 refreshes of
//! 4,500 copies a frame), grows them one doubling at a time, and drops them
//! when it is done: every growth is an allocation, a rehash of what was put in,
//! and, for the tables of hundreds of kilobytes, memory that the system gives
//! pages to and takes back each time. The thread keeps the emptied table (its
//! room, not what it held) for the next submission.
//!
//! Nothing a table holds outlives the submission that filled it: it is
//! cleared before it is kept ([`give`]); what takes it ([`take`]) finds it
//! empty, or, if another use of it is under way on this thread, a new empty
//! one (the one of the use under way is given back when that is over, and
//! the later of two gives is the one kept).

use std::cell::RefCell;
use std::collections::HashMap;
use std::thread::LocalKey;

/// The most entries a kept table or list may have room for: past it the
/// room is let go (a frame with a very wide submission does not leave a big
/// table behind for the rest of the run).
const MOST: usize = 1 << 16;

/// What is kept: emptied, and its room let go if it is past [`MOST`].
pub(super) trait Reset {
    fn reset(&mut self);
}

impl<K, V, S: Default> Reset for HashMap<K, V, S> {
    fn reset(&mut self) {
        if self.capacity() > MOST {
            *self = HashMap::with_hasher(S::default());
        } else {
            self.clear();
        }
    }
}

impl<T> Reset for Vec<T> {
    fn reset(&mut self) {
        if self.capacity() > MOST {
            *self = Vec::new();
        } else {
            self.clear();
        }
    }
}

/// What this thread kept in `key`, or an empty one if it kept none (or has
/// ended: a thread's own storage is gone then).
pub(super) fn take<T: Default>(key: &'static LocalKey<RefCell<T>>) -> T {
    key.try_with(|kept| std::mem::take(&mut *kept.borrow_mut()))
        .unwrap_or_default()
}

/// Keeps `held` (emptied) in `key` for the next [`take`].
pub(super) fn give<T: Reset>(key: &'static LocalKey<RefCell<T>>, mut held: T) {
    held.reset();
    let _ = key.try_with(|kept| *kept.borrow_mut() = held);
}

/// A table by a copy's address that a submission fills while it prepares
/// ([`super::ByCopy`]): `Kept::take(&KEY)` is the one this thread kept, or a
/// new one, and `Kept::give` keeps it for the next. With `lean` off it is a
/// new table that is dropped.
pub(super) struct Lean<T: Default + Reset + 'static> {
    key: &'static LocalKey<RefCell<T>>,
    lean: bool,
    held: Option<T>,
}

impl<T: Default + Reset + 'static> Lean<T> {
    /// The table of `key` (`lean`: the one the thread kept).
    pub(super) fn of(key: &'static LocalKey<RefCell<T>>, lean: bool) -> Lean<T> {
        let held = Some(if lean { take(key) } else { T::default() });
        Lean { key, lean, held }
    }
}

impl<T: Default + Reset + 'static> std::ops::Deref for Lean<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.held.as_ref().expect("held until dropped")
    }
}

impl<T: Default + Reset + 'static> std::ops::DerefMut for Lean<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.held.as_mut().expect("held until dropped")
    }
}

impl<T: Default + Reset + 'static> Drop for Lean<T> {
    fn drop(&mut self) {
        if self.lean
            && let Some(held) = self.held.take()
        {
            give(self.key, held);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::ByCopy;

    thread_local! {
        static TABLE: RefCell<ByCopy<usize>> = RefCell::new(ByCopy::default());
        static LIST: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }

    /// What is kept is empty, and has the room that was made for it.
    #[test]
    fn a_kept_table_is_empty_and_keeps_its_room() {
        let mut table = Lean::of(&TABLE, true);
        assert!(table.is_empty());
        for i in 0..5000 {
            table.insert(i, i * 2);
        }
        let room = table.capacity();
        assert!(room >= 5000);
        drop(table);
        let again = Lean::of(&TABLE, true);
        assert!(
            again.is_empty(),
            "nothing a submission put in is left for the next"
        );
        assert_eq!(again.capacity(), room, "the room is");
    }

    /// With `lean` off nothing is kept: a new table each time, as before.
    #[test]
    fn a_table_that_is_not_lean_is_new_each_time() {
        let mut table = Lean::of(&TABLE, false);
        table.insert(1, 1);
        drop(table);
        let again = Lean::of(&TABLE, false);
        assert!(again.is_empty());
        let mut kept = Lean::of(&TABLE, true);
        kept.insert(7, 7);
        // (A lean one taken after a lean one is given back: what was put in is gone.)
        drop(kept);
        assert!(Lean::of(&TABLE, true).is_empty());
    }

    /// A use inside a use (the lists of another submission prepared by a call from
    /// the first) finds an empty one, and what each puts in stays its own.
    #[test]
    fn a_use_inside_a_use_has_a_table_of_its_own() {
        let mut outer = Lean::of(&LIST, true);
        outer.push(1);
        let mut inner = Lean::of(&LIST, true);
        assert!(inner.is_empty());
        inner.push(2);
        assert_eq!(*outer, [1]);
        assert_eq!(*inner, [2]);
        drop(inner);
        drop(outer);
        assert!(Lean::of(&LIST, true).is_empty());
    }

    /// A table that grew past the most is let go of, not kept.
    #[test]
    fn a_table_past_the_most_is_let_go_of() {
        let mut list = Lean::of(&LIST, true);
        list.extend(0..(MOST as u64 + 10));
        assert!(list.capacity() > MOST);
        drop(list);
        assert!(Lean::of(&LIST, true).capacity() <= MOST);
    }

    /// The tables of one thread are not another's.
    #[test]
    fn each_thread_keeps_its_own() {
        let mut here = Lean::of(&LIST, true);
        here.reserve(1000);
        drop(here);
        let there = std::thread::spawn(|| Lean::of(&LIST, true).capacity())
            .join()
            .unwrap();
        assert_eq!(there, 0);
    }
}
