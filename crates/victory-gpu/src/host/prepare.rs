//! The stages of a submission: the late and own sends, the settling of flights, `Host::submit` and the report for a removed device.

use super::*;

impl Host {
    /// A queue's submission with [`late`]: the host lists `lists`, taken
    /// from the title's, go to the card in `order` and are not waited for.
    /// They join the line of flights and are settled, with the others in
    /// the air, at the next point where what they do can be observed
    /// ([`late`] lists those points); their host lists are reset and kept
    /// for reuse then, as a queue does for the lists it ran.
    pub(crate) fn execute_late(&self, lists: Vec<crate::list::Detached>, order: Vec<usize>) {
        self.execute_parts(lists, order, &[]);
    }

    /// [`Host::execute_late`] of parts of submissions in one: another begins
    /// at each place of `order` in `parts` ([`joins`]).
    pub(crate) fn execute_parts(&self, lists: Vec<crate::list::Detached>, order: Vec<usize>, parts: &[usize]) {
        if order.is_empty() {
            crate::list::recycle(lists, &order);
            return;
        }
        late_threads();
        // The thread of stage A is done with the submission before any decision is made about it (and the one that
        // asks and refreshes the entry after it: every copy is the lane's again).
        front::join();
        front2::join();
        if let Some(cut) = {
            let runs: Vec<&HostList> = order.iter().map(|&i| lists[i].host()).collect();
            card_steps::cut_for(&runs)
        } {
            match split(lists, order, cut) {
                Ok([(lists, order), (more, behind)]) => {
                    let first: Vec<usize> = parts.iter().copied().filter(|&p| p < cut).collect();
                    let second: Vec<usize> = parts.iter().filter(|&&p| p > cut).map(|p| p - cut).collect();
                    let ahead = crate::queue::take_ahead();
                    // (Nothing is offered to the thread of stage A while the first half is sent: the line's first entry
                    // is not what comes next, the second half is.)
                    let half = front::split_pending();
                    self.execute_parts(lists, order, &first);
                    drop(half);
                    crate::queue::restore_ahead(ahead);
                    self.execute_parts(more, behind, &second);
                }
                Err((lists, order)) => self.send_late(lists, order, parts),
            }
            return;
        }
        if let Some(cut) = blocked_cut(&lists, &order, parts) {
            match split(lists, order, cut) {
                Ok([(lists, order), (more, behind)]) => {
                    let (first, second) = parts_around(parts, cut);
                    let ahead = crate::queue::take_ahead();
                    let half = front::split_pending();
                    self.execute_parts(lists, order, &first);
                    drop(half);
                    crate::queue::restore_ahead(ahead);
                    self.execute_parts(more, behind, &second);
                }
                Err((lists, order)) => self.send_late(lists, order, parts),
            }
            return;
        }
        self.send_late(lists, order, parts);
    }

    pub(super) fn send_late(&self, lists: Vec<crate::list::Detached>, order: Vec<usize>, parts: &[usize]) {
        let sent = {
            let runs: Vec<&HostList> = order.iter().map(|&i| lists[i].host()).collect();
            self.submit(&runs, parts, true)
        };
        let Some((mut submitted, mut sent)) = sent else {
            // (Not reached: a submission sent ahead always comes back.)
            crate::list::recycle(lists, &order);
            return;
        };
        let told = !sent.writes.is_empty();
        let sizes = crate::memory::asked_sizes(order.iter().map(|&i| lists[i].host()));
        let most = {
            let mut line = line();
            let pending = sent.pending.take();
            let Line { line: items, written, lists: flying, target, pending: registered, pending_copies, .. } = &mut *line;
            join_ranges(
                written,
                sent.back
                    .iter()
                    .flat_map(|(_, ranges)| ranges.iter().copied())
                    .chain(sent.texture_back_ranges.iter().copied()),
            );
            *flying += order.len();
            *target = *submitted;
            items.push(Late::Run(Flight {
                number: sent.number,
                fence: *target,
                lists,
                order,
                card: sent.card,
                back: sent.back,
                own: sent.own,
                texture_back_ranges: sent.texture_back_ranges,
            }));
            items.extend(sent.writes.into_iter().map(|(at, what, bytes)| Late::Write(at, what, bytes)));
            if let Some(pending) = pending {
                pending.done(registered, pending_copies);
            }
            *flying >= late_most()
        };
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        RELAY_MOVED.notify_one();
        if told && !late_writes() {
            self.settle(&mut submitted, WHY_WRITES);
        } else if sizes {
            self.settle(&mut submitted, WHY_READ);
        } else if most && relay_exact() {
            // Until there are fewer, not until there are none.
            relay_wait(WHY_MOST, |line| line.lists < late_most());
        } else if most {
            self.settle(&mut submitted, WHY_MOST);
        } else {
            LINE_MOVED.notify_one();
        }
        drop(submitted);
        if land_aside() && HELPER_UP.load(Ordering::Acquire) {
            // The thread that settles flights gives the landed lists back, as it does when it is asked to look
            // at what is in the air: it resets nearly all of them already (1,025 of 1,263 in a timeline).
            line().poke = true;
            LINE_MOVED.notify_one();
        } else {
            let _landing = crate::timeline::span("landed lists reset and given back");
            land();
        }
    }

    /// Sends `list`, a closed list of ours (the present's copy), as a
    /// flight ([`late_free_present`]): not waited for, settled with the
    /// others. The caller keeps the list and does not reset it before the
    /// submission whose number comes back has finished ([`FINISHED`]); 0 if
    /// it ran and was waited for after all.
    pub(super) fn execute_own(&self, list: &HostList) -> u64 {
        late_threads();
        let Some((submitted, mut sent)) = self.submit(&[list], &[], true) else {
            return 0;
        };
        let number = sent.number;
        {
            let mut line = line();
            let pending = sent.pending.take();
            let Line { line: items, written, lists: flying, target, pending: registered, pending_copies, .. } = &mut *line;
            join_ranges(
                written,
                sent.back
                    .iter()
                    .flat_map(|(_, ranges)| ranges.iter().copied())
                    .chain(sent.texture_back_ranges.iter().copied()),
            );
            *flying += 1;
            *target = *submitted;
            items.push(Late::Run(Flight {
                number,
                fence: *target,
                lists: Vec::new(),
                order: Vec::new(),
                card: sent.card,
                back: sent.back,
                own: sent.own,
                texture_back_ranges: sent.texture_back_ranges,
            }));
            items.extend(sent.writes.into_iter().map(|(at, what, bytes)| Late::Write(at, what, bytes)));
            if let Some(pending) = pending {
                pending.done(registered, pending_copies);
            }
        }
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        LINE_MOVED.notify_one();
        RELAY_MOVED.notify_one();
        drop(submitted);
        number
    }

