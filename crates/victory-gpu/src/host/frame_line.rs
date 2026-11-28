//! The lines logged per frame next to the frame-rate line (the card's clock, the blame, the line of flights, the split of the preparing, the lines made aside). `Meter` stays in host.rs: its frame-rate line is parsed by target (`victory_gpu::host`).

use super::*;

/// For the frame-rate line: [`CARD_CLOCK_US`] per frame, `ms` being the
/// frame's time.
pub(super) fn card_clock_line(frames: f64, ms: f64) {
    let [idle, behind, stood] = [0, 1, 2].map(|i| CARD_CLOCK_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
    tracing::info!(
        "the card's clock per frame: nothing of ours queued {idle:.1} ms (the card about {:.0}% busy by this count), \
         {behind:.1} ms of that while the submitting thread was working (the card idle behind us), the rest while the title \
         had not submitted; the submitting thread stood waiting for the card {stood:.1} ms",
        if ms > 0.0 { (100.0 * (1.0 - idle / ms)).clamp(0.0, 100.0) } else { 0.0 }
    );
}

/// For the frame-rate line: the five that waited longest.
#[cfg(feature = "diagnostics")]
pub(super) fn log_blame(frames: f64) {
    let all = BLAME.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    let wide = WIDE_WRITES.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    let mut all: Vec<_> = all.into_iter().collect();
    all.sort_unstable_by_key(|(_, (_, us, _))| std::cmp::Reverse(*us));
    for ((kind, why, len, from, size), (n, us, at)) in all.into_iter().take(5) {
        // The passes that declare it written, the ones that do so most.
        let mut passes: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
        for ((a, b, _), (n, shaders)) in &wide {
            if *a < at + len && at < *b {
                *passes.entry(shaders).or_default() += n;
            }
        }
        let mut passes: Vec<(&str, u64)> = passes.into_iter().collect();
        passes.sort_unstable_by_key(|p| std::cmp::Reverse(p.1));
        let passes: Vec<String> =
            passes.into_iter().take(5).map(|(shaders, n)| format!("{shaders} {:.1}x", n as f64 / frames)).collect();
        tracing::info!(
            "waited for flights, per frame: {:.1} ms in {:.2} waits for {kind} +{len:#x} (last at {at:#x}), in shader writes declared \
             over {from:#x}+{size:#x} (declared per frame over a megabyte or more there, by shader: {}){}{why}",
            us as f64 / 1e3 / frames,
            n as f64 / frames,
            passes.join(", "),
            match (why.is_empty(), kind) {
                (true, _) => "",
                (_, "title memory changed under shader writes") => "; not put in under them: ",
                _ => "; not done on the card: ",
            },
        );
    }
}

/// For the frame-rate line, with [`late`]: what the line of flights did
/// per frame since the last one.
pub(super) fn late_line(frames: f64) {
    if !late() {
        return;
    }
    prefetch::line(frames);
    front::frame_line(frames);
    askaside::line(frames);
    waits::line(frames);
    onepass::line(frames);
    let s: Vec<f64> = LATE_STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "submissions sent ahead per frame: {:.1} in {:.1} settles ({:.1} flights each time), {:.1} ms waiting for the card in them; \
         settled for: a synchronous submission or present {:.1}, the card had run them {:.1}, a wait for a memory value {:.1}, \
         title memory changed under shader writes {:.1}, title memory read {:.1} (by a buffer copy {:.1}, a fill of upload memory {:.1}, \
         a texture {:.1}), a step of ours {:.1}, {} lists in the air {:.1}, \
         bottom-of-pipe writes (VICTORY_LATE_WAIT_WRITES=0) {:.1}, a blocking fence wait {:.1}; \
         {:.1} waits for memory values met by a flight's bottom-of-pipe write, {:.1} fence signals set behind flights",
        s[0],
        s[1],
        if s[1] > 0.0 { s[11] / s[1] } else { 0.0 },
        s[12] / 1000.0,
        s[WHY_SYNC],
        s[WHY_CARD_DONE],
        s[WHY_VALUE],
        s[WHY_STALE],
        s[WHY_READ],
        s[15],
        s[16],
        s[17],
        s[WHY_RUN],
        late_most(),
        s[WHY_MOST],
        s[WHY_WRITES],
        s[WHY_FENCE],
        s[13],
        s[14],
    );
    {
        let w: Vec<f64> = LATE_WAITED.iter().map(|w| w.swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames).collect();
        tracing::info!(
            "of the waiting for the card in settles, per frame, by what settled: a synchronous submission or present {:.1} ms, \
             the card had run them {:.1}, a wait for a memory value {:.1}, title memory changed under shader writes {:.1}, \
             title memory read {:.1}, a step of ours {:.1}, lists in the air {:.1}, bottom-of-pipe writes {:.1}, a blocking fence wait {:.1}",
            w[WHY_SYNC],
            w[WHY_CARD_DONE],
            w[WHY_VALUE],
            w[WHY_STALE],
            w[WHY_READ],
            w[WHY_RUN],
            w[WHY_MOST],
            w[WHY_WRITES],
            w[WHY_FENCE],
        );
    }
    if relay() {
        let r: Vec<f64> = RELAY_STATS.iter().map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        tracing::info!(
            "the relay per frame (VICTORY_RELAY=1): {:.1} flights landed in {:.1} landings by the thread that copies back, \
             beside the submitting thread; that thread waited {:.1} ms for the card with nothing to land and {:.1} ms for \
             fetches of card copies' pieces, and was {:.1} ms landing in all (the frame line's \"copying back\" is its time; \
             \"settles\" above are waits of other threads for it); {:.1} of the flights had nothing for the title to read (no copy on the card,              declared write, read-back)",
            r[1],
            r[0],
            r[2] / 1000.0,
            r[3] / 1000.0,
            r[4] / 1000.0,
            r[5],
        );
        if send::own() {
            let t: Vec<f64> = send::STATS.iter().map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames).collect();
            tracing::info!(
                "bytes of submissions' own sent up per frame (VICTORY_SEND_OWN): {:.1} MB in {:.0} runs, {:.0} words in scatters; {:.2} ms \
                 taking them out of the CPU sides",
                t[0] / (1 << 20) as f64,
                t[1],
                t[2],
                t[3] / 1000.0,
            );
            if stage::on() {
                let q: Vec<f64> = stage::STATS.iter().map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames).collect();
                tracing::info!(
                    "sends crossing the bus on a copy queue per frame (VICTORY_SEND_COPYQ): {:.1} submissions, {:.1} MB; left on the drawing                      queue: {:.1} too small, {:.1} for want of room, {:.1} failed",
                    q[0],
                    q[1] / (1 << 20) as f64,
                    q[2],
                    q[3],
                    q[4],
                );
            }
        }
        let t: Vec<f64> = torn::STATS.iter().map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        if t[0] > 0.0 {
            tracing::info!(
                "fetched pieces checked per frame (VICTORY_FETCH_CHECK): {:.1}, of which {:.2} were not what their flight ended                  with (caught while a later flight was at them, or title bytes went up meanwhile); {:.2} were found whole when                  fetched again",
                t[0],
                t[1],
                t[2],
            );
        }
    }
    crate::land::log(frames);
    if place::on() {
        let t: Vec<f64> = place::STATS.iter().map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        tracing::info!(
            "title buffer copies at their place in the lists, per frame (VICTORY_COPIES_IN_PLACE): {:.1} of {:.1} KB, {:.1} of them out              of what shaders wrote; {:.2} copies into textures out of what shaders wrote",
            t[0],
            t[1] / 1024.0,
            t[2],
            t[3],
        );
    }
    card_steps::log(frames);
    {
        let r: Vec<f64> = REFRESH_SIZE.iter().map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        let [ranges, bytes, pages, with, detect, lock, merge, put] = crate::shadow::under_stats().map(|n| n as f64 / frames);
        let mb = |v: f64| v / (1 << 20) as f64;
        if r[0] + r[3] + ranges > 0.0 {
            tracing::info!(
                "the refreshes' work per frame: {:.0} spans in front of the lists' steps ({:.0} MB of range; {:.0} left after those known to copy nothing), {:.0} before shader writes ({:.0} MB of range); what the steps refer to, each byte once (VICTORY_REFRESH_SIZING=1): {:.0} MB in {:.0} refreshes; narrowed (VICTORY_REFRESH_NARROW={}), checked (VICTORY_REFRESH_CHECK=1): {:.1} MB looked at, {:.3} MB stale, {:.1} MB not to be looked at without asking; under flights' writes: {ranges:.0} declared ranges ({:.0} MB), {pages:.0} stale pages of 64 KiB in {with:.0} of them; the submitting thread's time there: {:.2} ms finding what is stale, {:.2} getting the copy's locks, {:.2} comparing and putting the pages in, {:.2} noting what goes up",
                r[0],
                mb(r[1]),
                r[2],
                r[3],
                mb(r[4]),
                mb(r[5]),
                r[6],
                if narrow_on() { refresh_narrow_gap() >> 10 } else { 0 },
                mb(r[7]),
                mb(r[8]),
                mb(r[9]),
                mb(bytes),
                detect / 1e6,
                lock / 1e6,
                merge / 1e6,
                put / 1e6,
            );
        }
    }
    crate::picked::log(frames);
    #[cfg(feature = "diagnostics")]
    log_blame(frames);
    if late_exact() {
        let [pages, runs, bytes] = crate::shadow::merged_stats().map(|n| n as f64 / frames);
        tracing::info!(
            "title bytes put in under flights' shader writes per frame (VICTORY_LATE_WAIT=2): {pages:.0} pages looked at, \
             {runs:.0} changed runs, {:.2} MB",
            bytes / (1 << 20) as f64
        );
    }
}

