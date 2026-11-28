//! Landing and relay glue: the helper threads that land flights, the relay worker, and the card's results coming back.

use super::*;

/// The relay's first stage (on by default, with stage 2 on top, see [`relay_exact`]; `VICTORY_RELAY=0` turns
/// it off; it turns
/// [`late`] on at stage 3 unless `VICTORY_LATE_WAIT` says another stage,
/// and is off with it, in the timing run and with kept snapshots).
///
/// With [`late`] alone every settle does three things on the thread that
/// holds the submission lock: it waits for everything in the air, copies
/// back with the card standing, and only then goes on preparing. The frame
/// stays a relay: the card works about 242 ms of it and one thread
/// prepares, waits and copies back for about 220.
///
/// With the switch a thread of its own ([`relay_worker`]) lands flights as
/// soon as the card has run them, in the order they were sent and without
/// the submission lock: it fetches and merges what their shaders wrote to
/// copies in the card's memory, compares and merges the other copies,
/// frees the flights' ranges, and then does their read-backs, their
/// bottom-of-pipe writes and the fence signals behind them, in the line's
/// order. The submitting thread meanwhile prepares and sends the next
/// submissions. [`Host::settle`] no longer settles anything: it waits until
/// that thread has landed everything in the air, at the same points as
/// before (the list in [`late`]), so whoever can observe title memory, a
/// fence or a present still does so only after the flights before it are
/// merged.
///
/// What makes that safe is in `crate::shadow::relay` (ranges counted per
/// flight; a merge that never stores into memory the card may be writing;
/// sends to card copies numbered) and here: flights land in the order of
/// [`LINE`] by one thread, so a later flight's results never reach title
/// memory before an earlier flight's; the compare on the card stores the
/// hash it made, so each flight's compare flags exactly what changed since
/// the compare before it, however late the pieces are fetched; the fetch
/// goes to the queue under [`QUEUE_ORDER`], which says which sends of
/// title bytes the card had received when it ran.
pub(crate) fn relay() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| late_stage() > 0 && crate::shadow::relay())
}

/// The relay's second stage (`VICTORY_RELAY=2`): a submission that reads
/// title memory a flight's shaders may write (a title buffer copy, a fill
/// of upload memory, a texture) waits only until the flights that declared
/// that memory have landed, not for everything in the air; and at
/// [`late_most`] lists in the air it waits until there are fewer. At `1`
/// both wait for everything.
pub(super) fn relay_exact() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    // Stage 2 unless `VICTORY_RELAY=1` asks for the first one (the default
    // since the walls of the first room were found drawn in every run with
    // this set and in fewer than half without it).
    *ON.get_or_init(|| relay() && crate::defaults::var_os("VICTORY_RELAY").is_none_or(|v| v == "2"))
}

/// Whether the pieces the relay fetches from the card's memory come over the
/// card's copy queue (`VICTORY_RELAY_COPY=1`) and not over the queue that
/// draws.
///
/// On the drawing queue a fetch runs behind whatever was sent meanwhile,
/// and with the relay that is most of a frame: the thread that lands flights
/// stood 48 ms of a 120 ms frame waiting for seven fetches of 94 MB in all,
/// a few milliseconds of copying, and everything that waits for a landing
/// (the title's fences, reads of memory flights may write) waited with it.
///
/// What changes: the fetch runs when its flights are done and beside later
/// submissions, so of the sends of title bytes it is only known to have run
/// behind those its own flights ran behind (`CardPlan::sent_as`). Bytes sent
/// later are left out of the merge (`Shadow::card_merge_relay`): they are
/// the title's, newer than what the flights wrote. A later flight's bytes
/// may be in what is fetched, as before.
pub(super) fn relay_copy() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::defaults::var_os("VICTORY_RELAY_COPY").is_some_and(|v| v == "1"))
}

/// Told when something joins [`LINE`], for the thread that lands flights
/// ([`relay`]); [`LINE_MOVED`] stays the word for the thread that looks at
/// what queues hold.
pub(super) static RELAY_MOVED: std::sync::Condvar = std::sync::Condvar::new();

/// Told when flights have landed ([`relay`]): who waits for that looks
/// again.
pub(super) static LINE_LANDED: std::sync::Condvar = std::sync::Condvar::new();

/// With [`relay`], the order of what we put on the queue: the number of
/// sends of changed ranges to copies in the card's memory so far
/// (`Host::card_before`). Held from the taking of the ranges until their
/// list is on the queue, and while a fetch of pieces goes to the queue: a
/// fetch sent with the count at N ran behind sends 1..N and ahead of every
/// later one.
pub(super) static QUEUE_ORDER: Mutex<u64> = Mutex::new(0);

/// Since the last frame-rate line, of the thread that lands flights
/// ([`relay`]): landings, flights landed, microseconds it waited for the
/// card with nothing to land, microseconds it waited for fetches of card
/// copies' pieces, microseconds of landing in all.
pub(super) static RELAY_STATS: [std::sync::atomic::AtomicU64; 7] = [const { std::sync::atomic::AtomicU64::new(0) }; 7];

/// For one plan of copies in the card's memory, with [`relay`]: `flags`
/// holds, for every piece of every range of the plan, the copy, the
/// piece's number in the copy and whether the compare flagged it there (or
/// did not run). The pieces to fetch, under whichever range, in order: all
/// of them flagged under any. Two ranges of a plan may hold parts of the same
/// 64 KiB piece; the compare hashes the whole piece for each and stores
/// the hash, so the second of them may find it stored already and not
/// flag it.
pub(super) fn flagged_pieces(flags: &[(usize, u32, bool)]) -> Vec<(usize, u32)> {
    let mut pieces: Vec<(usize, u32)> = flags.iter().filter(|f| f.2).map(|f| (f.0, f.1)).collect();
    pieces.sort_unstable();
    pieces.dedup();
    pieces
}

/// Resets the host lists of settled flights and keeps them for reuse, as
/// a queue does for the lists it ran (`crate::list::recycle`).
pub(super) fn land() {
    let landed: Vec<_> = {
        let mut line = line();
        let n = line.landed.len().saturating_sub(bury_lag() as usize);
        line.landed.drain(..n).collect()
    };
    for (lists, order) in landed {
        crate::list::recycle(lists, &order);
    }
}