    /// Waits for the card to run every flight in the air ([`late`]) and
    /// does what they left to do, as [`Host::execute`] does for one
    /// submission after its wait: the pieces their shaders wrote in copies
    /// in the card's memory are fetched and merged (each once), the ranges
    /// they declared in the other copies are compared and copied back
    /// (each byte once, whichever flights declared it), and then, in the
    /// order they were given: their bottom-of-pipe writes and the fence
    /// signals behind them. `submitted` is the submission lock's value;
    /// `why` one of the `WHY_` places. Nothing happens if nothing flies.
    ///
    /// Everything in the air is settled, never the first flights alone:
    /// the merge writes the copies' CPU side, which a flight still on the
    /// card may be writing too.
    pub(super) fn settle(&self, submitted: &mut u64, why: usize) {
        if !late() {
            return;
        }
        if relay() {
            // Nothing is settled here: the thread that lands flights does
            // it, in order, and this waits until nothing is in the air and
            // nothing is being landed. Every point that settled before
            // comes here, so each still sees title memory, fences and
            // bottom-of-pipe values as after a settle.
            relay_wait(why, |line| line.line.is_empty() && !line.settling);
            return;
        }
        let was_working = working(true);
        let _settling = crate::timeline::span("settling flights");
        let mut counted = false;
        loop {
            let items = {
                let mut line = line();
                if line.line.is_empty() {
                    line.settling = false;
                    break;
                }
                line.settling = true;
                line.rebuild_written(std::iter::empty());
                line.lists = 0;
                std::mem::take(&mut line.line)
            };
            let flights = items.iter().filter(|i| matches!(i, Late::Run(_))).count();
            if !counted {
                counted = true;
                LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
                LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
            }
            LATE_STATS[11].fetch_add(flights as u64, Ordering::Relaxed);
            if flights > 0 {
                submitting(2, flights);
                let waiting = std::time::Instant::now();
                // SAFETY: the fence and event are ours, under the
                // submission lock; `submitted` is the last value signalled.
                unsafe {
                    match self.fence.SetEventOnCompletion(*submitted, self.event) {
                        Ok(()) => {
                            working(false);
                            #[cfg(feature = "diagnostics")]
                            let _card = crate::timeline::card_wait(Some(*submitted));
                            WaitForSingleObject(self.event, INFINITE);
                            stood(waiting);
                            working(true);
                        }
                        Err(e) => tracing::warn!(error = %e, "submissions sent ahead not waited for"),
                    }
                }
                let gpu = waiting.elapsed();
                LATE_STATS[12].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                LATE_WAITED[why.min(10)].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                crate::copystat::card_waited(gpu.as_micros() as u64, || {
                    format!("{flights} submissions sent ahead, settled together")
                });
                // SAFETY: a plain query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    let runs: Vec<&HostList> = items
                        .iter()
                        .filter_map(|i| match i {
                            Late::Run(f) => Some(f),
                            _ => None,
                        })
                        .flat_map(|f| f.order.iter().map(|&i| f.lists[i].host()))
                        .collect();
                    fail(format_args!(
                        "host GPU device removed after {flights} submissions sent ahead (VICTORY_LATE_WAIT=1) of {} lists{}{}",
                        runs.len(),
                        self.removal_data(&runs),
                        last_notes(&runs)
                    ));
                }
                submitting(3, flights);
                let copying = std::time::Instant::now();
                let mut seen = std::collections::HashSet::new();
                let mut back: Vec<Declared> = Vec::new();
                for item in &items {
                    let Late::Run(flight) = item else {
                        continue;
                    };
                    if let Some(card) = &flight.card {
                        self.card_back(card, submitted, &mut seen);
                    }
                    for (shadow, ranges) in &flight.back {
                        match back.iter_mut().find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow)) {
                            Some((_, all)) => {
                                for range in ranges {
                                    if !all.contains(range) {
                                        all.push(*range);
                                    }
                                }
                            }
                            None => back.push((shadow.clone(), ranges.clone())),
                        }
                    }
                }
                hand_back(&back);
                COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            // In the order given: what tells the title the card is done.
            let mut last = 0;
            let mut landed = Vec::new();
            for item in items {
                match item {
                    Late::Run(mut flight) => {
                        last = flight.number;
                        for &i in &flight.order {
                            read_backs(flight.lists[i].host());
                        }
                        if let Some(card) = flight.card {
                            self.card_done(card);
                        }
                        for detached in &mut flight.lists {
                            detached.release_tables();
                        }
                        landed.push((flight.lists, flight.order));
                    }
                    Late::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
                    Late::Signal(fence, value) => crate::fence::set(fence as crate::object::This, value),
                }
            }
            if last > 0 {
                FINISHED.store(last, Ordering::Release);
                bury(last);
            }
            line().landed.extend(landed);
        }
        if counted {
            self.log_messages();
            submitting(0, 0);
            // What was settled may be what a queue stands at, and the
            // caller may not be one that looks (a read-back, a present).
            if why != WHY_CARD_DONE {
                line().poke = true;
                LINE_MOVED.notify_one();
            }
        }
        working(was_working);
    }

    /// For a submission of `runs` that refreshes the ranges `stale` of host
    /// copies, which flights may be writing and the title wrote too
    /// ([`stale_under_flights`]): the flights land first. `ranges` are
    /// those of the submission's one asking, which is made again.
    ///
    /// With [`card_steps`] only the flights that declared them: a range of
    /// a copy nobody declared any more is refreshed like any other. What
    /// they landed is title memory written, so everything is asked and
    /// looked at again; what is left after three times waits as before.
    pub(super) fn settle_stale(
        &self,
        submitted: &mut u64,
        runs: &[&HostList],
        mut stale: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
        ranges: &[(usize, usize)],
        asking: &mut Option<crate::watch::Standing>,
        mut pass: Option<&mut onepass::Pass>,
    ) {
        if relay_exact() && card_steps::stage() > 0 {
            for _ in 0..3 {
                #[cfg(feature = "diagnostics")]
                blame_stale(&stale);
                relay_wait(WHY_STALE, |_| stale.iter().all(|(shadow, from, to)| !shadow.writes_pending_in(*from, *to)));
                ask_for_submission(ranges, asking);
                stale = stale_of(runs, pass.as_deref_mut());
                if stale.is_empty() {
                    return;
                }
            }
        }
        #[cfg(feature = "diagnostics")]
        blame_stale(&stale);
        self.settle(submitted, WHY_STALE);
        ask_for_submission(ranges, asking);
    }

    /// [`Host::execute`], or with `late` the first half of
    /// [`Host::execute_late`]: the lists are prepared and sent, and what is
    /// to be done once they have run comes back with the submission lock
    /// still held.
    ///
    /// `parts`: the lists of `runs` at which another part begins
    /// ([`joins`]): those go to the queue in calls of their own, one after the
    /// other, as they would have.
    pub(super) fn submit<'a>(&'a self, runs: &[&HostList], parts: &[usize], late: bool) -> Option<(std::sync::MutexGuard<'a, u64>, Sent)> {
        #[cfg(feature = "diagnostics")]
        let called = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::submission_begins();
        // One submission at a time from preparation to copy-back: snapshots
        // and the ranges shaders write are per host copy, not per submission.
        // (A submission that is waited for takes its number after the one stage A prepared has taken its own: it gets
        // the lock when stage A is not asked for.)
        let mut stage = crate::timeline::span("submit: the submission lock");
        let lock = || self.submitted.lock().unwrap_or_else(|e| e.into_inner());
        let mut submitted = if late { lock() } else { front::lock_quiet(lock) };
        // Upscalings these lists recorded, in submission order: the next
        // present tags the latest (frame generation).
        for run in runs {
            let id = run.take_upscale();
            if id != 0 {
                crate::streamline::submitted(id);
            }
            let id = run.take_hudless();
            if id != 0 {
                crate::streamline::hudless_submitted(id);
            }
        }
        // The refreshes of this submission begun while the one before was sent
        // are over before anything is asked or refreshed here, and so is
        // everything stage A did of it (`VICTORY_STAGE_A_STEPS`): its asking, its
        // refreshes and its steps, which are not done again.
        let mut staged = if late {
            // (The thread of stage A first: what it offered to the thread of the refreshes, beside its steps
            // [`relax::deep`], is offered while it works, and is there to be waited for after it is done.)
            front::join();
            front2::join();
            prefetch::join();
            front::take(runs, parts)
        } else {
            None
        };
        let staged_number = staged.as_ref().map(front::Front::number);
        let was_working = working(true);
        submitting(1, runs.len());
        #[cfg(feature = "diagnostics")]
        let ahead = Split::begin();
        #[cfg(feature = "diagnostics")]
        SPLIT[11].fetch_add(called.elapsed().as_nanos() as u64, Ordering::Relaxed);
        // The write watch asked about what the lists refresh, on a thread of ours, while this one plans the steps it gives
        // to the card, waits and sweeps (`VICTORY_ASK_ASIDE`, on unless it is `0`, only with flights in the air: with none there is nothing to
        // go beside). What the planning adds to the lists is asked after; a wait for flights, which lands title memory,
        // makes the asking be made again after it (`flights_waited`).
        let mut aside: Option<askaside::Offered> = None;
        let mut aside_ranges: Vec<(usize, usize)> = Vec::new();
        let mut flights_waited = false;
        // The steps of the lists looked through once, for what to ask, what flights may write, what is refreshed in front
        // and the steps themselves (`VICTORY_ONE_PASS=1`, with `VICTORY_LEAN_STEPS`): read once for the submission.
        let mut pass: Option<scratch::Lean<onepass::Pass>> = (onepass::on() && lean_steps()).then(|| scratch::Lean::of(&onepass::PASS, true));
        if askaside::wanted(late, staged_number.is_some(), flying) {
            let ranges = match pass.as_mut() {
                Some(pass) => {
                    pass.add(runs);
                    let all = pass.lists();
                    pass.asks(0..all)
                }
                None => ask_spans(runs, lean_steps()),
            };
            if !ranges.is_empty() {
                aside = askaside::begin(ranges.clone());
                if aside.is_some() {
                    aside_ranges = ranges;
                }
            }
        }
        stage.next("submit: steps given to the card");
        // Flights in the air ([`late`]): a submission that is waited for
        // settles them first, one sent ahead only if preparing it reads
        // what they write.
        // Or the reads are done on the card, behind the flights.
        // Which queue the submission goes to (`VICTORY_ASYNC_COMPUTE=2`): lists of the compute type to the compute queue,
        // the others to the host's; with it on everything is joined to the host's fence by the sequencer ([`lanes`]).
        let mut route = lanes::Route::begin(self, runs, *submitted + 1);
        let on_compute = route.as_ref().is_some_and(|r| r.compute());
        // The steps given to the card: the thread of stage A planned them with the rest of the stage and the plan stands
        // (`VICTORY_STAGE_A_PLAN`: checked against the line, which must not have had a submission registered since), or they
        // are planned here.
        let planned = staged.as_mut().and_then(front::Front::take_plan);
        let own = match planned {
            Some((taken, generation)) => {
                plan::check(generation);
                Some(taken)
            }
            None if late && staged_number.is_none() => self.card_steps(runs, on_compute),
            None => None,
        };
        if let (Some(route), Some(own)) = (route.as_mut(), own.as_ref()) {
            route.add_list(own.list());
        }
        // The next submission's refreshes of the copies this one does nothing to begin on the thread of
        // `prefetch` now, beside what this one does up to its snapshots (`VICTORY_STAGE_A=1`); they are over
        // at `join_early`, before anything of this submission's that the card sees.
        if late && staged_number.is_none() {
            prefetch::offer_early(runs, own.as_ref().map(|own| own.list()));
        }
        // A round of the heaps' records asked for as the submission came into
        // the line (`VICTORY_HEAPS_EARLY`), unless it has to wait for flights.
        let mut early = if late { crate::queue::heaps_round() } else { 0 };
        #[cfg(feature = "diagnostics")]
        ahead.part(12);
        stage.next("submit: waiting for flights it reads from");
        if !late {
            self.settle(&mut submitted, WHY_SYNC);
        } else if own.is_none()
            && staged_number.is_none()
            && let Some(why) = late_blocked(runs)
        {
            // What the flights land may be descriptors: a round after it.
            early = 0;
            flights_waited = true;
            if relay_exact() && why == WHY_READ {
                // Only until the flights that may write what is read have
                // landed: their copy-back's end, not the card's other work.
                relay_wait(why, |line| {
                    blocked_by(&line.written, runs, false).is_none() && !card_steps::copied_read(&card_steps::copies(line), runs)
                });
            } else {
                self.settle(&mut submitted, why);
            }
        }
        #[cfg(feature = "diagnostics")]
        ahead.part(13);
        // Descriptors the title copied into placed heaps directly, before
        // command lists that index those slots are submitted.
        let heaps_synced = if early > 0 {
            crate::descriptor::SyncedHeaps::asked(early)
        } else {
            crate::descriptor::begin_sync_heaps()
        };
        // That list runs first, and is one of the submission's from here on.
        let with: Vec<&HostList>;
        let runs = match &own {
            Some(own) => {
                with = std::iter::once(own.list()).chain(runs.iter().copied()).collect();
                &with[..]
            }
            None => runs,
        };
        let parts: Vec<usize> = parts.iter().map(|&n| n + usize::from(own.is_some())).collect();
        stage.next("submit: the sweep");
        let behind = late && flying();
        // Before this submission counts as started: what the sweep lets go
        // of is named by no list, and nothing runs now (behind flights the
        // sweep waits for a submission that finds none in the air: there is
        // one at every present).
        static SWEPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if !behind {
            if upcheck::on() {
                upcheck::at_rest(self);
            }
            sweep();
            SWEPT.store(now_ms(), Ordering::Relaxed);
        } else if late_free_present() && now_ms() > SWEPT.load(Ordering::Relaxed) + 1500 {
            // With the present sent ahead nothing else brings a moment
            // with nothing in the air: one is made for the sweep.
            flights_waited = true;
            self.settle(&mut submitted, WHY_SYNC);
            if upcheck::on() {
                upcheck::at_rest(self);
            }
            sweep();
            SWEPT.store(now_ms(), Ordering::Relaxed);
        }
        let behind = late && flying();
        stage.next("submit: gathering what to ask about");
        let this_submission = match staged_number {
            Some(number) => number,
            None => STARTED.fetch_add(1, Ordering::AcqRel) + 1,
        };
        // The ranges shaders write are counted for it (with the relay).
        crate::shadow::preparing(this_submission);
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        ahead.part(14);
        #[cfg(feature = "diagnostics")]
        let lock = (started - called).as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::Submissions, 1);
        let mut lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(2 * runs.len());
        // Where each run's lists start in `lists`.
        let mut firsts = Vec::with_capacity(runs.len());
        let mut batch = Batch::new(lean_steps());
        if let Some(staged) = staged {
            batch.placed = staged.into_placed();
        }
        #[cfg(feature = "diagnostics")]
        let split = Split::begin();
        // The time a settle of flights takes inside this preparing: the
        // wait for the card and the copy-back have their own places on the
        // frame line (run 92 showed them twice, "preparing" 250 ms for 135).
        let mut settled = std::time::Duration::ZERO;
        // The asking of the write watch that stands for this submission
        // (`VICTORY_ASK_TOKENS=1`: one of several that may stand together).
        let mut asking: Option<crate::watch::Standing> = None;
        // The pass over the steps, in the order the lists go: with the asking begun beside the planning it has the title's
        // lists already, and the planning's own list goes in front of them; else it is made here (what was looked at
        // before the planning is let go).
        if let Some(pass) = pass.as_mut() {
            if aside.is_some() {
                if let Some(own) = &own {
                    pass.add_first(&own.list());
                }
            } else {
                scratch::Reset::reset(&mut **pass);
                pass.add(runs);
            }
        }
        // One write-watch asking for everything the submission refreshes
        // (`VICTORY_ASK_ONCE`): per copy, the span of its refreshed ranges.
        if staged_number.is_some() {
            // (Asked, refreshed and stepped by the thread of stage A.)
        } else if crate::watch::once() {
            // (With `VICTORY_LEAN_STEPS` the thread's kept table: the ranges are what it holds at the end.)
            // Asked beside the planning (`VICTORY_ASK_ASIDE`): the title's lists were gathered then, and what is
            // gathered now is the planning's own list, which comes first among `runs`.
            let (ranges, own_ranges) = if aside.is_some() {
                let own_ranges = if own.is_some() {
                    match pass.as_mut() {
                        Some(pass) => pass.asks(0..1),
                        None => ask_spans(&runs[..1], batch.lean),
                    }
                } else {
                    Vec::new()
                };
                let mut ranges = std::mem::take(&mut aside_ranges);
                ranges.extend(own_ranges.iter().copied());
                (ranges, own_ranges)
            } else {
                let ranges = match pass.as_mut() {
                    Some(pass) => {
                        let all = pass.lists();
                        pass.asks(0..all)
                    }
                    None => ask_spans(runs, batch.lean),
                };
                (ranges, Vec::new())
            };
            #[cfg(feature = "diagnostics")]
            SPLIT[8].fetch_add(split.so_far(), Ordering::Relaxed);
            stage.next("submit: asking the write watch");
            match aside.take() {
                Some(offered) => {
                    // The asking made beside the planning is over (a drop of the holder waits for it).
                    offered.join();
                    if flights_waited {
                        // What flights landed since is title memory written: everything is asked again, as it was
                        // after any wait.
                        askaside::redone();
                        ask_for_submission(&ranges, &mut asking);
                    } else if !own_ranges.is_empty() {
                        askaside::more(own_ranges.len());
                        crate::watch::ask_more(&own_ranges);
                    }
                }
                None => ask_for_submission(&ranges, &mut asking),
            }
            #[cfg(feature = "diagnostics")]
            split.part(0);
            stage.next("submit: title bytes under flights' writes");
            // Behind flights: the title wrote memory that a flight's shaders
            // may write too, and this submission refreshes it. The flights
            // are settled, and the asking made again: the copy-back wrote
            // title memory, which other copies of it must see.
            if upcheck::on() {
                upcheck::order(runs);
                if behind {
                    upcheck::reads(runs);
                }
            }
            let stale = if behind { stale_of(runs, pass.as_deref_mut()) } else { Vec::new() };
            if !stale.is_empty() {
                let settling = std::time::Instant::now();
                self.settle_stale(&mut submitted, runs, stale, &ranges, &mut asking, pass.as_deref_mut());
                settled += settling.elapsed();
            }
        } else if behind && !stale_of(runs, pass.as_deref_mut()).is_empty() {
            let settling = std::time::Instant::now();
            self.settle(&mut submitted, WHY_STALE);
            settled += settling.elapsed();
        }
        #[cfg(feature = "diagnostics")]
        split.part(1);
        // What flights in the air may write.
        let air = line().written.clone();
        stage.next("submit: refreshes ahead of writes");
        // (The records of the pass go with what is ahead by number of copy: they are used together or not at all.)
        // (Not with stage A: its thread does the front refreshes.)
        let front_pass = staged_number.is_none() && pass.is_some() && refresh_front() && !refresh_sizing() && !refresh_check();
        if staged_number.is_some() {
            // (Done by the thread of stage A.)
        } else if refresh_front() {
            match pass.as_mut() {
                Some(pass) if front_pass => batch.refresh_front_pass(pass, &parts),
                _ => batch.refresh_front(runs, &parts),
            }
        } else if ahead_of_writes() {
            batch.refresh_ahead(runs, |list, i| own.as_ref().is_some_and(|own| own.takes(list, i)));
        }
        stage.next("submit: the lists' steps");
        // With the summaries of the lists' steps [`onepass`]: the steps the card's plan takes, for each list in order (a
        // cursor over them, not a look in a table for each fill and copy).
        let takes: Option<Vec<Vec<usize>>> = (front_pass && own.is_some()).then(|| own.as_ref().map(|own| own.by_list(runs)).unwrap_or_default());
        // What the lists' preludes ask of the drawing queue when the lists are of the compute type.
        let mut hand: Vec<D3D12_RESOURCE_BARRIER> = Vec::new();
        for (n, list) in runs.iter().enumerate() {
            if staged_number.is_none() {
                let cursor = if front_pass { pass.as_ref().and_then(|pass| pass.cursor(n)) } else { None };
                match &takes {
                    Some(takes) => {
                        list.run_deferred(&mut batch, &air, onepass::taken_in_order(&takes[n]), cursor);
                    }
                    None => list.run_deferred(&mut batch, &air, |i| own.as_ref().is_some_and(|own| own.takes(list, i)), cursor),
                }
            }
            firsts.push(lists.len());
            if let Some(prelude) = list.prelude(&mut hand) {
                lists.push(Some(prelude));
            }
            lists.push(list.list.cast().ok());
        }
        if let Some(route) = route.as_mut() {
            route.hand(hand);
        }
        batch.fill();
        // (The holds the pass took on the copies are let go; what is ahead stays with the batch to its end.)
        drop(pass.take());
        stage.next("submit: the last refreshes");
        batch.flush();
        match asking.take() {
            Some(standing) => crate::watch::end_standing(standing),
            None => crate::watch::ask_once_end(),
        }
        #[cfg(feature = "diagnostics")]
        split.part(2);
        stage.next("submit: snapshots");
        // The snapshots of the ranges shaders may write, if they were left
        // for now (`crate::shadow::wide_back`): the refreshes are done, the
        // lists have not gone to the card.
        {
            #[cfg(feature = "diagnostics")]
            let snapshots = std::time::Instant::now();
            if snap_aside() {
                crate::shadow::take_snapshots_aside();
            } else {
                crate::shadow::take_snapshots();
            }
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::SnapshotUs, snapshots.elapsed().as_micros() as u64);
        }
        #[cfg(feature = "diagnostics")]
        split.part(3);
        stage.next("submit: waiting for the refreshes begun before the steps");
        prefetch::join_early();
        stage.next("submit: copies in the card's memory");
        // Copies in the card's memory: what changed on their CPU side goes
        // up ahead of the lists, and a compare of what the shaders may
        // write follows them in the same submission.
        let flown = late && relay();
        if flown && self.card_results_low() {
            // (Flights landed here write host copies: the snapshots given away are done first.)
            crate::shadow::snapshots_wait();
            let settling = std::time::Instant::now();
            self.settle(&mut submitted, WHY_MOST);
            settled += settling.elapsed();
        }
        // What shaders may write through views they pick: last, so that no
        // refresh of this submission leaves it alone.
        let picked = crate::picked::declare(runs);
        if !picked.written.is_empty() {
            crate::shadow::snapshots_wait();
        }
        for (shadow, at, len) in &picked.written {
            shadow.writing(*at, *len);
        }
        let declared = declared_writes(runs);
        // What it declares written is in the line before the card is given
        // anything of it (`VICTORY_PENDING_FLIGHT=1`).
        let pending = (late && pending_flights()).then(|| {
            let back = crate::picked::with(declared.clone(), &picked.written);
            let mut ranges: Vec<(usize, usize)> = back.iter().flat_map(|(_, ranges)| ranges.iter().copied()).collect();
            for list in runs {
                ranges.extend(list.deferred().texture_backs.iter().map(crate::texback::Back::range));
            }
            // (With what its flight will say of who writes what, and the card's copies of its own steps: stage A of the
            // next submission looks at them too, and plans its reads for the card from them.)
            let (reads_back, writes) = plan::backs_and_writes(runs);
            let copies = own.as_ref().map(|own| own.copied().collect::<Vec<_>>()).unwrap_or_default();
            PendingFlight::register_with(ranges, plan::pending_writers(&back, reads_back, writes, copies))
        });
        // The copies this submission still does something to after the card's
        // plan is made: its title buffer copies' places.
        // (The switch may change while the title runs, `VICTORY_AB`: read once, so that what is offered is what was excluded.)
        let early = prefetch::on();
        let excluding: Vec<usize> = if early {
            batch.placed.iter().map(|(shadow, ..)| std::sync::Arc::as_ptr(shadow) as usize).collect()
        } else {
            Vec::new()
        };
        // Stage A of the next submission by its own thread (`VICTORY_STAGE_A_STEPS`): it begins once this submission's
        // places are in the copies (what it writes into title memory and the copies must not be in this one's sends,
        // or under its places), so only here if there are none to wait for.
        let by_thread = early && front::on();
        let mut told: Option<bool> = None;
        let mut given = false;
        let card = self.card_before(&declared, &picked, flown, &mut route, &mut |taken| {
            if told.is_none() {
                // (Told once, as it may not be offered later either.)
                told = Some(taken);
                if early && taken {
                    // (The copies it refreshes may be those of declared ranges' neighbours: the snapshots first.)
                    crate::shadow::snapshots_wait();
                    if !by_thread {
                        prefetch::offer(&excluding);
                    } else if excluding.is_empty() {
                        given = front::offer();
                    }
                }
            }
        });
        // The snapshots given away are done before the copies the lists put into host copies are written, and
        // before the lists go anywhere (what it waits is still the snapshots' time of the frame line).
        #[cfg(feature = "diagnostics")]
        let snapshots_waited = std::time::Instant::now();
        crate::shadow::snapshots_wait();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::SnapshotUs, snapshots_waited.elapsed().as_micros() as u64);
        if told.is_none() && early && !by_thread {
            prefetch::offer(&excluding);
        }
        // The sends are taken: from here on the references hold what the
        // copies in the lists put.
        for (shadow, at, bytes) in batch.placed.drain(..) {
            shadow.placed_copy(at, &bytes);
        }
        // (Not told: nothing was taken, as for the offer after the take.) Past the places, the copies are the next
        // submission's to refresh.
        if by_thread && !given && told.unwrap_or(true) && !front::offer() {
            prefetch::offer(&[]);
        }
        if let Some(check) = card.as_ref().and_then(|c| c.check.clone()) {
            lists.push(Some(check));
        }
        #[cfg(feature = "diagnostics")]
        split.part(4);
        stage.next("submit: waiting for the heaps' records");
        waits::timed(waits::Wait::Heaps, || drop(heaps_synced));
        // The slots that waited for a texture's host copy, before the
        // lists that may read them run.
        crate::descriptor::make_waiting();
        #[cfg(feature = "diagnostics")]
        split.part(15);
        let prepared = std::time::Instant::now();
        stage.next("submit: sending");
        // When it runs on the card, for the timeline.
        #[cfg(feature = "diagnostics")]
        let stamp = stamps::take();
        #[cfg(feature = "diagnostics")]
        if let Some((ahead, behind)) = stamp.and_then(stamps::lists).filter(|_| !on_compute) {
            lists.insert(0, Some(ahead));
            lists.push(Some(behind));
            firsts.iter_mut().for_each(|first| *first += 1);
        }
        #[cfg(feature = "diagnostics")]
        let sending = crate::timeline::ticks();
        if late {
            *submitted += 1;
            #[cfg(feature = "diagnostics")]
            if let Some([front, behind]) = self.span_lists().filter(|_| !on_compute) {
                lists.insert(0, Some(front));
                lists.push(Some(behind));
                firsts.iter_mut().for_each(|first| *first += 1);
            }
            let cuts: Vec<usize> = parts.iter().filter_map(|&n| firsts.get(n).copied()).collect();
            // SAFETY: closed lists; the fence is ours.
            let signalled = unsafe {
                if let Some(route) = route.as_mut() {
                    route.waits(self);
                }
                let queue = route.as_ref().map_or(&self.queue, |r| r.queue(self));
                let mut from = 0;
                for to in cuts.into_iter().chain([lists.len()]) {
                    queue.ExecuteCommandLists(&lists[from..to]);
                    from = to;
                }
                match route.as_mut() {
                    Some(route) => route.signal(self, *submitted),
                    None => self.queue.Signal(&self.fence, *submitted),
                }
            };
            awake::sent(&self.device);
            #[cfg(feature = "diagnostics")]
            crate::timeline::sent(sending, *submitted, stamp);
            if !on_compute {
                self.card_sent();
            }
            stage.next("submit: noting what is in the air");
            if let Err(e) = signalled {
                fail(format_args!("a submission sent ahead could not be followed by a signal ({e})"));
            }
            #[cfg(feature = "diagnostics")]
            split.part(5);
            let back = crate::picked::with(declared, &picked.written);
            let mut texture_back_ranges = Vec::new();
            for list in runs {
                texture_back_ranges.extend(list.deferred().texture_backs.iter().map(crate::texback::Back::range));
            }
            let writes = runs.iter().flat_map(|l| l.deferred().writes.clone()).collect();
            SUBMISSION_US[0].fetch_add((prepared - started).saturating_sub(settled).as_micros() as u64, Ordering::Relaxed);
            submitting(0, 0);
            #[cfg(feature = "diagnostics")]
            crate::copystat::submission_ends(lock);
            let sent = Sent {
                number: this_submission,
                card,
                back,
                own,
                texture_back_ranges,
                writes,
                pending,
            };
            working(was_working);
            return Some((submitted, sent));
        }
        // Diagnostics: each list on its own, timed (`VICTORY_GPU_TIMING`).
        #[cfg(feature = "diagnostics")]
        let groups: Vec<&[Option<ID3D12CommandList>]> = if timing() {
            firsts
                .iter()
                .zip(firsts.iter().skip(1).chain([&lists.len()]))
                .map(|(&a, &b)| &lists[a..b])
                .collect()
        } else {
            vec![&lists[..]]
        };
        #[cfg(not(feature = "diagnostics"))]
        let groups = [&lists[..]];
        submitting(2, runs.len());
        for (i, group) in groups.iter().enumerate() {
            *submitted += 1;
            let started = std::time::Instant::now();
            // SAFETY: closed lists; the fence and event are ours.
            unsafe {
                if let Some(route) = route.as_mut() {
                    route.waits(self);
                }
                let queue = route.as_ref().map_or(&self.queue, |r| r.queue(self));
                queue.ExecuteCommandLists(group);
                if !on_compute {
                    self.card_sent();
                }
                let signalled = match route.as_mut() {
                    Some(route) => route.signal(self, *submitted),
                    None => self.queue.Signal(&self.fence, *submitted),
                };
                let waited = signalled.and_then(|()| self.fence.SetEventOnCompletion(*submitted, self.event));
                #[cfg(feature = "diagnostics")]
                crate::timeline::sent(sending, *submitted, stamp);
                match waited {
                    Ok(()) => {
                        working(false);
                        #[cfg(feature = "diagnostics")]
                        let _card = crate::timeline::card_wait(Some(*submitted));
                        WaitForSingleObject(self.event, INFINITE);
                        stood(started);
                        working(true);
                    }
                    Err(e) => tracing::warn!(error = %e, "host submission not waited for"),
                }
            }
            // With timing on each group is one of `runs` (and the compare
            // of copies in the card's memory, with the last): every
            // command's time on the card goes to the sums by pipeline.
            #[cfg(feature = "diagnostics")]
            if timing() {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                for (ms, op) in runs[i].slowest(frequency, usize::MAX) {
                    crate::copystat::command_took(&op, ms, runs[i].queue_kind());
                }
            }
            // A submission of one list was never named here (`groups.len()
            // > 1` was the test): the two 50 ms lists of the gameplay
            // frame, each a submission of its own, printed nothing.
            #[cfg(feature = "diagnostics")]
            if timing() && started.elapsed() > std::time::Duration::from_millis(50) {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                let commands = runs[i].notes.lock().unwrap_or_else(|e| e.into_inner()).len();
                tracing::warn!(ms = started.elapsed().as_millis(), commands, "slow host list");
                for (ms, op) in runs[i].slowest(frequency, 6) {
                    tracing::warn!("slow list command, {ms:.1} ms: {op}");
                }
            }
            let _ = (i, started);
        }
        let _ = &firsts;
        FINISHED.store(this_submission, Ordering::Release);
        bury(this_submission);
        let gpu = prepared.elapsed();
        SUBMISSION_US[0].fetch_add((prepared - started).as_micros() as u64, Ordering::Relaxed);
        SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        crate::copystat::card_waited(gpu.as_micros() as u64, || {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| l.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .collect();
            let text = format!("{} lists, pipelines {pipelines:?}", runs.len());
            text.chars().take(1200).collect()
        });
        // The host resets a device whose submission runs for about 2 s.
        if gpu > std::time::Duration::from_millis(250) {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| l.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .collect();
            tracing::warn!(ms = gpu.as_millis(), lists = runs.len(), ?pipelines, "slow host submission");
        }
        // SAFETY: a plain query.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            fail(format_args!(
                "host GPU device removed after a submission of {} lists{}{}",
                runs.len(),
                self.removal_data(runs),
                last_notes(runs)
            ));
        }
        submitting(3, runs.len());
        stage.next("submit: copying back");
        // What shaders wrote goes back before the bottom-of-pipe writes,
        // which tell the title the GPU is done; each range once per
        // submission, however many dispatches declared it (the world's
        // passes declare the same big ranges dozens of times).
        let copying = std::time::Instant::now();
        // For copies in the card's memory the compare has run there: the
        // pieces that differ are fetched and merged.
        if let Some(card) = &card {
            self.card_back(card, &mut submitted, &mut std::collections::HashSet::new());
        }
        let back = crate::picked::with(declared, &picked.written);
        hand_back(&back);
        if let Some(card) = card {
            self.card_done(card);
        }
        #[cfg(feature = "diagnostics")]
        if !back.is_empty() {
            crate::copystat::add(crate::copystat::C::BackSubmissions, 1);
        }
        // Nothing of this submission goes back to the title: no range of
        // shader writes, no value written when it is done. The wait for
        // such a one is only for order and for what the card still holds.
        #[cfg(feature = "diagnostics")]
        if back.is_empty() && runs.iter().all(|l| l.deferred().writes.is_empty()) {
            use crate::copystat::{C, add};
            add(C::NothingBack, 1);
            add(C::NothingBackUs, gpu.as_micros() as u64);
        }
        COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
        for list in runs {
            // Read backs first: a value written at the bottom of the pipe
            // may be what tells the title that they are there.
            read_backs(list);
            for &(at, what, bytes) in &list.deferred().writes {
                what.write(at as *mut u8, bytes);
            }
        }
        self.log_messages();
        submitting(0, 0);
        #[cfg(feature = "diagnostics")]
        crate::copystat::submission_ends(lock);
        // With the relay: nothing is in the air now, so no fetch still to
        // be merged is older than the sends to card copies so far, and
        // their notes can go.
        if relay() {
            crate::shadow::relay_fetched(*QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
        }
        working(was_working);
        None
    }

    /// With `VICTORY_D3D12_DRED`: the faulting address and the allocations
    /// it lies in (by the names we give them) or the title memory copy just
    /// below it; for each list that did not finish, the command it stopped
    /// at and what the draws and dispatches in flight there run (or, in
    /// lists that never started, the pipelines they bind).
    pub(super) fn removal_data(&self, runs: &[&HostList]) -> String {
        let Ok(dred) = self.device.cast::<ID3D12DeviceRemovedExtendedData1>() else {
            return String::new();
        };
        let mut text = String::new();
        let mut fault_va = None;
        // SAFETY: the outputs are the runtime's, valid while the device
        // lives; nodes are walked as linked, histories have their counts.
        unsafe {
            if let Ok(fault) = dred.GetPageFaultAllocationOutput1()
                && fault.PageFaultVA != 0
            {
                let va = fault.PageFaultVA;
                fault_va = Some(va);
                text.push_str(&format!("; page fault at {va:#x}"));
                for (what, head) in [
                    ("inside", fault.pHeadExistingAllocationNode),
                    ("inside freed", fault.pHeadRecentFreedAllocationNode),
                ] {
                    let mut names = Vec::new();
                    let mut node = head;
                    while let Some(n) = node.as_ref()
                        && names.len() < 8
                    {
                        let name = if n.ObjectNameW.is_null() {
                            "unnamed".to_owned()
                        } else {
                            n.ObjectNameW.to_string().unwrap_or_default()
                        };
                        names.push(format!("{name} (type {})", n.AllocationType.0));
                        node = n.pNext;
                    }
                    if !names.is_empty() {
                        text.push_str(&format!("; {what} {names:?}"));
                    }
                }
                text.push_str(&crate::shadow::near(va));
                text.push_str(&crate::descriptor::host::near_heaps(va));
                #[cfg(feature = "diagnostics")]
                text.push_str(&crate::book::at_loss(va));
            }
            // The plain output: the extended one (with marker texts) came
            // back without stopped lists on this host.
            if dred_faults() {
                return text;
            }
            let crumbs = match dred.GetAutoBreadcrumbsOutput() {
                Ok(crumbs) => crumbs,
                Err(e) => {
                    text.push_str(&format!("; no breadcrumbs ({e})"));
                    return text;
                }
            };
            // Every list's progress (done/count), in the runtime's order.
            let mut progress = Vec::new();
            let mut node = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = node.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                let ours = p.pCommandList.as_ref().map(|l| l.as_raw());
                let index = runs.iter().position(|r| Some(r.list.as_raw()) == ours);
                progress.push(format!("{done}/{} {index:?}", p.BreadcrumbCount));
                node = p.pNext;
            }
            text.push_str(&format!("; breadcrumbs (done/count, list) {progress:?}"));
            let mut plain = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = plain.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                if done < p.BreadcrumbCount && !p.pCommandHistory.is_null() {
                    let history =
                        std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize);
                    text.push_str(&format!(
                        "; a list stopped at command {done} of {} (op {})",
                        p.BreadcrumbCount, history[done as usize].0
                    ));
                    let list = p.pCommandList.as_ref().map(|l| l.as_raw());
                    let run = runs.iter().find(|r| Some(r.list.as_raw()) == list);
                    let named = [
                        (!p.pCommandListDebugNameW.is_null()).then(|| p.pCommandListDebugNameW.to_string().unwrap_or_default()),
                        (!p.pCommandListDebugNameA.is_null()).then(|| {
                            std::ffi::CStr::from_ptr(p.pCommandListDebugNameA.cast()).to_string_lossy().into_owned()
                        }),
                    ]
                    .into_iter()
                    .flatten()
                    .next()
                    .unwrap_or_default();
                    text.push_str(&format!(" of \"{named}\""));
                    // Not one of the caller's: by the name.
                    #[cfg(feature = "diagnostics")]
                    let by_name = list_number(&named).and_then(|n| {
                        let all = LIST_NOTES.lock().unwrap_or_else(|e| e.into_inner());
                        all.get(&n).map(|(notes, pipelines, _)| (notes.upgrade(), pipelines.upgrade()))
                    });
                    #[cfg(feature = "diagnostics")]
                    let indirects = run.map(|r| r.indirects.clone()).or_else(|| {
                        let n = list_number(&named)?;
                        LIST_NOTES.lock().unwrap_or_else(|e| e.into_inner()).get(&n)?.2.upgrade()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    #[allow(clippy::type_complexity)]
                    let by_name: Option<(Option<()>, Option<std::sync::Arc<Mutex<std::collections::BTreeSet<String>>>>)> = None;
                    #[cfg(feature = "diagnostics")]
                    let noted = run.map(|r| r.notes.clone()).or_else(|| by_name.as_ref().and_then(|b| b.0.clone()));
                    let bound = run.map(|r| r.pipelines.clone()).or_else(|| by_name.as_ref().and_then(|b| b.1.clone()));
                    // The k-th marker command is the k-th note.
                    #[cfg(feature = "diagnostics")]
                    let markers: Vec<(u32, String)> = noted.map_or_else(Vec::new, |notes| {
                        let notes = notes.lock().unwrap_or_else(|e| e.into_inner());
                        history
                            .iter()
                            .enumerate()
                            .filter(|(_, op)| **op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
                            .zip(notes.iter())
                            .map(|((at, _), (note, range))| {
                                let place = fault_va.zip(*range).map(|(va, r)| fault_in(va, r));
                                (at as u32, format!("{note}{}", place.unwrap_or_default()))
                            })
                            .collect()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    let markers: Vec<(u32, String)> = Vec::new();
                    #[cfg(not(feature = "diagnostics"))]
                    let _ = fault_va;
                    if done > 0 {
                        text.push_str(&in_flight(history, &markers, done as usize));
                    }
                    #[cfg(feature = "diagnostics")]
                    if let Some(indirects) = indirects {
                        text.push_str(&indirects_around(history, done as usize, &indirects));
                    }
                    // (Without markers a list that stopped midway says what
                    // it may have been at only by what it binds.)
                    if (done == 0 || markers.is_empty())
                        && let Some(bound) = bound
                    {
                        let pipelines = bound.lock().unwrap_or_else(|e| e.into_inner());
                        text.push_str(&format!(", its pipelines {pipelines:?}"));
                    }
                }
                plain = p.pNext;
            }
        }
        text
    }
}