/// The split of the preparing, per frame, for the frame-rate line.
#[cfg(feature = "diagnostics")]
pub(super) fn split_line(frames: f64) {
    let ms: Vec<f64> = SPLIT.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64 / 1e6 / frames).collect();
    let f: Vec<f64> = FLUSHES.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    let e: Vec<f64> = crate::watch::early_asks().iter().map(|&n| n as f64 / frames).collect();
    let a: Vec<f64> = crate::watch::asks().iter().map(|&n| n as f64 / frames).collect();
    let h: Vec<f64> = crate::draw::HEAP_SPLIT.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64 / 1e6 / frames).collect();
    let s: Vec<f64> = crate::shadow::send_stats().iter().map(|&n| n as f64 / frames).collect();
    tracing::info!(
        "preparing per frame, the submitting thread: {:.1} ms asking the write watch ({:.1} of them finding what to ask), {:.1} putting the title's bytes under flights' \
         writes, {:.1} in the lists' steps (refreshes {:.1} in {:.0} flushes of {:.0}, {:.0} of them known to copy nothing, {:.0} on \
         several threads, {:.1} one at a time in {:.0} before shader writes; copies and fills {:.1}, the refreshes before copies among them), {:.1} snapshots, {:.1} for copies in the \
         card's memory, {:.1} sending the lists (VICTORY_LIGHT_REFRESH is {}); ahead of these {:.1} waiting for the submission lock, {:.1} \
         planning steps done on the card, {:.1} waiting for flights, {:.1} the heaps' records and what is swept; at the end {:.1} waiting \
         for the heaps' records; asked early by a thread of its own {:.0} pieces of 2 MiB, {:.1} ms (VICTORY_ASK_EARLY)",
        ms[0],
        ms[8],
        ms[1],
        ms[2],
        ms[6],
        f[0],
        f[1],
        f[2],
        f[3],
        ms[9],
        ms[10] * 1e6,
        ms[7],
        ms[3],
        ms[4],
        ms[5],
        if crate::watch::light() { "on" } else { "off" },
        ms[11],
        ms[12],
        ms[13],
        ms[14],
        ms[15],
        e[0],
        e[1] / 1e3,
    );
    tracing::info!(
        "asking about title memory per frame, all threads: {:.0} kernel askings over {:.0} MB ({:.1} ms, {:.0} pages of 4 KiB found \
         written, {:.0} askings refused); {:.0} questions about holes; view ranges: {:.0} answered by their \
         bits, {:.0} by the guards whole, {:.0} block by block; {:.0} blocks made read-only, {:.0} opened by ourselves; {:.1} ms \
         waiting for the guards, {:.1} ms changing protections; pages taken for the card: {:.1} MB compared, {:.1} MB of them \
         as they were, {:.1} MB to go up in {:.0} runs (VICTORY_SEND_CHANGED)",
        a[0],
        a[1] / 1024.0,
        a[11] / 1e6,
        a[12],
        a[10],
        a[9],
        a[2],
        a[3],
        a[4],
        a[5],
        a[6],
        a[7] / 1e6,
        a[8] / 1e6,
        s[0] / 1048576.0,
        s[1] / 1048576.0,
        s[2] / 1048576.0,
        s[3],
    );
    tracing::info!(
        "the pass over the heap's views per frame: {:.1} ms looking at slots, {:.1} their copies' spans and refreshes, {:.1} textures; the \
         first two beside the recording (VICTORY_HEAP_PASS_ASIDE), {:.1} ms waited for at the lists' close",
        h[0],
        h[1],
        h[2],
        aside::WAITED.swap(0, Ordering::Relaxed) as f64 / 1e3 / frames,
    );
}

