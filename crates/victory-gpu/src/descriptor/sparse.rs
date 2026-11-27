//! The per-slot arrays of a host view heap, that take memory only for the
//! slots used (`VICTORY_HEAP_SPARSE=1`; off: whole at once, as before).
//!
//! Every host view heap has arrays beside it with a place for every slot
//! of the heap: the resource each slot shows (a lock and a pointer, 16
//! bytes), when each was made from what, which are tables'. They were made
//! by writing every element, so each heap took 4 MB for its 255,000 dynamic
//! slots and 25 MB more for a title heap of 750,000 slots, whether a slot
//! was used or not, and the title makes a host heap for each of its
//! shader-visible heaps that is used.
//!
//! - Arrays of numbers are made of zeroed memory, which the system gives
//!   as pages that nothing has touched, and takes pages for only when a
//!   slot in them is written.
//! - The array of locks and pointers ([`Pages`]) is made in pages of
//!   [`PAGE`] slots when a slot in the page is first asked for to be
//!   written; reading a slot of a page not made finds nothing.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Slots in a page of [`Pages`].
pub(crate) const PAGE: usize = 4096;

/// Pages made, of all [`Pages`] alive, and how many slots they hold (for the
/// memory line).
pub(crate) static PAGES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// `n` atomic words that are all zero, in memory no page of which is
/// touched until a word in it is.
pub(crate) fn zeroed_u64(n: usize) -> Vec<AtomicU64> {
    let mut words = ManuallyDrop::new(vec![0u64; n]);
    // SAFETY: `AtomicU64` has the size, alignment and representation of
    // `u64`; the allocation is handed over whole, with its length and
    // capacity, to be freed as the same layout.
    unsafe {
        Vec::from_raw_parts(
            words.as_mut_ptr().cast::<AtomicU64>(),
            words.len(),
            words.capacity(),
        )
    }
}

/// [`zeroed_u64`] of 32-bit words.
pub(crate) fn zeroed_u32(n: usize) -> Vec<AtomicU32> {
    let mut words = ManuallyDrop::new(vec![0u32; n]);
    // SAFETY: as for `zeroed_u64`.
    unsafe {
        Vec::from_raw_parts(
            words.as_mut_ptr().cast::<AtomicU32>(),
            words.len(),
            words.capacity(),
        )
    }
}

/// The same, with every element written when `sparse` is off.
pub(crate) fn words64(n: usize, sparse: bool) -> Vec<AtomicU64> {
    if sparse {
        zeroed_u64(n)
    } else {
        (0..n).map(|_| AtomicU64::new(0)).collect()
    }
}

/// [`words64`] of 32-bit words.
pub(crate) fn words32(n: usize, sparse: bool) -> Vec<AtomicU32> {
    if sparse {
        zeroed_u32(n)
    } else {
        (0..n).map(|_| AtomicU32::new(0)).collect()
    }
}

/// A lock and an option for each of `len` slots, in pages made when first
/// asked for. A page is never given back while the array lives: a slot
/// found in it stays where it is.
pub(crate) struct Pages<T> {
    pages: Box<[OnceLock<Box<[Mutex<Option<T>>]>>]>,
    len: usize,
}

impl<T> Pages<T> {
    /// `len` slots; with `sparse` off every page is made now.
    pub(crate) fn new(len: usize, sparse: bool) -> Self {
        let pages = (0..len.div_ceil(PAGE)).map(|_| OnceLock::new()).collect();
        let this = Pages { pages, len };
        if !sparse {
            for page in 0..this.pages.len() {
                this.page(page);
            }
        }
        this
    }

    fn page(&self, page: usize) -> &[Mutex<Option<T>>] {
        self.pages[page].get_or_init(|| {
            PAGES[0].fetch_add(1, Ordering::Relaxed);
            PAGES[1].fetch_add(PAGE as u64, Ordering::Relaxed);
            (0..PAGE).map(|_| Mutex::new(None)).collect()
        })
    }

    /// Slot `index`, its page made if it is not.
    pub(crate) fn get(&self, index: usize) -> Option<&Mutex<Option<T>>> {
        (index < self.len).then(|| &self.page(index / PAGE)[index % PAGE])
    }

    /// Slot `index` if its page was made.
    pub(crate) fn peek(&self, index: usize) -> Option<&Mutex<Option<T>>> {
        if index >= self.len {
            return None;
        }
        Some(&self.pages[index / PAGE].get()?[index % PAGE])
    }
}