/// Whether the thread of [`late_helper`] is running.
pub(super) static HELPER_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The thread that settles the flights once the card has run them all
/// ([`late`]): without it a bottom-of-pipe write would reach the title only
/// at the next call that settles, and a title thread that polls such a
/// value while the submitting thread waits for that thread would never
/// see it.
pub(super) fn late_helper() {
    crate::cores::keep_on_fast_cores("victory-late-wait");
    let Some(host) = host() else {
        return;
    };
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        tracing::warn!("no event for the thread that settles submissions sent ahead");
        return;
    };
    // (Only now: the lane leaves the landed lists to this thread, and a thread that returned above gives back none.)
    HELPER_UP.store(true, Ordering::Release);
    loop {
        let target = {
            let mut line = line();
            loop {
                if line.poke {
                    line.poke = false;
                    break None;
                }
                // (With the relay the flights are another thread's.)
                if !relay() && !line.line.is_empty() && !line.settling {
                    break Some(line.target);
                }
                line = LINE_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(target) = target else {
            let mut helping = crate::timeline::span("helper: landed lists reset and given back");
            land();
            helping.next("helper: looking at what queues hold");
            crate::queue::release_held(false);
            continue;
        };
        // The card first, without the submission lock: the submitting
        // thread prepares and sends meanwhile.
        // SAFETY: our fence and event.
        unsafe {
            if host.fence.SetEventOnCompletion(target, event).is_ok() {
                #[cfg(feature = "diagnostics")]
                let _card = crate::timeline::card_wait(Some(target));
                WaitForSingleObject(event, INFINITE);
            }
        }
        {
            let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
            // Something newer went up meanwhile: the card is waited for
            // again, the submitting thread is not held up for it.
            if line().target != target {
                continue;
            }
            host.settle(&mut submitted, WHY_CARD_DONE);
        }
        land();
        crate::queue::release_held(false);
    }
}

/// Starts the threads of [`late`], once: the one that settles flights when
/// the card has run them and looks at what queues hold, and with [`relay`]
/// the one that lands flights.
pub(super) fn late_threads() {
    static HELPER: std::sync::Once = std::sync::Once::new();
    HELPER.call_once(|| {
        if let Err(e) = std::thread::Builder::new().name("victory-late-wait".into()).spawn(late_helper) {
            tracing::warn!(error = %e, "no thread to settle submissions sent ahead: they are settled by the calls that follow");
        }
        tracing::info!(
            most = late_most(),
            writes = late_writes(),
            "submissions are sent to the card without a wait for each (VICTORY_LATE_WAIT=1)"
        );
        if relay() {
            // Without it nothing would ever land.
            if let Err(e) = std::thread::Builder::new().name("victory-relay".into()).spawn(relay_worker) {
                fail(format_args!("no thread to land submissions sent ahead (VICTORY_RELAY=1): {e}"));
            }
            tracing::info!(
                exact = relay_exact(),
                "what shaders wrote is copied back by a thread of its own, beside the submitting thread (VICTORY_RELAY=1)"
            );
        }
    });
}

/// With [`relay`]: waits until `done` says so of the line, looked at each
/// time flights have landed. Counted as a settle for `why`, and its time
/// as waiting for the card. The caller may hold the submission lock: the
/// thread that lands flights never takes it.
pub(super) fn relay_wait(why: usize, done: impl Fn(&Line) -> bool) {
    let mut line = line();
    if done(&line) {
        return;
    }
    LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
    LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
    let was_working = working(false);
    let waiting = std::time::Instant::now();
    let _waiting = crate::timeline::span(match why {
        WHY_SYNC => "waiting for landings: everything (a present, the sweep)",
        WHY_VALUE => "waiting for landings: a memory value",
        WHY_STALE => "waiting for landings: title memory changed under shader writes",
        WHY_READ => "waiting for landings: title memory read",
        WHY_MOST => "waiting for landings: lists in the air",
        WHY_FENCE => "waiting for landings: a blocking fence wait",
        _ => "waiting for landings: another reason",
    });
    while !done(&line) {
        line = LINE_LANDED.wait(line).unwrap_or_else(|e| e.into_inner());
    }
    drop(line);
    stood(waiting);
    working(was_working);
    waits::note(waits::Wait::Flights, waiting.elapsed().as_nanos() as u64);
    let us = waiting.elapsed().as_micros() as u64;
    LATE_STATS[12].fetch_add(us, Ordering::Relaxed);
    LATE_WAITED[why.min(10)].fetch_add(us, Ordering::Relaxed);
    SUBMISSION_US[1].fetch_add(us, Ordering::Relaxed);
    #[cfg(feature = "diagnostics")]
    blamed(why, us);
}

/// What the thread that lands flights fetches card copies' pieces with
/// ([`relay`]): a list, a fence and an event of its own (the host's are
/// behind the submission lock), and the memory the pieces come down to.
pub(super) struct RelayFetch {
    pub(super) list: Prelude,
    /// With [`relay_copy`]: the card's copy queue, which `list` is for.
    pub(super) queue: Option<ID3D12CommandQueue>,
    pub(super) fence: ID3D12Fence,
    pub(super) event: HANDLE,
    /// The last value signalled on `fence`.
    pub(super) sent: u64,
    /// Buffer, mapping, bytes.
    pub(super) buffer: Option<(ID3D12Resource, usize, u64)>,
    /// When `buffer` is made smaller again ([`crate::lean::fetch_trim`]).
    pub(super) trim: crate::lean::Trim,
    /// Pieces caught while a later flight was writing them.
    pub(super) suspects: torn::Suspects,
    /// What earlier fetches brought of flights not landed yet.
    pub(super) had: crate::land::Had,
}

/// The thread that lands flights ([`relay`]): as soon as the card has run
/// the first flights of the line it takes them, with what follows them,
/// and does what they left to do ([`Host::relay_land`]); else it waits for
/// the card, or for a flight. It takes neither the submission lock nor
/// anything a caller of [`Host::settle`] may hold: resetting the landed
/// lists and looking at what queues hold are left to [`late_helper`].
pub(super) fn relay_worker() {
    crate::cores::keep_on_fast_cores("victory-relay");
    crate::threadcpu::register("relay");
    crate::cores::prioritize("victory-relay", crate::cores::Role::Helper);
    let Some(host) = host() else {
        return;
    };
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        fail(format_args!("no event for the thread that lands submissions sent ahead (VICTORY_RELAY=1)"));
    };
    let mut fetch: Option<RelayFetch> = None;
    loop {
        let next = {
            let mut line = line();
            loop {
                if line.line.is_empty() {
                    line = RELAY_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                let fences: Vec<Option<u64>> = line
                    .line
                    .iter()
                    .map(|l| match l {
                        Late::Run(flight) => Some(flight.fence),
                        _ => None,
                    })
                    .collect();
                // SAFETY: a plain query.
                let completed = unsafe { host.fence.GetCompletedValue() };
                let n = landable(&fences, completed);
                if n == 0 {
                    // The first thing is a flight the card has not run.
                    break Err(fences[0].unwrap_or(0));
                }
                line.settling = true;
                let items: Vec<Late> = line.line.drain(..n).collect();
                // Nothing behind them, and the title is told something: the
                // landing its next frame waits for ([`crate::land::tail`]).
                let tail = line.line.is_empty() && items.iter().any(|l| matches!(l, Late::Signal(..)));
                line.in_hand = items
                    .iter()
                    .filter_map(|l| match l {
                        Late::Write(at, what, bytes) => Some((*at, *what, *bytes)),
                        _ => None,
                    })
                    .collect();
                if card_steps::stage() > 0 {
                    line.landing = card_steps::writers(&items);
                }
                break Ok((items, tail));
            }
        };
        match next {
            Ok((items, tail)) => host.relay_land(items, tail, &mut fetch),
            Err(target) => {
                let waiting = std::time::Instant::now();
                #[cfg(feature = "diagnostics")]
                let _card = crate::timeline::card_wait(Some(target));
                // SAFETY: the host's fence and this thread's event.
                let waited = unsafe {
                    host.fence.SetEventOnCompletion(target, event).map(|()| {
                        WaitForSingleObject(event, INFINITE);
                    })
                };
                if waited.is_err() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                RELAY_STATS[2].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
        }
    }
}

/// [`relay`]: what shaders wrote to the declared ranges `back` of copies
/// in the PC's memory goes into title memory. As [`hand_back`], each byte
/// once, the pieces that differ found on several threads; but the merge is
/// `Shadow::relay_merge`, which does not write the copy, and the ranges
/// are not done with here (each flight's are, afterwards). In two steps:
/// what is to be looked through and what of it differs ([`RelayBack::look`],
/// which needs nothing that the fetch from the card's memory brings), and
/// the merge.
pub(super) struct RelayBack {
    pub(super) work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
    pub(super) found: Option<Vec<Vec<(usize, usize)>>>,
}

pub(super) fn relay_hand_back(back: &[Declared]) -> RelayBack {
    let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    for (shadow, declared) in back {
        if shadow.carded() {
            continue;
        }
        let mut spans: Vec<(usize, usize)> = Vec::new();
        join_ranges(&mut spans, declared.iter().copied());
        for &(from, to) in &spans {
            shadow.relay_compared(from, to - from);
        }
        work.extend(spans.into_iter().map(|(from, to)| (shadow.clone(), from, to)));
    }
    RelayBack { work, found: None }
}

impl RelayBack {
    /// Looks the ranges through, once; its time counts as `stat`.
    pub(super) fn look(&mut self, stat: usize) {
        if self.work.is_empty() || self.found.is_some() {
            return;
        }
        let scanning = std::time::Instant::now();
        // Looked through without a lock: a piece that changes meanwhile (a
        // later flight writing, the title's bytes going in) is found now or
        // by that flight's landing, and the merge looks again under the
        // lock.
        self.found = Some(crate::shadow::find_all(&self.work));
        crate::land::spent(stat, scanning);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::BackScanUs, scanning.elapsed().as_micros() as u64);
    }

    /// What differs goes into title memory.
    pub(super) fn merge(mut self) {
        use crate::land;
        self.look(land::stat::SCAN);
        let Some(found) = self.found else {
            return;
        };
        let merging = std::time::Instant::now();
        let mut written: Vec<u64> = Vec::new();
        if land::on() {
            // Pieces in the same block of title memory in one job, in order.
            let pieces: Vec<(usize, usize, usize)> =
                found.iter().enumerate().flat_map(|(i, found)| found.iter().map(move |&(at, n)| (i, at, n))).collect();
            let start = |i: usize| self.work[i].0.span().0 as usize;
            let mut order: Vec<usize> = (0..pieces.len()).collect();
            order.sort_by_key(|&p| (start(pieces[p].0) + pieces[p].1) / crate::land::BLOCK);
            let cut: Vec<(usize, usize)> = order.iter().map(|&p| (start(pieces[p].0) + pieces[p].1, pieces[p].2)).collect();
            let jobs = land::jobs(&cut);
            let put: Vec<std::sync::atomic::AtomicU64> = self.work.iter().map(|_| Default::default()).collect();
            land::each(jobs.len(), &|j| {
                let working = std::time::Instant::now();
                let mut notes = land::Notes::default();
                for &p in &order[jobs[j].clone()] {
                    let (i, at, n) = pieces[p];
                    put[i].fetch_add(self.work[i].0.land_back(at, n, &mut notes), Ordering::Relaxed);
                }
                notes.flush();
                land::spent(land::stat::BACK_ALL, working);
            });
            land::count(land::stat::JOBS, jobs.len() as u64);
            written.extend(put.iter().map(|p| p.load(Ordering::Relaxed)));
        } else {
            let (mut now, mut title) = (Vec::new(), Vec::new());
            for ((shadow, ..), found) in self.work.iter().zip(found) {
                let merge = |(at, n)| shadow.relay_merge(at, n, &mut now, &mut title) as u64;
                written.push(found.into_iter().map(merge).sum());
            }
        }
        land::spent(land::stat::BACK, merging);
        crate::sizes::landed();
        for ((_, from, to), written) in self.work.iter().zip(written) {
            COPY_BACK[1].fetch_add(written, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(*from, to - from, written as usize);
            let _ = (from, to);
        }
    }
}

/// See [`HostList::defer_gpu_write`].
pub(super) fn declare_once() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_DECLARE_ONCE").map_or(true, |v| v != "0"))
}

/// The ranges the shaders of `runs` may write, per host copy, as declared.
/// One list's lock at a time: a submission may name a list twice.
pub(super) fn declared_writes(runs: &[&HostList]) -> Vec<Declared> {
    if summarized() { declared_writes_summarized(runs) } else { declared_writes_steps(runs) }
}

/// [`declared_writes`] over the steps.
pub(super) fn declared_writes_steps<L: onepass::StepList>(runs: &[L]) -> Vec<Declared> {
    let mut back: Vec<Declared> = Vec::new();
    // Where each copy is in it, and the ranges seen: dispatches declare the
    // same ranges thousands of times a frame.
    let mut places: ByCopy<usize> = ByCopy::default();
    let mut seen = std::collections::HashSet::<(usize, usize, usize), std::hash::BuildHasherDefault<AddressHash>>::default();
    for list in runs {
        list.steps(|steps| {
            for step in steps {
                if let Step::GpuWrite(shadow, address, len) = step {
                    let key = std::sync::Arc::as_ptr(shadow) as usize;
                    if !seen.insert((key, *address, *len)) {
                        continue;
                    }
                    let i = *places.entry(key).or_insert_with(|| {
                        back.push((shadow.clone(), Vec::new()));
                        back.len() - 1
                    });
                    back[i].1.push((*address, *address + *len));
                }
            }
        });
    }
    back
}

/// [`declared_writes`] from the summaries of the lists' steps (the ranges given to shaders are in them, in order).
pub(super) fn declared_writes_summarized<L: onepass::StepList>(runs: &[L]) -> Vec<Declared> {
    let mut back: Vec<Declared> = Vec::new();
    let mut places: ByCopy<usize> = ByCopy::default();
    let mut seen = std::collections::HashSet::<(usize, usize, usize), std::hash::BuildHasherDefault<AddressHash>>::default();
    for list in runs {
        let summary = list.summary();
        for (shadow, from, to) in summary.written() {
            let key = std::sync::Arc::as_ptr(shadow) as usize;
            if !seen.insert((key, from, to - from)) {
                continue;
            }
            let i = *places.entry(key).or_insert_with(|| {
                back.push((shadow.clone(), Vec::new()));
                back.len() - 1
            });
            back[i].1.push((from, to));
        }
    }
    back
}

/// The walls trace (`VICTORY_WALLS_TRACE=1` or `2`; off by default): log
/// lines that say what the title's CPU was given while it decided whether
/// the camera is in a room. The values of the small read-backs
/// ([`read_backs`]), the frame's main pass by scissor rectangle with its
/// culling dispatches (`draw.rs`, `walls_census`), the shader-written
/// buffers merged into title memory (`shadow.rs`, `walls_merged`), host
/// copies found stale after their refresh (`shadow.rs`; only with
/// `VICTORY_REFRESH_CHECK=1` as well: it costs about 100 ms a frame), and, in the
/// runtime, the read requests that fail (with 2: every request).
pub(crate) fn walls_trace() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WALLS_TRACE").is_some_and(|v| v == "1" || v == "2"))
}

