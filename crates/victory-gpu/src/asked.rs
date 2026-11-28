//! What a thread remembers of the answers to asks that are made again and again, to give them back without the
//! lock and the search the first answer took (`crate::shadow`'s windows; `VICTORY_ASK_CACHE=1`).
//!
//! An answer is a shared object that is handed out by another map, under its lock, and that is let go of when nobody
//! holds it. What a thread keeps is a weak hold, stamped with a number that grows whenever that map may have changed
//! (and on the let-go side *before* it looks at who holds what): an answer is given back if the stamp is the number
//! still, after the hold was made strong. Whoever lets an object go finds it held now, or had counted before that
//! number moved: then it is not handed out.

use std::sync::atomic::{Ordering, fence};
use std::sync::{Arc, Weak};

/// A direct-mapped table of the answers of one thread.
pub(crate) struct Asked<T> {
    slots: Vec<(u64, u64, u64, Weak<T>)>,
}

impl<T> Asked<T> {
    pub(crate) fn new(len: usize) -> Self {
        Asked { slots: (0..len.max(1)).map(|_| (0, 0, u64::MAX, Weak::new())).collect() }
    }

    fn at(&self, a: u64, b: u64) -> usize {
        let h = (a ^ b.rotate_left(32)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (h >> 40) as usize % self.slots.len()
    }

    /// The answer to the ask `(a, b)` if what it was stamped with still holds (`still(stamp)`: the number is as it was, or
    /// nothing changed in the range of the answer since), before the hold was made strong and after.
    pub(crate) fn get(&self, a: u64, b: u64, still: impl Fn(u64) -> bool) -> Option<Arc<T>> {
        let (ka, kb, stamp, held) = &self.slots[self.at(a, b)];
        if (*ka, *kb) != (a, b) || !still(*stamp) {
            return None;
        }
        let answer = held.upgrade()?;
        fence(Ordering::SeqCst);
        still(*stamp).then_some(answer)
    }

    /// `answer` was given for the ask `(a, b)` while the number was `stamp` (read before the ask).
    pub(crate) fn put(&mut self, a: u64, b: u64, stamp: u64, answer: &Arc<T>) {
        let at = self.at(a, b);
        self.slots[at] = (a, b, stamp, Arc::downgrade(answer));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn an_answer_is_given_back_while_the_number_stays() {
        let mut asked: Asked<u32> = Asked::new(8);
        let answer = Arc::new(7u32);
        assert!(asked.get(1, 2, |st| st == 5).is_none());
        asked.put(1, 2, 5, &answer);
        assert_eq!(asked.get(1, 2, |st| st == 5).as_deref(), Some(&7));
        // Another ask, another number: nothing.
        assert!(asked.get(1, 3, |st| st == 5).is_none());
        // (Another number: what is stamped is not what holds.)
        assert!(asked.get(1, 2, |st| st == 6).is_none());
        // Nobody holds it any more: nothing (the weak hold is no hold).
        drop(answer);
        assert!(asked.get(1, 2, |st| st == 5).is_none());
        // The number moves while the hold is made strong: nothing.
        let answer = Arc::new(8u32);
        asked.put(1, 2, 5, &answer);
        let n = std::cell::Cell::new(5);
        assert!(asked.get(1, 2, |st| st == n.replace(n.get() + 1)).is_none());
    }

    #[test]
    fn a_colliding_ask_takes_the_place_of_the_other() {
        let mut asked: Asked<u32> = Asked::new(1);
        let (a, b) = (Arc::new(1u32), Arc::new(2u32));
        asked.put(1, 1, 0, &a);
        asked.put(2, 2, 0, &b);
        assert!(asked.get(1, 1, |st| st == 0).is_none());
        assert_eq!(asked.get(2, 2, |st| st == 0).as_deref(), Some(&2));
    }

    /// Threads that ask while another changes what is answered and moves the number: never the answer of another
    /// ask, and when it is quiet again every answer is the current one.
    #[test]
    fn threads_never_get_the_answer_of_another_ask_while_it_changes() {
        const ASKS: u64 = 64;
        let world = AtomicU64::new(1);
        // The map: for each ask the answer, (ask, generation); as for windows, replaced under the lock and the
        // number moved before.
        let map: Mutex<Vec<Arc<(u64, u64)>>> = Mutex::new((0..ASKS).map(|k| Arc::new((k, 1))).collect());
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                for generation in 2..400u64 {
                    let mut map = map.lock().unwrap();
                    world.fetch_add(1, Ordering::SeqCst);
                    for (k, a) in map.iter_mut().enumerate() {
                        *a = Arc::new((k as u64, generation));
                    }
                    drop(map);
                    std::thread::yield_now();
                }
                stop.store(true, Ordering::SeqCst);
            });
            for t in 0..4u64 {
                let (world, map, stop) = (&world, &map, &stop);
                s.spawn(move || {
                    let mut asked: Asked<(u64, u64)> = Asked::new(16);
                    let mut i = t;
                    while !stop.load(Ordering::SeqCst) {
                        i = i.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                        let k = (i >> 33) % ASKS;
                        let now = || world.load(Ordering::SeqCst);
                        let answer = match asked.get(k, 0, |st| st == now()) {
                            Some(a) => a,
                            None => {
                                let stamp = now();
                                let a = map.lock().unwrap()[k as usize].clone();
                                asked.put(k, 0, stamp, &a);
                                a
                            }
                        };
                        assert_eq!(answer.0, k);
                    }
                });
            }
        });
        // Quiet: asked twice (the second from the table), the answer is the current one.
        let mut asked: Asked<(u64, u64)> = Asked::new(16);
        for k in 0..ASKS {
            let stamp = world.load(Ordering::SeqCst);
            let a = map.lock().unwrap()[k as usize].clone();
            asked.put(k, 0, stamp, &a);
            let again = asked.get(k, 0, |st| st == world.load(Ordering::SeqCst)).unwrap();
            assert!(Arc::ptr_eq(&a, &again));
            assert_eq!(again.1, 399 + 0);
        }
    }
}