impl<T> Drop for Pages<T> {
    fn drop(&mut self) {
        let made = self.pages.iter().filter(|p| p.get().is_some()).count() as u64;
        PAGES[0].fetch_sub(made, Ordering::Relaxed);
        PAGES[1].fetch_sub(made * PAGE as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn zeroed_words_are_zero_and_work_as_atomics() {
        let words = zeroed_u64(1 << 20);
        assert!(
            words
                .iter()
                .step_by(4099)
                .all(|w| w.load(Ordering::Relaxed) == 0)
        );
        words[777_777].store(u64::MAX, Ordering::Relaxed);
        assert_eq!(words[777_777].fetch_add(1, Ordering::Relaxed), u64::MAX);
        let small = zeroed_u32(5);
        small[4].store(9, Ordering::Relaxed);
        assert_eq!(
            small.iter().map(|w| w.load(Ordering::Relaxed)).sum::<u32>(),
            9
        );
        assert_eq!(zeroed_u64(0).len(), 0);
        // Either way, the same array.
        for sparse in [false, true] {
            let a = words64(10_000, sparse);
            a[9_999].store(3, Ordering::Relaxed);
            assert_eq!(
                (
                    a.len(),
                    a[9_999].load(Ordering::Relaxed),
                    a[0].load(Ordering::Relaxed)
                ),
                (10_000, 3, 0)
            );
            assert_eq!(words32(7, sparse).len(), 7);
        }
    }

    #[test]
    fn a_page_is_made_when_a_slot_in_it_is_asked_for() {
        let pages = Pages::<u32>::new(3 * PAGE + 5, true);
        assert!(pages.peek(0).is_none(), "nothing made yet");
        *pages.get(PAGE + 7).unwrap().lock().unwrap() = Some(7);
        assert!(
            pages.peek(PAGE + 8).is_some(),
            "the page is there for its neighbours"
        );
        assert_eq!(*pages.peek(PAGE + 7).unwrap().lock().unwrap(), Some(7));
        assert_eq!(*pages.peek(PAGE + 8).unwrap().lock().unwrap(), None);
        assert!(pages.peek(0).is_none() && pages.peek(2 * PAGE).is_none());
        // The last, short page, and past the end.
        assert!(pages.get(3 * PAGE + 4).is_some());
        assert!(pages.get(3 * PAGE + 5).is_none() && pages.peek(3 * PAGE + 5).is_none());
        assert!(pages.get(usize::MAX).is_none());
    }

    #[test]
    fn whole_and_sparse_arrays_answer_alike_at_any_slot() {
        let whole = Pages::<usize>::new(3 * PAGE + 5, false);
        let sparse = Pages::<usize>::new(3 * PAGE + 5, true);
        for index in [0, 1, PAGE - 1, PAGE, 2 * PAGE + 3, 3 * PAGE + 4] {
            for pages in [&whole, &sparse] {
                assert_eq!(*pages.get(index).unwrap().lock().unwrap(), None);
                *pages.get(index).unwrap().lock().unwrap() = Some(index);
            }
            assert_eq!(
                *whole.peek(index).unwrap().lock().unwrap(),
                *sparse.peek(index).unwrap().lock().unwrap()
            );
        }
        // A whole array has every page; a sparse one only those written.
        assert!(whole.peek(PAGE + 1).is_some());
        assert!(sparse.peek(PAGE + 1).is_none());
    }

    #[test]
    fn writers_on_many_threads_make_each_page_once_and_lose_nothing() {
        let pages = Arc::new(Pages::<u64>::new(64 * PAGE, true));
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let pages = pages.clone();
                std::thread::spawn(move || {
                    // Every thread walks all pages, a different slot in each, and
                    // all of them meet on slot 0 of every page.
                    for page in 0..64usize {
                        *pages
                            .get(page * PAGE + t as usize + 1)
                            .unwrap()
                            .lock()
                            .unwrap() = Some(t + 100);
                        let mut shared = pages.get(page * PAGE).unwrap().lock().unwrap();
                        *shared = Some(shared.unwrap_or(0) + 1);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        for page in 0..64usize {
            assert_eq!(*pages.peek(page * PAGE).unwrap().lock().unwrap(), Some(8));
            for t in 0..8u64 {
                assert_eq!(
                    *pages
                        .peek(page * PAGE + t as usize + 1)
                        .unwrap()
                        .lock()
                        .unwrap(),
                    Some(t + 100)
                );
            }
        }
    }
}