/// After `list` has run: the texels its read-backs copied into host
/// buffers of ours go to the title's memory (`HostList::defer_read_back`).
///
/// A submission that is waited for did this itself. One sent ahead
/// ([`late`]) did not, at any stage: the flights' lists were settled
/// without it, so the title read a buffer nobody wrote, as before read-backs
/// were made (the exposure metering's two small float targets, every frame).
pub(super) fn read_backs(list: &HostList) {
    // Textures shaders wrote that the title reads on the CPU
    // ([`crate::texback`]).
    for back in &list.deferred().texture_backs {
        crate::texback::land(back);
    }
    for (buffer, at, [row, rows, pitch]) in &list.deferred().backs {
        let mut data = std::ptr::null_mut();
        // SAFETY: a read-back buffer of ours, written by the list that has
        // run; the title's destination holds every row (checked when the
        // copy was recorded).
        unsafe {
            if buffer.Map(0, None, Some(&mut data)).is_ok() && !data.is_null() {
                let from = data.cast::<u8>().cast_const();
                for line in 0..*rows {
                    std::ptr::copy_nonoverlapping(from.add(line * row.next_multiple_of(256)), (*at as *mut u8).add(line * pitch), *row);
                }
                // The walls trace: what the title is handed (one row of at
                // most 24 values: its 5x1 and 1x1 float targets).
                if walls_trace() && *rows == 1 && *row <= 96 {
                    let mut values = [0f32; 24];
                    std::ptr::copy_nonoverlapping(from, values.as_mut_ptr().cast::<u8>(), *row & !3);
                    tracing::info!(
                        frame = crate::frame::current(),
                        to = format_args!("{:#x}", *at),
                        bytes = *row,
                        values = ?&values[..*row / 4],
                        "walls trace: a read-back lands in title memory"
                    );
                }
                buffer.Unmap(0, None);
            }
        }
    }
    // The samples the occlusion queries of this list put in title memory.
    for &(at, count) in &list.deferred().occluded {
        // SAFETY: the title's destination, which the read back above filled.
        let counts = unsafe { std::slice::from_raw_parts(at as *const u64, count) };
        crate::occlusion::landed(counts);
    }
}

