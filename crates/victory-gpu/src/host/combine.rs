//! Group commit for waits that one call can serve for many threads.
//!
//! New card memory is made resident (`MakeResident`) and then waited for until
//! the system has mapped it (a copy on a queue of ours): the first a call for
//! each object, the second a round trip through the whole mapping queue of the
//! system. With the scene's first frames asking for memory from eight threads
//! at once (15,000 `MakeResident` and 14,000 mapping waits in the 120 s after
//! the entry frame of the cutscene, 47 to 57 s of thread time in all, V4a and
//! V4b), each thread paid a call or a round trip of its own, one after the
//! other. [`Combine::join`] lets the thread that finds nothing running do the
//! work for itself and for everyone who joins while it runs: those wait for the
//! one after it, which does the work of all of them in one call. A thread alone
//! pays nothing for this.

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex};

struct State<T> {
    /// Items joined and not yet taken by a batch, with their numbers.
    pending: Vec<(u64, T)>,
    next_id: u64,
    /// The number of the batch the next to start will be; a thread that joins
    /// while one runs belongs to this one.
    next_batch: u64,
    /// Batches before this one are done.
    completed: u64,
    running: bool,
    /// What went wrong for an item, until its thread takes it.
    failed: BTreeMap<u64, windows::core::Error>,
}

pub(crate) struct Combine<T> {
    state: Mutex<State<T>>,
    done: Condvar,
}

impl<T> Combine<T> {
    pub(crate) const fn new() -> Combine<T> {
        Combine {
            state: Mutex::new(State {
                pending: Vec::new(),
                next_id: 0,
                next_batch: 0,
                completed: 0,
                running: false,
                failed: BTreeMap::new(),
            }),
            done: Condvar::new(),
        }
    }

    /// Returns when `work` has run with `item` among its items. `work` gets
    /// every item joined since the last batch started (numbered) and gives
    /// back the numbers that failed and why; it runs on one of the joining
    /// threads, one batch at a time, with no lock of ours held.
    pub(crate) fn join(
        &self,
        item: T,
        work: impl FnOnce(Vec<(u64, T)>) -> Vec<(u64, windows::core::Error)>,
    ) -> windows::core::Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let id = st.next_id;
        st.next_id += 1;
        st.pending.push((id, item));
        // A batch already running began before this item was joined: it
        // belongs to the next.
        let mine = st.next_batch;
        let mut work = Some(work);
        loop {
            if st.completed > mine {
                break;
            }
            if st.running {
                st = self.done.wait(st).unwrap_or_else(|e| e.into_inner());
                continue;
            }
            // Nothing runs: this thread does the batch, which is `mine` (the
            // ones before it are done).
            let batch = st.next_batch;
            st.next_batch += 1;
            st.running = true;
            let items = std::mem::take(&mut st.pending);
            drop(st);
            let mut ended = Ended {
                combine: self,
                batch,
                ids: items.iter().map(|(id, _)| *id).collect(),
                failed: None,
            };
            if let Some(work) = work.take() {
                ended.failed = Some(work(items));
            }
            drop(ended);
            st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        }
        match st.failed.remove(&id) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// A batch's end, however its work ended (a panic included): its failures are
/// noted and everyone waiting is woken.
struct Ended<'a, T> {
    combine: &'a Combine<T>,
    batch: u64,
    ids: Vec<u64>,
    /// What the work said failed; `None` if it did not come back (a panic):
    /// then every item of the batch failed.
    failed: Option<Vec<(u64, windows::core::Error)>>,
}

impl<T> Drop for Ended<'_, T> {
    fn drop(&mut self) {
        let mut st = self.combine.state.lock().unwrap_or_else(|e| e.into_inner());
        match self.failed.take() {
            Some(failed) => st.failed.extend(failed),
            None => {
                for id in &self.ids {
                    st.failed.insert(*id, windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
                }
            }
        }
        st.completed = self.batch + 1;
        st.running = false;
        drop(st);
        self.combine.done.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::Combine;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn fail() -> windows::core::Error {
        windows::core::Error::from(windows::Win32::Foundation::E_FAIL)
    }

    #[test]
    fn a_thread_alone_runs_its_own_batch() {
        let c = Combine::<u32>::new();
        let mut seen = Vec::new();
        c.join(7, |items| {
            seen = items.iter().map(|(_, v)| *v).collect();
            Vec::new()
        })
        .unwrap();
        assert_eq!(seen, [7]);
        // And again: nothing is left over from the first.
        c.join(8, |items| {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].1, 8);
            Vec::new()
        })
        .unwrap();
    }

    #[test]
    fn threads_that_join_while_a_batch_runs_share_the_next_one() {
        let c = Arc::new(Combine::<usize>::new());
        let batches = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let n = 8;
        let threads: Vec<_> = (0..n)
            .map(|i| {
                let (c, batches, seen) = (c.clone(), batches.clone(), seen.clone());
                std::thread::spawn(move || {
                    c.join(i, |items| {
                        batches.fetch_add(1, Ordering::SeqCst);
                        // A slow batch, so that the others join behind it.
                        std::thread::sleep(Duration::from_millis(30));
                        seen.lock().unwrap().extend(items.iter().map(|(_, v)| *v));
                        Vec::new()
                    })
                    .unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let mut seen = seen.lock().unwrap().clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..n).collect::<Vec<_>>(), "every item was in a batch, once");
        let batches = batches.load(Ordering::SeqCst);
        assert!(batches < n, "{batches} batches for {n} threads");
    }

    #[test]
    fn a_failure_reaches_the_thread_whose_item_it_was() {
        let c = Arc::new(Combine::<&'static str>::new());
        let threads: Vec<_> = ["good", "bad", "good"]
            .into_iter()
            .map(|name| {
                let c = c.clone();
                std::thread::spawn(move || {
                    let r = c.join(name, |items| {
                        std::thread::sleep(Duration::from_millis(20));
                        items.iter().filter(|(_, v)| *v == "bad").map(|(id, _)| (*id, fail())).collect()
                    });
                    (name, r.is_ok())
                })
            })
            .collect();
        for t in threads {
            let (name, ok) = t.join().unwrap();
            assert_eq!(ok, name == "good", "{name}");
        }
    }

    #[test]
    fn a_panicking_batch_does_not_leave_the_others_waiting() {
        let c = Arc::new(Combine::<u8>::new());
        let first = {
            let c = c.clone();
            std::thread::spawn(move || {
                let _ = c.join(1, |_| panic!("the work failed"));
            })
        };
        assert!(first.join().is_err());
        assert!(c.state.lock().unwrap().failed.contains_key(&0), "its item is marked failed");
        c.state.lock().unwrap().failed.clear();
        // The next join runs its own batch.
        c.join(2, |items| {
            assert_eq!(items.len(), 1);
            Vec::new()
        })
        .unwrap();
    }
}
