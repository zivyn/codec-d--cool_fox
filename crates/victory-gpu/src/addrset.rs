//! The addresses at which the title sees one 64 KiB page.
//!
//! Most pages are shown once or twice, but the title maps one page of its
//! pool at every address of a tiled heap that has nothing in it yet (a page
//! of zeros, thousands of times), and each address was a `Vec` entry that a
//! page's unmapping looked through to take out: the unmapping of n addresses
//! of one page cost n squared, with the lock of the views held (a call of
//! 467 ms was seen in the title's page mappings). A few addresses stay in a
//! `Vec`, in the order they came; more are a set.

use std::collections::HashSet;

/// Kept as a `Vec` up to this many.
const FEW: usize = 16;

#[derive(Clone, Debug)]
pub enum AddressSet {
    Few(Vec<usize>),
    Many(HashSet<usize>),
}

impl Default for AddressSet {
    fn default() -> Self {
        AddressSet::Few(Vec::new())
    }
}

impl AddressSet {
    pub fn push(&mut self, at: usize) {
        match self {
            AddressSet::Few(few) => {
                few.push(at);
                if few.len() > FEW {
                    *self = AddressSet::Many(few.drain(..).collect());
                }
            }
            AddressSet::Many(many) => {
                many.insert(at);
            }
        }
    }

    /// Takes `at` out (every time it is there).
    pub fn remove(&mut self, at: usize) {
        match self {
            AddressSet::Few(few) => few.retain(|&shown| shown != at),
            AddressSet::Many(many) => {
                many.remove(&at);
                if many.len() <= FEW / 2 {
                    *self = AddressSet::Few(many.drain().collect());
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        match self {
            AddressSet::Few(few) => few.len(),
            AddressSet::Many(many) => many.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        let (few, many) = match self {
            AddressSet::Few(few) => (Some(few.iter().copied()), None),
            AddressSet::Many(many) => (None, Some(many.iter().copied())),
        };
        few.into_iter().flatten().chain(many.into_iter().flatten())
    }

    /// The addresses as a slice, when there are few.
    pub fn as_slice(&self) -> Option<&[usize]> {
        match self {
            AddressSet::Few(few) => Some(few),
            AddressSet::Many(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_few_keep_their_order_and_many_are_a_set() {
        let mut set = AddressSet::default();
        for at in [30, 10, 20] {
            set.push(at);
        }
        assert_eq!(set.iter().collect::<Vec<_>>(), [30, 10, 20]);
        assert_eq!(set.as_slice(), Some(&[30, 10, 20][..]));
        set.remove(10);
        assert_eq!(set.iter().collect::<Vec<_>>(), [30, 20]);
        for at in 0..100 {
            set.push(1000 + at);
        }
        assert!(matches!(set, AddressSet::Many(_)));
        assert_eq!(set.len(), 102);
        assert!(set.as_slice().is_none());
        for at in 0..100 {
            set.remove(1000 + at);
        }
        let mut left: Vec<usize> = set.iter().collect();
        left.sort_unstable();
        assert_eq!(left, [20, 30]);
        assert!(matches!(set, AddressSet::Few(_)), "back to a vector when few");
        set.remove(20);
        set.remove(30);
        assert!(set.is_empty());
    }

    /// n addresses of one page taken out one by one: n steps, not n squared.
    #[test]
    fn taking_out_many_addresses_is_not_quadratic() {
        let n = 200_000;
        let mut set = AddressSet::default();
        for at in 0..n {
            set.push(at * 0x10000);
        }
        let started = std::time::Instant::now();
        for at in 0..n {
            set.remove(at * 0x10000);
        }
        assert!(set.is_empty());
        // (A vector took many seconds at this size.)
        assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
    }
}