/// After the card ran what declared `back`: what shaders wrote there goes
/// into title memory, and the ranges are done with.
pub(super) fn hand_back(back: &[Declared]) {
    hand_back_ranges(back);
    // What the card wrote is in title memory: the sizes the title will read ([`crate::sizes`]).
    crate::sizes::landed();
}

pub(super) fn hand_back_ranges(back: &[Declared]) {
    if crate::shadow::wide_back() {
        // Every range first (the pieces that differ, found on several
        // threads), then the merges, in order, as below.
        let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
        for (shadow, declared) in back {
            if shadow.carded() {
                continue;
            }
            let mut sorted = declared.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            work.extend(spans.into_iter().map(|(from, to)| (shadow.clone(), from, to)));
        }
        #[cfg(feature = "diagnostics")]
        let scanning = std::time::Instant::now();
        let found = crate::shadow::find_all(&work);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::BackScanUs, scanning.elapsed().as_micros() as u64);
        for ((shadow, from, to), found) in work.iter().zip(found) {
            let written = shadow.copy_back_with(*from, to - from, Some(found));
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(*from, to - from, written);
        }
        for (shadow, declared) in back {
            shadow.copy_back_done(declared);
        }
        return;
    }
    for (shadow, declared) in back {
        if shadow.carded() {
            // Done before (`Host::card_back`).
            shadow.copy_back_done(declared);
            continue;
        }
        // Each byte compared once: the world declares dozens of views
        // of its 174 MB heap that overlap almost entirely, and comparing
        // every one whole was gigabytes a frame (0.3-0.5 s) for the few
        // MB shaders wrote. Only ranges that overlap or touch are
        // joined: what lies between two was not given to shaders (the
        // snapshot does not hold it).
        let mut sorted = declared.clone();
        sorted.sort_unstable();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for (from, to) in sorted {
            match spans.last_mut() {
                Some(last) if from <= last.1 => last.1 = last.1.max(to),
                _ => spans.push((from, to)),
            }
        }
        for (from, to) in spans {
            let written = shadow.copy_back(from, to - from);
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(from, to - from, written);
        }
        shadow.copy_back_done(declared);
    }
}

