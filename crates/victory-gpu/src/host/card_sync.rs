//! The card's copies: the clock, the list that brings them up to date (`card_before`) and the plan's end.

use super::*;

impl CardPlan {
    /// `crate::picked` is told where what it looked at changed, by the
    /// compare's results.
    pub(super) fn picked_found(&self, differs: &[u32]) {
        for r in self.ranges.iter().filter(|r| r.picked) {
            let changed = (0..r.span.pieces).find(|p| differs.get((r.at + p) as usize).is_some_and(|&d| torn::flagged(d)));
            if let Some(p) = changed {
                crate::picked::found(&r.shadow, r.shadow.card_piece(&r.span, p).0);
            }
        }
    }

    /// Verification mode (`VICTORY_GPU_RO_CHECK=1`): warns if a piece the
    /// compare found changed on the card lies in title memory the title
    /// asked for with read-only GPU page protection (the ranges
    /// `VICTORY_GPU_RO_SKIP` leaves out of what shaders are declared to write).
    pub(super) fn check_gpu_ro(&self, differs: &[u32]) {
        if !crate::shadow::gpu_ro_check() {
            return;
        }
        static WARNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for r in self.ranges.iter().filter(|r| !r.picked && r.compared) {
            let (base, shadow_len) = r.shadow.span();
            for p in 0..r.span.pieces {
                if !differs.get((r.at + p) as usize).is_some_and(|&d| torn::flagged(d)) {
                    continue;
                }
                let (lo, hi) = r.shadow.card_piece(&r.span, p);
                let at = base + lo as u64;
                let len = (hi - lo) as u64;
                if crate::shadow::overlaps_gpu_readonly(at, len) {
                    let n = WARNED.fetch_add(1, Ordering::Relaxed) + 1;
                    if n <= 16 || n.is_power_of_two() {
                        tracing::warn!(
                            hit = n,
                            piece = format_args!("{at:#x}..{:#x}", at + len),
                            declared = format_args!("{:#x}..{:#x}", r.declared.0, r.declared.1),
                            shadow = format_args!("{base:#x}+{shadow_len:#x}"),
                            "VICTORY_GPU_RO_CHECK: GPU compare saw changed bytes inside a GPU-read-only region"
                        );
                    }
                }
            }
        }
    }
}