/// Diagnostics: the lines of the traffic and of the copies' statistics, with
/// the memory line (the process's memory, the card's, the census of resources,
/// queries of the address space: 16 ms of `VirtualQuery` and some 70 ms in
/// all in one profile) are made by a thread of ours, not by the thread that
/// presents (`VICTORY_LINES_ASIDE=0`: there, as before). On the thread that
/// presents, once a second, they stood in the submit thread's critical path
/// and in every timeline and every ms/frame of a diagnostics run. A build
/// without diagnostics does not make these lines.
#[cfg(feature = "diagnostics")]
pub(super) fn lines_aside(frames: f64) {
    static WORKER: OnceLock<Option<std::sync::mpsc::Sender<f64>>> = OnceLock::new();
    let make = |frames: f64| {
        traffic::line(frames);
        crate::copystat::log(frames);
    };
    let worker = WORKER.get_or_init(|| {
        if std::env::var_os("VICTORY_LINES_ASIDE").is_some_and(|v| v == "0") {
            return None;
        }
        let (tx, rx) = std::sync::mpsc::channel::<f64>();
        std::thread::Builder::new()
            .name("victory-lines".into())
            .spawn(move || {
                while let Ok(frames) = rx.recv() {
                    make(frames);
                }
            })
            .ok()
            .map(|_| tx)
    });
    match worker {
        Some(tx) if tx.send(frames).is_ok() => {}
        _ => make(frames),
    }
}