impl Host {
    /// The relay's own list, fence, event for [`Host::relay_card_back`].
    pub(super) fn make_relay_fetch(&self) -> Option<RelayFetch> {
        // SAFETY: plain object creation.
        let copying = relay_copy().then(|| unsafe {
            (|| -> windows::core::Result<(Prelude, ID3D12CommandQueue)> {
                let allocator: ID3D12CommandAllocator = self.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
                let list: ID3D12GraphicsCommandList =
                    self.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
                list.Close()?;
                let queue: ID3D12CommandQueue = self.device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_COPY,
                    Priority: D3D12_COMMAND_QUEUE_PRIORITY_HIGH.0,
                    ..Default::default()
                })?;
                Ok(((allocator, list), queue))
            })()
        });
        let (list, queue) = match copying {
            Some(Ok((list, queue))) => (list, Some(queue)),
            Some(Err(e)) => {
                tracing::warn!(error = %e, "no copy queue: what shaders wrote to the card's memory is fetched on the drawing queue");
                (self.make_prelude()?, None)
            }
            None => (self.make_prelude()?, None),
        };
        // SAFETY: plain objects of our own.
        unsafe {
            let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            Some(RelayFetch {
                list,
                queue,
                fence,
                event,
                sent: 0,
                buffer: None,
                trim: crate::lean::Trim::new(),
                suspects: torn::Suspects::default(),
                had: crate::land::Had::default(),
            })
        }
    }

    /// [`relay`], on the thread that lands flights: for the plans of the
    /// flights being landed (the card has run them), the pieces their
    /// compares flagged are fetched, once each, with a list and a wait of
    /// this thread's own, and what shaders wrote in them goes into title
    /// memory (`Shadow::card_merge_relay`). As [`Host::card_back`], but:
    /// no submission lock; a piece flagged under one range of a plan is
    /// fetched under all ([`flagged_pieces`]); the fetch runs behind
    /// whatever was sent meanwhile, so it may bring a later flight's
    /// bytes, which that flight's landing then finds merged already; and
    /// nothing is sent back up. `plans` come with the value signalled behind
    /// their flight. `meanwhile` is called while the fetch is on its way, if
    /// there is one.
    pub(super) fn relay_card_back(&self, plans: &[(&CardPlan, u64)], fetch: &mut Option<RelayFetch>, meanwhile: &mut dyn FnMut()) {
        use crate::land;
        let gathering = std::time::Instant::now();
        let differs = {
            let work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
            work.as_ref().map(|work| (work.differs.1, (work.differs.2 / 4) as usize))
        };
        let mut pieces: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, u64)> = Vec::new();
        let mut total = 0u64;
        if let (Some(fetch), Some(first)) = (fetch.as_mut(), plans.iter().map(|p| p.1).min()) {
            fetch.had.landing(first);
        }
        // SAFETY: the mapping of the results. The compares of these plans
        // have run, and their dwords are given to no other plan before these
        // are done with (`card_done`).
        let differs = differs.map(|(differs, dwords)| unsafe { std::slice::from_raw_parts(differs as *const u32, dwords) });
        if let Some(differs) = differs {
            // Pieces caught while a later flight was at them (`torn`).
            let suspects = fetch.as_ref().map(|f| &f.suspects);
            let mut seen = std::collections::HashSet::new();
            // Copies of buffers only shaders touch, left out of this landing
            // (`land::quiet`), and those that are brought whole.
            let mut verdicts: std::collections::HashMap<usize, land::quiet::Verdict> = std::collections::HashMap::new();
            let mut whole: Vec<std::sync::Arc<crate::shadow::Shadow>> = Vec::new();
            for &(plan, behind) in plans {
                plan.picked_found(differs);
                plan.check_gpu_ro(differs);
                let flags: Vec<(usize, u32, bool)> = plan
                    .ranges
                    .iter()
                    .filter(|r| !r.picked)
                    .flat_map(|r| {
                        (0..r.span.pieces).map(move |p| {
                            let flagged = !r.compared || differs.get((r.at + p) as usize).is_none_or(|&d| torn::flagged(d));
                            let wanted = suspects.map_or(flagged, |s| s.wanted((r.shadow.id(), r.span.first + p), flagged));
                            (std::sync::Arc::as_ptr(&r.shadow) as usize, r.span.first + p, wanted)
                        })
                    })
                    .collect();
                let fetched = flagged_pieces(&flags);
                for r in plan.ranges.iter().filter(|r| !r.picked) {
                    let copy = std::sync::Arc::as_ptr(&r.shadow) as usize;
                    let verdict = *verdicts.entry(copy).or_insert_with(|| {
                        let verdict = if land::quiet::on() || crate::readprobe::on() { r.shadow.landing_verdict() } else { land::quiet::Verdict::Land };
                        if verdict == land::quiet::Verdict::Whole {
                            whole.push(r.shadow.clone());
                        }
                        verdict
                    });
                    if verdict == land::quiet::Verdict::Left {
                        for p in 0..r.span.pieces {
                            if fetched.binary_search(&(copy, r.span.first + p)).is_ok() {
                                let (lo, hi) = r.shadow.card_piece(&r.span, p);
                                land::count(land::stat::QUIET_PIECES, u64::from(lo < hi));
                                land::count(land::stat::QUIET_BYTES, hi.saturating_sub(lo) as u64);
                            }
                        }
                        crate::shadow::CARD_STATS[2].fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
                        continue;
                    }
                    if upcheck::on() {
                        upcheck::declared(r.shadow.id(), r.span.from as usize, r.span.to as usize);
                    }
                    for p in 0..r.span.pieces {
                        if fetched.binary_search(&(copy, r.span.first + p)).is_err() {
                            continue;
                        }
                        let (lo, hi) = r.shadow.card_piece(&r.span, p);
                        if lo < hi && !seen.contains(&(copy, lo, hi)) {
                            let key = (r.shadow.id(), r.shadow.span().0 as usize + lo, hi - lo);
                            if fetch.as_ref().is_some_and(|f| f.had.holds(behind, key)) {
                                land::count(land::stat::HAD, 1);
                                land::count(land::stat::HAD_BYTES, (hi - lo) as u64);
                                if land::skip_on() {
                                    continue;
                                }
                            }
                            if upcheck::on() {
                                upcheck::fetched(r.shadow.id(), lo);
                            }
                            seen.insert((copy, lo, hi));
                            pieces.push((r.shadow.clone(), lo, hi, total));
                            total += (hi - lo) as u64;
                        }
                    }
                    crate::shadow::CARD_STATS[2].fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
                }
            }
            // A copy that was left out and has been told written in: all of
            // it comes down (merged as any piece is, only what differs from
            // the CPU's side goes into title memory).
            for shadow in &whole {
                let copy = std::sync::Arc::as_ptr(shadow) as usize;
                let before = pieces.len();
                for (lo, hi) in shadow.whole_pieces() {
                    if seen.insert((copy, lo, hi)) {
                        pieces.push((shadow.clone(), lo, hi, total));
                        total += (hi - lo) as u64;
                    }
                }
                tracing::warn!(
                    start = format_args!("{:#x}", shadow.span().0),
                    pieces = pieces.len() - before,
                    "a copy left out of landing (VICTORY_LAND_QUIET) was told written in: it lands whole once and as any other from now on"
                );
            }
        }
        if land::on() {
            // In the order they are landed, each byte of a copy once.
            let keys: Vec<land::Piece> =
                pieces.iter().map(|(shadow, lo, hi, _)| (shadow.id(), shadow.span().0 as usize + lo, hi - lo)).collect();
            total = 0;
            pieces = land::ordered(&keys)
                .into_iter()
                .map(|(i, title, len)| {
                    let shadow = &pieces[i].0;
                    let lo = title - shadow.span().0 as usize;
                    let at = total;
                    total += len as u64;
                    (shadow.clone(), lo, lo + len, at)
                })
                .collect();
        }
        land::spent(land::stat::GATHER, gathering);
        if fetch.is_none() {
            *fetch = self.make_relay_fetch();
        }
        // On the copy queue a later flight may be at a piece while it is
        // fetched: the pieces come down whole, to be hashed.
        let check = (torn::on() && fetch.as_ref().is_some_and(|f| f.queue.is_some()))
            .then(|| torn::Check::new(plans.iter().map(|p| p.0), differs.unwrap_or_default(), &mut pieces));
        if let Some(check) = &check {
            total = check.total;
        }
        #[cfg(feature = "diagnostics")]
        for (shadow, lo, hi, _) in &pieces {
            traffic::moved(traffic::FETCHED, shadow.span(), std::iter::once((*lo, *hi)));
        }
        // The sends these flights ran behind, all a fetch on the copy queue
        // is known to have run behind.
        let behind = plans.iter().map(|p| p.0.sent_as).max().unwrap_or(0);
        if pieces.is_empty() {
            // Nothing to fetch: every send so far is older than the next
            // fetch, if that goes to the queue the sends went to.
            let after = match fetch.as_ref().is_some_and(|f| f.queue.is_some()) {
                true => behind,
                false => *QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()),
            };
            crate::shadow::relay_fetched(after);
            return;
        }
        let Some(f) = fetch.as_mut() else {
            fail(format_args!("no list, fence or event to fetch what shaders wrote to the card's memory (VICTORY_RELAY=1)"));
        };
        // A buffer that a minute of fetches never needed a quarter of is made
        // again at what they needed (`VICTORY_FETCH_TRIM`).
        let mut size = total.next_multiple_of(16 << 20);
        if crate::lean::fetch_trim()
            && let Some(have) = f.buffer.as_ref().map(|b| b.2)
            && let Some(smaller) = f.trim.ask(have, total, 16 << 20)
        {
            size = size.max(smaller);
            f.buffer = None;
            FETCH_TRIMMED.fetch_add(1, Ordering::Relaxed);
        }
        if f.buffer.as_ref().is_none_or(|b| b.2 < total) {
            f.buffer = self.shared_buffer(size).map(|(buffer, data)| (buffer, data as usize, size));
        }
        FETCH_BYTES[0].store(f.buffer.as_ref().map_or(0, |b| b.2), Ordering::Relaxed);
        let Some((buffer, data, _)) = f.buffer.clone() else {
            fail(format_args!("no memory to fetch what shaders wrote to the card's memory ({total} bytes, VICTORY_RELAY=1)"));
        };
        f.sent += 1;
        let ((allocator, list), fence, event, sent) = (f.list.clone(), f.fence.clone(), f.event, f.sent);
        let copy_queue = f.queue.clone();
        // With the compute queue of the host (`VICTORY_ASYNC_COMPUTE=2`) the fetch reads the copies only after the
        // submissions of that queue in the air that write them, and those that come later wait for it ([`lanes`]).
        let behind_compute = {
            let mut copies: Vec<u64> = check.as_ref().map_or(&pieces, |c| &c.copies).iter().map(|p| p.0.id()).collect();
            copies.sort_unstable();
            copies.dedup();
            lanes::fetching(self, &copies, &fence, sent)
        };
        let waiting = std::time::Instant::now();
        // The flights the fetch runs behind: those in the line by now have
        // been given to the queue. On the copy queue, those landed here.
        let ahead = match copy_queue {
            Some(_) => 0,
            None => line().target,
        };
        // SAFETY: the list's last run was waited for by this thread; live
        // buffers (the copies are held by the plans); the fence and the
        // event are this thread's.
        let ran = unsafe {
            (|| -> windows::core::Result<u64> {
                allocator.Reset()?;
                list.Reset(&allocator, None)?;
                for (shadow, lo, hi, at) in check.as_ref().map_or(&pieces, |c| &c.copies) {
                    list.CopyBufferRegion(&buffer, *at, &shadow.buffer, *lo as u64, (*hi - *lo) as u64);
                }
                list.Close()?;
                let lists = [Some(list.cast::<ID3D12CommandList>()?)];
                let _fetching = crate::timeline::span("landing: fetching pieces");
                if let Some(queue) = &copy_queue {
                    if let Some((compute, value)) = behind_compute {
                        queue.Wait(compute, value)?;
                    }
                    queue.ExecuteCommandLists(&lists);
                    queue.Signal(&fence, sent)?;
                    meanwhile();
                    fence.SetEventOnCompletion(sent, event)?;
                    #[cfg(feature = "diagnostics")]
                    let _card = crate::timeline::card_wait(None);
                    WaitForSingleObject(event, INFINITE);
                    return Ok(behind);
                }
                // In the order of the queue: the sends of title bytes so
                // far are ahead of this fetch, every later one behind it.
                let order = QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((compute, value)) = behind_compute {
                    self.queue.Wait(compute, value)?;
                }
                self.queue.ExecuteCommandLists(&lists);
                let after = *order;
                drop(order);
                self.card_sent();
                self.queue.Signal(&fence, sent)?;
                meanwhile();
                fence.SetEventOnCompletion(sent, event)?;
                #[cfg(feature = "diagnostics")]
                let _card = crate::timeline::card_wait(None);
                WaitForSingleObject(event, INFINITE);
                Ok(after)
            })()
        };
        let after = match ran {
            Ok(after) => after,
            Err(e) => fail(format_args!("what shaders wrote to the card's memory could not be fetched ({e}, VICTORY_RELAY=1)")),
        };
        let us = waiting.elapsed().as_micros() as u64;
        RELAY_STATS[3].fetch_add(us, Ordering::Relaxed);
        {
            use crate::shadow::CARD_STATS;
            CARD_STATS[3].fetch_add(pieces.len() as u64, Ordering::Relaxed);
            CARD_STATS[4].fetch_add(total, Ordering::Relaxed);
            CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
            CARD_STATS[6].fetch_add(us, Ordering::Relaxed);
        }
        let merging = std::time::Instant::now();
        let _merging = crate::timeline::span("landing: merging fetched pieces");
        let mut written = 0;
        // The tail's landing: what the title was seen to read is landed at
        // once, what it was not is left owed ([`crate::land::owed`]). What
        // cannot be armed is landed here, at once.
        let mut had_keys: Option<Vec<(u64, usize, usize)>> = None;
        let mut tail_bytes = (0u64, 0u64);
        if land::in_tail() && land::on() {
            for (shadow, lo, hi, _) in &pieces {
                let n = (hi - lo) as u64;
                if shadow.title_read() { tail_bytes.0 += n } else { tail_bytes.1 += n }
            }
            land::count(land::stat::TAIL_READ_BYTES, tail_bytes.0);
            land::count(land::stat::TAIL_UNREAD_BYTES, tail_bytes.1);
        }
        if land::owed::on() && land::on() && land::in_tail() && check.is_none() {
            had_keys = Some(pieces.iter().map(|(shadow, lo, hi, _)| (shadow.id(), shadow.span().0 as usize + lo, hi - lo)).collect());
            let (eager, owed): (Vec<_>, Vec<_>) = std::mem::take(&mut pieces).into_iter().partition(|p| !p.0.lazy_landing());
            pieces = eager;
            let spans: Vec<(usize, usize)> = owed
                .iter()
                .map(|(shadow, lo, hi, _)| (shadow.span().0 as usize + lo, shadow.span().0 as usize + hi))
                .collect();
            let grouped = land::owed::groups(&spans);
            let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); grouped.len()];
            for (i, &(a, _)) in spans.iter().enumerate() {
                buckets[grouped.partition_point(|g| g.1 <= a)].push(i);
            }
            let mut units: Vec<(usize, usize, land::owed::Land, Box<dyn Fn() + Send + Sync>)> = Vec::new();
            for (&(from, to), bucket) in grouped.iter().zip(&buckets) {
                let runs: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, usize)> =
                    bucket.iter().map(|&i| (owed[i].0.clone(), owed[i].1, owed[i].2, data + owed[i].3 as usize)).collect();
                let mut copies: Vec<std::sync::Arc<crate::shadow::Shadow>> = Vec::new();
                for run in &runs {
                    if !copies.iter().any(|c| std::sync::Arc::ptr_eq(c, &run.0)) {
                        copies.push(run.0.clone());
                    }
                }
                let landing: land::owed::Land = Box::new(move || {
                    let mut notes = land::Notes::default();
                    for run in runs.chunk_by(|a, b| std::sync::Arc::ptr_eq(&a.0, &b.0)) {
                        let addressed: Vec<(usize, usize, usize)> = run.iter().map(|(_, lo, hi, now)| (*lo, *hi, *now)).collect();
                        // SAFETY: the fetched bytes, which stay (the buffer is
                        // not used again before `finish_all`); the copies are
                        // held by the unit.
                        unsafe { run[0].0.land_card_with(&addressed, after, &mut notes, true) };
                    }
                    notes.flush();
                    if land::owed::check_on() {
                        for (shadow, lo, hi, now) in &runs {
                            land::owed::verify(shadow.span().0 as usize + lo, *now as *const u8, hi - lo);
                        }
                    }
                });
                let touched: Box<dyn Fn() + Send + Sync> = Box::new(move || copies.iter().for_each(|c| c.note_title_read()));
                units.push((from, to, landing, touched));
            }
            // Armed; those that could not be are landed here and now.
            for landing in land::owed::register(units) {
                landing();
            }
        }
        if land::on() {
            let titles: Vec<(usize, usize)> =
                pieces.iter().map(|(shadow, lo, hi, _)| (shadow.span().0 as usize + lo, hi - lo)).collect();
            let jobs = land::jobs(&titles);
            let put = std::sync::atomic::AtomicU64::new(0);
            land::each(jobs.len(), &|j| {
                let working = std::time::Instant::now();
                if let Some(check) = &check {
                    for i in jobs[j].clone() {
                        // SAFETY: as below.
                        unsafe { check.look(i, data) };
                    }
                }
                let mut notes = land::Notes::default();
                for run in pieces[jobs[j].clone()].chunk_by(|a, b| std::sync::Arc::ptr_eq(&a.0, &b.0)) {
                    let addressed: Vec<(usize, usize, usize)> =
                        run.iter().map(|(_, lo, hi, at)| (*lo, *hi, data + *at as usize)).collect();
                    // SAFETY: the mapping of the fetched bytes, `total` of
                    // them; the copy into it has run, and the next waits
                    // for this call.
                    put.fetch_add(unsafe { run[0].0.land_card(&addressed, after, &mut notes) }, Ordering::Relaxed);
                }
                notes.flush();
                land::spent(land::stat::CARD_ALL, working);
            });
            land::count(land::stat::JOBS, jobs.len() as u64);
            written = put.into_inner();
        } else {
            let mut title = Vec::new();
            for (i, (shadow, lo, hi, at)) in pieces.iter().enumerate() {
                if let Some(check) = &check {
                    // SAFETY: as below.
                    unsafe { check.look(i, data) };
                }
                // SAFETY: the mapping of the fetched bytes, `total` of them;
                // the copy into it has run.
                let gpu = unsafe { std::slice::from_raw_parts((data + *at as usize) as *const u8, hi - lo) };
                let merged = shadow.card_merge_relay(*lo, gpu, after, &mut title);
                shadow.walls_merged(gpu.len(), merged);
                written += merged as u64;
            }
        }
        land::spent(land::stat::CARD, merging);
        if tail_bytes.0 + tail_bytes.1 > 0 {
            let us = merging.elapsed().as_micros() as u64;
            land::count(land::stat::TAIL_UNREAD_US, us * tail_bytes.1 / (tail_bytes.0 + tail_bytes.1));
        }
        COPY_BACK[1].fetch_add(written, Ordering::Relaxed);
        if let Some(f) = fetch.as_mut() {
            if let Some(check) = &check {
                check.done(&mut f.suspects);
            }
            // (What was fetched, whether it is landed yet or not.)
            let keys: Vec<(u64, usize, usize)> = had_keys
                .unwrap_or_else(|| pieces.iter().map(|(shadow, lo, hi, _)| (shadow.id(), shadow.span().0 as usize + lo, hi - lo)).collect());
            f.had.fetched(ahead, keys.into_iter());
        }
        // Sends up to `after` are of no use to any later merge.
        crate::shadow::relay_fetched(after);
    }

    /// [`relay`], on the thread that lands flights: `items` are the first
    /// things of the line, flights the card has run and what follows them.
    /// What their shaders wrote goes into title memory (copies in the
    /// card's memory first, then the others, each byte once whichever
    /// flights declared it); then every flight's ranges are done with;
    /// then, in the line's order, each flight's read-backs, its
    /// bottom-of-pipe writes and the fence signals behind it. Title memory
    /// is complete before the title is told anything.
    pub(super) fn relay_land(&self, items: Vec<Late>, tail: bool, fetch: &mut Option<RelayFetch>) {
        // What an earlier landing still owes is in before this one begins, and
        // so before any signal behind it is told.
        crate::land::owed::finish_all();
        let landing = std::time::Instant::now();
        let _tail = crate::land::tail(tail);
        let (mut card_us, mut pc_us) = (0u64, 0u64);
        let mut stage = crate::timeline::span("landing: what is to be looked through");
        {
            let flights: Vec<&Flight> = items
                .iter()
                .filter_map(|l| match l {
                    Late::Run(flight) => Some(flight),
                    _ => None,
                })
                .collect();
            if !flights.is_empty() {
                // SAFETY: a plain query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    let runs: Vec<&HostList> = flights.iter().flat_map(|f| f.order.iter().map(|&i| f.lists[i].host())).collect();
                    fail(format_args!(
                        "host GPU device removed after {} submissions sent ahead (VICTORY_RELAY=1) of {} lists{}{}",
                        flights.len(),
                        runs.len(),
                        self.removal_data(&runs),
                        last_notes(&runs)
                    ));
                }
                let plans: Vec<(&CardPlan, u64)> = flights.iter().filter_map(|f| Some((f.card.as_ref()?, f.fence))).collect();
                // (Each byte once: the ranges are joined.)
                let mut back: Vec<Declared> = Vec::new();
                for flight in &flights {
                    for (shadow, ranges) in flight.back.iter().filter(|(shadow, _)| !shadow.carded()) {
                        match back.iter_mut().find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow)) {
                            Some((_, all)) => all.extend_from_slice(ranges),
                            None => back.push((shadow.clone(), ranges.clone())),
                        }
                    }
                }
                let mut hand_back = relay_hand_back(&back);
                stage.next("landing: copies in the card's memory");
                let began = std::time::Instant::now();
                self.relay_card_back(&plans, fetch, &mut || {
                    if crate::land::on() {
                        let _looking = crate::timeline::span("landing: looking through copies, beside the fetch");
                        hand_back.look(crate::land::stat::SCAN_BESIDE);
                    }
                });
                card_us = began.elapsed().as_micros() as u64;
                stage.next("landing: copies in the PC's memory");
                let began = std::time::Instant::now();
                hand_back.merge();
                pc_us = began.elapsed().as_micros() as u64;
                stage.next("landing: ranges done with");
                let finishing = std::time::Instant::now();
                // Only now, and flight by flight: a range another flight
                // in the air declared too stays closed to refreshes.
                for flight in &flights {
                    for (shadow, ranges) in &flight.back {
                        shadow.copy_back_done(ranges);
                    }
                }
                crate::land::spent(crate::land::stat::DONE, finishing);
                COPY_BACK[0].fetch_add(landing.elapsed().as_micros() as u64, Ordering::Relaxed);
                RELAY_STATS[0].fetch_add(1, Ordering::Relaxed);
                RELAY_STATS[1].fetch_add(flights.len() as u64, Ordering::Relaxed);
                // Flights whose landing has nothing for the title to read (no
                // card copies, declared writes, read-backs of buffers or
                // textures): the title could be told of them as soon as the
                // card is done.
                let bare = flights
                    .iter()
                    .filter(|f| {
                        f.card.is_none()
                            && f.back.is_empty()
                            && f.texture_back_ranges.is_empty()
                            && f.order.iter().all(|&i| {
                                let d = f.lists[i].host().deferred();
                                d.backs.is_empty() && d.texture_backs.is_empty()
                            })
                    })
                    .count();
                RELAY_STATS[5].fetch_add(bare as u64, Ordering::Relaxed);
            }
        }
        // In the order given: what tells the title the card is done.
        stage.next("landing: read-backs, telling the title");
        let mut last = 0;
        let mut landed = Vec::new();
        let mut telling = std::time::Instant::now();
        for item in items {
            telling = match item {
                Late::Run(mut flight) => {
                    last = flight.number;
                    for &i in &flight.order {
                        read_backs(flight.lists[i].host());
                    }
                    if let Some(card) = flight.card {
                        self.card_done(card);
                    }
                    if let Some(own) = flight.own {
                        own.landed();
                    }
                    for detached in &mut flight.lists {
                        detached.release_tables();
                    }
                    landed.push((flight.lists, flight.order));
                    crate::land::spent(crate::land::stat::READ_BACKS, telling)
                }
                Late::Write(at, what, bytes) => {
                    what.write(at as *mut u8, bytes);
                    crate::land::spent(crate::land::stat::TOLD, telling)
                }
                Late::Signal(fence, value) => {
                    crate::fence::set(fence as crate::object::This, value);
                    crate::land::spent(crate::land::stat::TOLD, telling)
                }
            };
        }
        // The title is told: what was left owed is landed now, here and by the
        // landing's threads, unless a thread of the title has landed it.
        crate::land::owed::run_pending();
        if last > 0 {
            FINISHED.store(last, Ordering::Release);
            bury(last);
        }
        stage.next("landing: the rest");
        self.log_messages();
        crate::land::spent(crate::land::stat::REST, telling);
        RELAY_STATS[4].fetch_add(landing.elapsed().as_micros() as u64, Ordering::Relaxed);
        if tail {
            use crate::land::{count, stat};
            count(stat::TAIL_N, 1);
            count(stat::TAIL_US, landing.elapsed().as_micros() as u64);
            count(stat::TAIL_CARD_US, card_us);
            count(stat::TAIL_PC_US, pc_us);
        }
        {
            let mut line = line();
            let Line {
                line: items,
                written,
                lists,
                settling,
                poke,
                landed: kept,
                in_hand,
                landing,
                pending,
                ..
            } = &mut *line;
            kept.extend(landed);
            in_hand.clear();
            landing.clear();
            // What is still in the air, from the flights that are (and the
            // submissions registered, which are not flights yet).
            *lists = 0;
            let mut ranges = Vec::new();
            for item in items.iter() {
                if let Late::Run(flight) = item {
                    *lists += flight.order.len().max(1);
                    ranges.extend(flight.back.iter().flat_map(|(_, r)| r.iter().copied()));
                    ranges.extend(flight.texture_back_ranges.iter().copied());
                }
            }
            rebuilt(written, ranges, pending);
            *settling = false;
            // The landed lists are reset, and what queues hold looked at,
            // by the helper thread (they take locks a waiter may hold).
            *poke = true;
        }
        LINE_MOVED.notify_all();
        LINE_LANDED.notify_all();
    }

    /// After a submission ran, for copies in the card's memory: the pieces
    /// of the declared ranges that the compare found differing (all of a
    /// range it did not compare) are fetched, with one more wait for the
    /// card, and what shaders wrote in them goes into title memory.
    /// `submitted` is the last value signalled on the fence. `seen` holds
    /// the pieces fetched already since the card was last waited for
    /// (several submissions settled together name the same ones).
    pub(super) fn card_back(
        &self,
        plan: &CardPlan,
        submitted: &mut u64,
        seen: &mut std::collections::HashSet<(usize, usize, usize)>,
    ) {
        if plan.ranges.is_empty() {
            return;
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(work) = work.as_mut() else {
            return;
        };
        // SAFETY: the mapping of the results, which the compare (it has
        // run) wrote.
        let differs = unsafe { std::slice::from_raw_parts(work.differs.1 as *const u32, (work.differs.2 / 4) as usize) };
        // The pieces to fetch: the range, the offsets in its copy, and
        // where the bytes come down.
        let mut pieces: Vec<(usize, usize, usize, u64)> = Vec::new();
        let mut total = 0u64;
        plan.picked_found(differs);
        plan.check_gpu_ro(differs);
        for (i, r) in plan.ranges.iter().enumerate().filter(|(_, r)| !r.picked) {
            for p in 0..r.span.pieces {
                if r.compared && !torn::flagged(differs[(r.at + p) as usize]) {
                    continue;
                }
                let (lo, hi) = r.shadow.card_piece(&r.span, p);
                if lo < hi && seen.insert((std::sync::Arc::as_ptr(&r.shadow) as usize, lo, hi)) {
                    pieces.push((i, lo, hi, total));
                    total += (hi - lo) as u64;
                }
            }
            crate::shadow::CARD_STATS[2].fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
        }
        let mut written = vec![0usize; plan.ranges.len()];
        if !pieces.is_empty() {
            let mut size = total.next_multiple_of(16 << 20);
            if crate::lean::fetch_trim()
                && let Some(have) = work.fetched.as_ref().map(|f| f.2)
                && let Some(smaller) = work.fetched_trim.ask(have, total, 16 << 20)
            {
                size = size.max(smaller);
                work.fetched = None;
                FETCH_TRIMMED.fetch_add(1, Ordering::Relaxed);
            }
            if work.fetched.as_ref().is_none_or(|f| f.2 < total) {
                work.fetched = self.shared_buffer(size).map(|(buffer, data)| (buffer, data as usize, size));
            }
            FETCH_BYTES[1].store(work.fetched.as_ref().map_or(0, |f| f.2), Ordering::Relaxed);
            let Some((fetched, data, _)) = &work.fetched else {
                fail(format_args!("no memory to fetch what shaders wrote to the card's memory ({total} bytes)"));
            };
            let waiting = std::time::Instant::now();
            let (allocator, list) = &work.fetch;
            // SAFETY: the list's last run was waited for; live buffers; the
            // fence and event are ours, under the submission lock.
            let ran = unsafe {
                (|| -> windows::core::Result<()> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    for &(i, lo, hi, at) in &pieces {
                        list.CopyBufferRegion(fetched, at, &plan.ranges[i].shadow.buffer, lo as u64, (hi - lo) as u64);
                    }
                    list.Close()?;
                    self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                    self.card_sent();
                    *submitted += 1;
                    self.queue.Signal(&self.fence, *submitted)?;
                    self.fence.SetEventOnCompletion(*submitted, self.event)?;
                    let was_working = working(false);
                    WaitForSingleObject(self.event, INFINITE);
                    stood(waiting);
                    working(was_working);
                    Ok(())
                })()
            };
            if let Err(e) = ran {
                fail(format_args!("what shaders wrote to the card's memory could not be fetched ({e})"));
            }
            {
                use crate::shadow::CARD_STATS;
                CARD_STATS[3].fetch_add(pieces.len() as u64, Ordering::Relaxed);
                CARD_STATS[4].fetch_add(total, Ordering::Relaxed);
                CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
                CARD_STATS[6].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            if wide_merge() && pieces.len() >= 8 {
                // The pieces are apart from one another (each its own bytes
                // of its copy and of title memory): several at a time.
                let fetched = *data;
                let counts: std::sync::Arc<Vec<std::sync::atomic::AtomicUsize>> =
                    std::sync::Arc::new((0..plan.ranges.len()).map(|_| std::sync::atomic::AtomicUsize::new(0)).collect());
                let each = pieces.len().div_ceil(8);
                let jobs: Vec<Box<dyn FnOnce() + Send>> = pieces
                    .chunks(each)
                    .map(|chunk| {
                        let chunk: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, usize, u64)> =
                            chunk.iter().map(|&(i, lo, hi, at)| (plan.ranges[i].shadow.clone(), i, lo, hi, at)).collect();
                        let counts = counts.clone();
                        Box::new(move || {
                            let mut title = Vec::new();
                            for (shadow, i, lo, hi, at) in chunk {
                                // SAFETY: the mapping of the fetched bytes;
                                // the copy into it has run, and the caller
                                // waits for this job.
                                let gpu = unsafe { std::slice::from_raw_parts((fetched + at as usize) as *const u8, hi - lo) };
                                let merged = shadow.card_merge(lo, gpu, &mut title);
                                shadow.walls_merged(gpu.len(), merged);
                                counts[i].fetch_add(merged, Ordering::Relaxed);
                            }
                        }) as Box<dyn FnOnce() + Send>
                    })
                    .collect();
                crate::shadow::wide_run(jobs);
                for (written, count) in written.iter_mut().zip(counts.iter()) {
                    *written += count.load(Ordering::Relaxed);
                }
            } else {
                let mut title = Vec::new();
                for &(i, lo, hi, at) in &pieces {
                    // SAFETY: the mapping of the fetched bytes, `total` of
                    // them; the copy into it has run.
                    let gpu = unsafe { std::slice::from_raw_parts((*data + at as usize) as *const u8, hi - lo) };
                    let merged = plan.ranges[i].shadow.card_merge(lo, gpu, &mut title);
                    plan.ranges[i].shadow.walls_merged(gpu.len(), merged);
                    written[i] += merged;
                }
            }
        }
        for (r, written) in plan.ranges.iter().zip(written) {
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            let (from, to) = r.declared;
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(from, to - from, written);
            let _ = (from, to);
        }
    }
}