impl Host {
    /// The card's clock ([`CARD_CLOCK_US`]), made at the first send.
    pub(super) fn card_clock(&self) -> Option<&'static CardClock> {
        static CLOCK: OnceLock<Option<CardClock>> = OnceLock::new();
        static WATCH: std::sync::Once = std::sync::Once::new();
        let clock = CLOCK
            .get_or_init(|| {
                // SAFETY: a plain fence on our device.
                let fence = unsafe { self.device.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE) }.ok()?;
                Some(CardClock {
                    fence,
                    state: Mutex::new((0, None)),
                    sent: std::sync::Condvar::new(),
                })
            })
            .as_ref()?;
        WATCH.call_once(|| {
            if let Err(e) = std::thread::Builder::new().name("victory-card-clock".into()).spawn(move || card_clock_watch(clock)) {
                tracing::warn!(error = %e, "no thread for the card's clock: the frame line's idle times stay 0");
            }
        });
        Some(clock)
    }

    /// Lists of ours just went to the queue: the clock's signal goes
    /// behind them, and an idle stretch of the card, if one was under way,
    /// ends here ([`CARD_CLOCK_US`]).
    pub(super) fn card_sent(&self) {
        let Some(clock) = self.card_clock() else {
            return;
        };
        let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 += 1;
        // SAFETY: our queue and the clock's fence.
        let _ = unsafe { self.queue.Signal(&clock.fence, state.0) };
        if let Some((since, worked_then)) = state.1.take() {
            CARD_CLOCK_US[0].fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
            CARD_CLOCK_US[1].fetch_add(worked().saturating_sub(worked_then), Ordering::Relaxed);
        }
        drop(state);
        clock.sent.notify_one();
    }

    /// Whether the host has what copies in the card's memory need (three
    /// lists and the compare); made at the first ask. A copy goes to the
    /// card's memory only if it has.
    pub(crate) fn card_ready(&self) -> bool {
        static READY: OnceLock<bool> = OnceLock::new();
        *READY.get_or_init(|| match self.make_card_work() {
            Ok(work) => {
                *CARD_WORK.lock().unwrap_or_else(|e| e.into_inner()) = Some(work);
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "no host copies in the card's memory: the compare could not be made");
                false
            }
        })
    }

    pub(super) fn make_card_work(&self) -> windows::core::Result<CardWork> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; the out
        // pointer is ours.
        unsafe {
            D3DCompile(
                CARD_COMPARE.as_ptr().cast(),
                CARD_COMPARE.len(),
                windows::core::s!("card compare"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )?;
        }
        let code = code.ok_or_else(windows::core::Error::empty)?;
        let list = || self.make_prelude().ok_or_else(windows::core::Error::empty);
        // A megabyte of results: 262,144 pieces, 16 GB of declared ranges
        // in one submission.
        const RESULTS: u64 = 1 << 20;
        let (differs, data) = self.shared_buffer(RESULTS).ok_or_else(windows::core::Error::empty)?;
        name(&differs, || "card compare results".into());
        // SAFETY: the blob's own bytes; the root signature is inside the
        // shader.
        unsafe {
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
            let pso: ID3D12PipelineState = self.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            })?;
            #[cfg(feature = "diagnostics")]
            name(&pso, || crate::replay::play::COMPARE.into());
            Ok(CardWork {
                sync: vec![list()?],
                check: vec![list()?],
                sync_c: Vec::new(),
                check_c: Vec::new(),
                fetch: list()?,
                root,
                pso,
                differs: (differs, data as usize, RESULTS),
                fetched: None,
                fetched_trim: crate::lean::Trim::new(),
                taken: 0,
                out: 0,
            })
        }
    }

    /// Before a submission's lists run, for copies in the card's memory
    /// ([`crate::shadow::cards`]): what changed on their CPU side since
    /// the last submission goes to their card side (a list of ours, run
    /// now; the submission's lists follow it on the queue), and the ranges
    /// this submission's shaders may write get a compare to run after its
    /// lists. `None` if there is nothing to do.
    ///
    /// `flown`: the submission is sent ahead with [`relay`]. Its compare
    /// then stores the hashes it makes (see [`flagged_pieces`] and
    /// `relay`).
    /// `picked`: ranges that may be written too, and ranges to look at
    /// (`crate::picked`).
    pub(super) fn card_before(
        &self,
        declared: &[Declared],
        picked: &crate::picked::Ranges,
        flown: bool,
        route: &mut Option<lanes::Route>,
        after_take: &mut dyn FnMut(bool),
    ) -> Option<CardPlan> {
        use std::sync::Arc;
        if !crate::shadow::cards() {
            return None;
        }
        // The ranges this submission's shaders may write, per copy.
        let declared: Vec<Declared> = declared.iter().filter(|(shadow, _)| shadow.carded()).cloned().collect();
        let declared = if crate::picked::lands() { crate::picked::with(declared, &picked.written) } else { declared };
        // (Of a copy with a declared range here, that says it.)
        let looked: Vec<&crate::picked::Range> =
            picked.looked.iter().filter(|l| !declared.iter().any(|(s, _)| Arc::ptr_eq(s, &l.0))).collect();
        // With the relay: the order of the queue is held from here until
        // what is taken is on the queue, and the taking is numbered.
        // (Read once: how this submission looks at the copies' twins.)
        let lean = lean_card();
        let mut order = relay().then(|| QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
        let sent_as = match order.as_mut() {
            Some(order) => {
                **order += 1;
                **order
            }
            None => 0,
        };
        let gathering = crate::timeline::span("copies in the card's memory: what changed");
        let mut dirty = crate::shadow::card_dirty(sent_as);
        gathering.end();
        if declared.is_empty() && dirty.is_empty() && looked.is_empty() {
            return None;
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let work = work.as_mut()?;
        let stamps = self.pass_slot();
        // A copy whose shader writes are declared for the first time gets
        // its bytes from before, whole.
        let mut whole: Vec<Arc<crate::shadow::Shadow>> = Vec::new();
        for shadow in declared.iter().map(|d| &d.0).chain(looked.iter().map(|l| &l.0)) {
            if shadow.card_make_before(|size| {
                let before = self.uav_buffer(crate::shadow::card_hash_bytes(size))?;
                name(&before, || "title buffer on the card, hashes of its pieces".into());
                Some(before)
            }) {
                whole.push(shadow.clone());
                if !dirty.iter().any(|(s, _)| Arc::ptr_eq(s, shadow)) {
                    dirty.push((shadow.clone(), Vec::new()));
                }
            }
        }
        let mut used: Vec<(u8, Prelude)> = Vec::new();
        let mut looks = None;
        // On the compute queue the lists we add are of the compute type, and what the plan touches is told to the route.
        let compute = route.as_ref().is_some_and(|r| r.compute());
        if let Some(route) = route.as_mut() {
            route.card(&dirty, &declared, &looked);
        }
        // The bytes as they are now, whatever later submissions put there.
        let taking = crate::timeline::span("copies in the card's memory: taken");
        let own = if send::own() { send::take(self, &dirty) } else { None };
        taking.end();
        // What the card is sent is out of the copies: the next submission's
        // refreshes may begin ([`prefetch`]). Not if the list below copies
        // the CPU side itself, when the card runs it (no own copies): the
        // refreshes would change what it reads.
        after_take(prefetch::may_offer(own.is_some(), dirty.is_empty()));
        if !dirty.is_empty() {
            let pooled = if compute { work.sync_c.pop() } else { work.sync.pop() };
            let Some(sync) = pooled.or_else(|| self.make_prelude_of(compute)) else {
                fail(format_args!("no host list to bring host copies in the card's memory up to date"));
            };
            let _recording = crate::timeline::span("copies in the card's memory: the list");
            let (allocator, list) = &sync;
            // SAFETY: the list's last run finished (it ran ahead of a
            // submission that was waited for, and came back after that);
            // live buffers, and the ranges lie in them.
            let recorded = unsafe {
                (|| -> windows::core::Result<()> {
                    #[cfg(feature = "diagnostics")]
                    let mut lap = std::time::Instant::now();
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    #[cfg(feature = "diagnostics")]
                    sync_lap(0, &mut lap);
                    self.pass_stamp(list, stamps, 0);
                    if upcheck::on() {
                        looks = upcheck::record(self, list, &dirty);
                    }
                    // Every copy first, then one barrier call, then every
                    // pass: a wait between each copy and its pass had the
                    // card do them one after the other.
                    let mut sent = Vec::new();
                    let mut transitioned = Vec::new();
                    if let Some(own) = &own {
                        own.record(self, list, &dirty, stamps, |shadow| looks.as_ref().is_some_and(|l| l.hold(shadow)));
                    }
                    for (shadow, ranges) in &dirty {
                        let Some((staging, before, _)) = shadow.card_sides() else {
                            continue;
                        };
                        if own.is_none() {
                            for &(a, b) in ranges {
                                list.CopyBufferRegion(&shadow.buffer, a as u64, staging, a as u64, (b - a) as u64);
                            }
                        }
                        let bytes: u64 = ranges.iter().map(|&(a, b)| (b - a) as u64).sum();
                        crate::shadow::CARD_STATS[0].fetch_add(bytes, Ordering::Relaxed);
                        crate::shadow::CARD_STATS[1].fetch_add(ranges.len() as u64, Ordering::Relaxed);
                        #[cfg(feature = "diagnostics")]
                        traffic::moved(traffic::UP, shadow.span(), ranges.iter().copied());
                        if own.is_none() && before.is_some() && !ranges.is_empty() {
                            let raw = shadow.buffer.as_raw();
                            if !transitioned.contains(&raw) {
                                transitioned.push(raw);
                                sent.push(transition(
                                    &shadow.buffer,
                                    D3D12_RESOURCE_STATE_COPY_DEST,
                                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                                ));
                            }
                        }
                    }
                    if !sent.is_empty() {
                        list.ResourceBarrier(&sent);
                    }
                    self.pass_stamp(list, stamps, 1);
                    list.SetComputeRootSignature(&work.root);
                    list.SetPipelineState(&work.pso);
                    list.SetComputeRootUnorderedAccessView(3, work.differs.0.GetGPUVirtualAddress());
                    for (shadow, ranges) in &dirty {
                        let (Some((_, Some(before), _)), size) = (shadow.card_sides(), shadow.card_hash_size()) else {
                            continue;
                        };
                        // No twin (it cost the card the copy's size again,
                        // 399 MB in run 75): `before` holds a 128-bit hash
                        // of each 64 KiB piece. The pieces just sent up are
                        // hashed again, all of them the first time; the
                        // compare after the lists hashes the declared
                        // pieces and flags those whose hash is another.
                        list.SetComputeRootShaderResourceView(1, shadow.buffer.GetGPUVirtualAddress());
                        list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                        let joins = rehash_runs() || own.is_some();
                        let runs: Vec<crate::shadow::CardSpan> = if whole.iter().any(|s| Arc::ptr_eq(s, shadow)) {
                            vec![shadow.card_run(0, usize::MAX)]
                        } else {
                            // (In order: the next of the same piece, of the
                            // tens of thousands there are, says nothing.)
                            let mut runs: Vec<crate::shadow::CardSpan> = Vec::new();
                            for run in ranges.iter().map(|&(a, b)| shadow.card_run(a, b)) {
                                match runs.last_mut() {
                                    Some(last) if joins && (last.first, last.pieces) == (run.first, run.pieces) => last.to = last.to.max(run.to),
                                    _ => runs.push(run),
                                }
                            }
                            runs
                        };
                        // The same pieces in fewer passes (`rehash_runs`).
                        let runs = if joins { crate::shadow::joined_piece_runs(runs) } else { runs };
                        CARD_PASSES[0].fetch_add(runs.len() as u64, Ordering::Relaxed);
                        CARD_PASSES[1].fetch_add(runs.iter().map(|r| u64::from(r.pieces)).sum::<u64>(), Ordering::Relaxed);
                        for run in runs {
                            card_pass(list, &run, size as u32, 0, 0);
                        }
                    }
                    self.pass_stamp(list, stamps, 2);
                    #[cfg(feature = "diagnostics")]
                    sync_lap(1, &mut lap);
                    list.Close()?;
                    #[cfg(feature = "diagnostics")]
                    sync_lap(2, &mut lap);
                    // Behind the copy of the sends over the bus, if it is
                    // on the copy queue (`VICTORY_SEND_COPYQ`).
                    // (Behind what the other queue must have done first.)
                    if let Some(route) = route.as_mut() {
                        route.waits(self);
                    }
                    let queue = route.as_ref().map_or(&self.queue, |r| r.queue(self));
                    if let Some(own) = &own {
                        own.wait(queue)?;
                    }
                    queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                    #[cfg(feature = "diagnostics")]
                    {
                        sync_lap(3, &mut lap);
                        SYNC_LIST[4].fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(())
                })()
            };
            if let Err(e) = recorded {
                fail(format_args!("host copies in the card's memory could not be brought up to date ({e})"));
            }
            used.push((if compute { 2 } else { 0 }, sync));
        }
        drop(order);
        let mut plan = CardPlan {
            ranges: Vec::new(),
            check: None,
            _synced: dirty.into_iter().map(|(shadow, _)| shadow).collect(),
            lists: used,
            sent_as,
            looks,
            sends: own.map(send::Own::memory),
            stamps,
        };
        // Each byte once, as the copy-back joins the declared ranges. The
        // results go behind those of plans still in the air.
        let mut at = work.taken;
        for (shadow, ranges) in &declared {
            let mut sorted = ranges.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            let twin = if lean { shadow.has_before() } else { shadow.card_sides().is_some_and(|(_, before, _)| before.is_some()) };
            for (from, to) in spans {
                let Some(span) = shadow.card_span(from, to) else {
                    continue;
                };
                let compared = twin && u64::from(at + span.pieces) * 4 <= work.differs.2;
                plan.ranges.push(CardRange {
                    shadow: shadow.clone(),
                    declared: (from, to),
                    span,
                    at,
                    compared,
                    picked: false,
                });
                if compared {
                    at += span.pieces;
                }
            }
        }
        for (shadow, from, len) in looked {
            let twin = if lean { shadow.has_before() } else { shadow.card_sides().is_some_and(|(_, before, _)| before.is_some()) };
            if let Some(span) = shadow.card_span(*from, from + len)
                && twin
                && u64::from(at + span.pieces) * 4 <= work.differs.2
            {
                plan.ranges.push(CardRange {
                    shadow: shadow.clone(),
                    declared: (*from, from + len),
                    span,
                    at,
                    compared: true,
                    picked: true,
                });
                at += span.pieces;
            }
        }
        work.taken = at;
        crate::echo::pieces(plan.ranges.iter().filter(|r| r.compared).map(|r| (r.shadow.id(), r.span.first, r.span.pieces)));
        if plan.ranges.iter().any(|r| r.compared) {
            let pooled = if compute { work.check_c.pop() } else { work.check.pop() };
            let Some(check) = pooled.or_else(|| self.make_prelude_of(compute)) else {
                fail(format_args!("no host list for the compare of host copies in the card's memory"));
            };
            let (allocator, list) = &check;
            // SAFETY: the list's last run finished (its submission was
            // waited for, and it came back after that); live buffers; the
            // compare's own root signature.
            let recorded = unsafe {
                (|| -> windows::core::Result<ID3D12CommandList> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    // What the submission's shaders wrote, first.
                    list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                        Anonymous: D3D12_RESOURCE_BARRIER_0 {
                            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                                pResource: std::mem::ManuallyDrop::new(None),
                            }),
                        },
                    }]);
                    self.pass_stamp(list, stamps, 3);
                    list.SetComputeRootSignature(&work.root);
                    list.SetPipelineState(&work.pso);
                    list.SetComputeRootUnorderedAccessView(3, work.differs.0.GetGPUVirtualAddress());
                    for r in plan.ranges.iter().filter(|r| r.compared) {
                        let (Some((_, Some(before), _)), size) = (r.shadow.card_sides(), r.shadow.card_hash_size()) else {
                            continue;
                        };
                        list.SetComputeRootShaderResourceView(1, r.shadow.buffer.GetGPUVirtualAddress());
                        list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                        // (What is only looked at keeps its hashes: what
                        // changed there is the next declared range's to land.)
                        card_pass(list, &r.span, size as u32, r.at, if flown && !r.picked { 2 } else { 1 });
                        CARD_PASSES[2].fetch_add(1, Ordering::Relaxed);
                    }
                    self.pass_stamp(list, stamps, 4);
                    list.Close()?;
                    list.cast()
                })()
            };
            match recorded {
                Ok(list) => plan.check = Some(list),
                Err(e) => fail(format_args!("the compare of host copies in the card's memory could not be recorded ({e})")),
            }
            plan.lists.push((if compute { 3 } else { 1 }, check));
        }
        work.out += 1;
        Some(plan)
    }

    /// With [`relay`]: whether the compare's result area is half taken by
    /// plans in the air. It starts from the beginning only when no plan is
    /// out, so the caller lets the flights land first.
    pub(super) fn card_results_low(&self) -> bool {
        CARD_WORK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|work| u64::from(work.taken) * 8 > work.differs.2)
    }

    /// The plan's submission has run and its results were read
    /// ([`Host::card_back`]): its lists are free for the next plan. Called
    /// when nothing of ours is on the card, so the compare's results start
    /// from the beginning again.
    pub(super) fn card_done(&self, plan: CardPlan) {
        if let Some(looks) = plan.looks {
            looks.read();
        }
        if let Some(memory) = plan.sends {
            send::give_back(memory);
        }
        self.pass_read(plan.stamps);
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(work) = work.as_mut() else {
            return;
        };
        for (kind, list) in plan.lists {
            match kind {
                0 => work.sync.push(list),
                1 => work.check.push(list),
                2 => work.sync_c.push(list),
                _ => work.check_c.push(list),
            }
        }
        work.out = work.out.saturating_sub(1);
        // With the relay other plans may be on the card, their results
        // not read yet: the area starts again only when none is out.
        if !relay() || work.out == 0 {
            work.taken = 0;
        }
    }
}
