//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and
//! the title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! The default budget is half a second per title frame. A pass the title
//! runs only once and whose pipeline takes longer is lost.
//!
//! A wait is only made where it can end in the pipeline: behind hundreds of
//! others, or with every worker busy for seconds, it cannot, and the frame
//! used to spend the whole budget, frame after frame (seven minutes of 500 ms
//! frames in the prologue), to drop the draw anyway. Now a thread waits for a
//! pipeline only if the time workers have been taking per pipeline says it
//! will be made within what is left of the budget ([`until_made`]); and the
//! pipelines the frame being recorded asks for are made before the ones
//! asked for in frames before it (`Queue::recent`).
//! `VICTORY_PIPELINE_PRIORITY=0`: as before. A dispatch is not dropped that
//! way at all ([`compute_budget`]): what it computes may be done once and kept
//! by the title, and it waits for its pipeline. Passes over the
//! screen (no vertex input, no depth) have workers and a longer budget of
//! their own ([`screen_budget`]): the title fills its lighting tables with
//! such draws once, at the world's first frame, and behind a world's worth of
//! pipelines they were dropped; that run had no lamps and no diffuse light to
//! its end (run textures-d2, the first after a new translation; Belobug
//! 18e57db8 found the same).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    /// Taken by a worker from [`Queue::slow`].
    was_slow: AtomicBool,
    made: OnceLock<Option<HostPipeline>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw or dispatch has requested it yet.
    key: String,
    used: AtomicBool,
    /// A pass over the screen ([`GraphicsState::screen_pass`]) or a confirmed
    /// opening-scene / bake producer that must never sit behind cold RT shaders.
    screen: bool,
    /// Confirmed first-scene / bake producer prepared early even on a cold cache.
    scene_input: bool,
    /// A compute program: what it computes may be done once and kept (a
    /// head's blend shape mappings, mip levels, colour tables), so a dispatch
    /// of it is not dropped for want of a host pipeline ([`compute_budget`]).
    compute: bool,
    /// Why this pipeline is intentionally skipped (e.g. RT resolve without BVH).
    left_out: Option<&'static str>,
    /// The frame (plus one) of the last request that put it in `Queue::recent`,
    /// and its place there.
    touched: AtomicU64,
    seq: AtomicU64,
    /// When a worker began making it, in microseconds since [`epoch`] plus one.
    making_at: AtomicU64,
}

fn named(shaders: &[Arc<Shader>], name: &[u8]) -> bool {
    shaders
        .iter()
        .any(|s| s.stage == "compute" && s.container.windows(name.len()).any(|w| w == name))
}

/// Which of the title's ray-tracing passes are left out, as `(the shading of
/// hits, the activation of probe rays)`, by what the switches say.
///
/// Without the structure builder every ray misses, and the ~1,000 programs
/// that shade hits cost compile time for nothing, so they are left out unless
/// `VICTORY_RT_RESOLVE` is set to something but 0; the activation of probe
/// rays is one pipeline, left out unless `VICTORY_ACTIVATE_RAYS` is set to
/// something but 0 (what it marks is then kept stale in blocks of the picture's
/// tiles). With the builder (`VICTORY_BVH_BUILDER=1`) both run, and only
/// `VICTORY_ACTIVATE_RAYS=0` takes the activation out.
fn passes_left_out(
    builder: bool,
    rt_resolve: Option<&str>,
    activate_rays: Option<&str>,
) -> (bool, bool) {
    let set = |v: Option<&str>| v.is_some_and(|v| v != "0");
    let hits = !builder && !set(rt_resolve);
    let activation = if builder {
        activate_rays == Some("0")
    } else {
        !set(activate_rays)
    };
    (hits, activation)
}

pub(super) fn left_out(shaders: &[Arc<Shader>]) -> Option<&'static str> {
    static LEFT_OUT: OnceLock<(bool, bool)> = OnceLock::new();
    let (hits, activation) = *LEFT_OUT.get_or_init(|| {
        let var = |name| std::env::var(name).ok();
        passes_left_out(
            std::env::var_os("VICTORY_BVH_BUILDER").is_some_and(|v| v == "1"),
            var("VICTORY_RT_RESOLVE").as_deref(),
            var("VICTORY_ACTIVATE_RAYS").as_deref(),
        )
    });
    if hits && named(shaders, b"CS_RaytracingResolve") {
        return Some("shades ray-tracing hits without BVH, left out");
    }
    if activation && named(shaders, b"CS_ScreenProbes_ActivateRays") {
        return Some("ray activation shader without BVH, left out");
    }
    None
}

/// Confirmed first-scene producers. Prepare these at creation even on a cold
/// launch so cutscene lighting, tiling, and bakes never miss their first frame.
fn scene_input(shaders: &[Arc<Shader>]) -> bool {
    shaders.iter().any(|s| {
        matches!(
            s.hash.as_str(),
            "970c4fbb3b017c0e"
                | "068c90badc86f1fd"
                | "e4a7e088c7780ce8"
                | "ebe4e53f031f5249"
                | "2cc4c197ee29e291"
                | "369cbe906ea72764"
        ) || crate::draw::BAKE_PIXEL_SHADERS.contains(&s.hash.as_str())
    })
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

/// What identifies a title pipeline across runs: its shaders and state.
pub(super) fn key_of(shaders: &[Arc<Shader>], graphics: Option<&GraphicsState>) -> String {
    let mut key = blake3::Hasher::new();
    for s in shaders {
        key.update(s.stage.as_bytes());
        key.update(s.hash.as_bytes());
    }
    key.update(graphics.map(|g| g.describe()).unwrap_or_default().as_bytes());
    key.finalize().to_hex()[..32].to_owned()
}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let key = key_of(&shaders, graphics.as_deref());
        let scene_input = scene_input(&shaders);
        let screen = (graphics.as_ref().is_some_and(|g| g.screen_pass())
            && shaders.iter().all(|s| s.stage != "mesh"))
            || scene_input;
        let left_out = left_out(&shaders);
        Pending {
            compute: shaders.iter().any(|s| s.stage == "compute"),
            screen,
            scene_input,
            left_out,
            shaders,
            graphics,
            queue_state: AtomicU8::new(NOT_QUEUED),
            was_slow: AtomicBool::new(false),
            made: OnceLock::new(),
            key,
            used: AtomicBool::new(false),
            touched: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            making_at: AtomicU64::new(0),
        }
    }

    pub(super) fn key(&self) -> &str {
        &self.key
    }

    fn make(&self) {
        self.made.get_or_init(|| {
            let began = Instant::now();
            self.making_at.store(began.duration_since(epoch()).as_micros() as u64 + 1, Ordering::Relaxed);
            let made = HostPipeline::new(&self.shaders, self.graphics.as_deref());
            super::library::note_time(&self.key, began.elapsed().as_millis() as u64);
            note_made(began.elapsed());
            made
        });
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some(p.left_out.map_or_else(|| "being made".into(), |why| format!("not made: {why}"))),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// The hashes of the shaders `p` is made of.
pub(crate) fn hashes(p: &Pending) -> String {
    let hashes: Vec<&str> = p.shaders.iter().map(|s| s.hash.as_str()).collect();
    hashes.join("+")
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads.
fn wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(500);
        tracing::info!(millis, "host pipeline wait budget per frame");
        Duration::from_millis(millis)
    })
}

/// As [`wait_budget`], for passes over the screen (`VICTORY_SCREEN_WAIT_MS`,
/// 10 s; 0: like the others). Below the title's 30 s watchdog.
fn screen_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_SCREEN_WAIT_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(10_000);
        Duration::from_millis(millis)
    })
}

/// Whether passes over the screen go before the others.
fn screens_first() -> bool {
    !screen_budget().is_zero()
}

/// What a thread recording a dispatch waits for its pipeline at most, whatever
/// the frame has spent: a dispatch dropped is a result lost, and the title
/// does not always redo it (a black head after a cold start: the one dispatch
/// that maps the blend shapes of the head was dropped behind the budget and
/// never came back). The frame
/// that waits asks for nothing more, and what it asked for is made first
/// (`Queue::recent`), so the wait is the pipelines' own making, not the queue
/// of the world's. `VICTORY_COMPUTE_WAIT_MS` (120 s; 0: as the draws, dropped past
/// the frame's budget). The title's watchdog is held meanwhile.
fn compute_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_COMPUTE_WAIT_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(120_000);
        Duration::from_millis(millis)
    })
}

/// Whether `p` is a pipeline whose dispatches wait for it ([`compute_budget`]).
fn waited_for(p: &Pending) -> bool {
    p.compute && !compute_budget().is_zero()
}


/// Whether a wait is skipped that cannot end in the pipeline, and the frame's
/// own requests go first (`VICTORY_PIPELINE_PRIORITY=0`: as before).
///
/// (May change while the title runs, `VICTORY_AB=..,VICTORY_PIPELINE_PRIORITY=0`: a wait decides for itself whether to
/// give up, and a touch moves a queued pipeline up only if it is on: a pipeline moved before the switch went off keeps
/// its place, one asked for after it is made in the order it came in.)
fn prioritised() -> bool {
    crate::ab::live_value!("VICTORY_PIPELINE_PRIORITY", bool, crate::ab::unless_zero)
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// What making a pipeline has taken lately, in microseconds (a moving average
/// of the last eight or so; 0 before the first), the workers that are not
/// screen-only, and how many of them are making one now.
static AVERAGE_US: AtomicU64 = AtomicU64::new(0);
static WORKERS: AtomicU64 = AtomicU64::new(0);
static BUSY: AtomicU64 = AtomicU64::new(0);

fn note_made(took: Duration) {
    let old = AVERAGE_US.load(Ordering::Relaxed);
    AVERAGE_US.store(averaged(old, took.as_micros() as u64), Ordering::Relaxed);
}

fn averaged(old: u64, us: u64) -> u64 {
    if old == 0 { us.max(1) } else { (old - old / 8 + us / 8).max(1) }
}

/// How long until a pipeline is made, for one that took `average` to make
/// on average: `ahead` pipelines are to be taken by workers before it, `idle`
/// of the `workers` have nothing to do; or, one a worker has been at for
/// `making`, what is left of the average (nothing past it: it may be close).
/// Zero when nothing is known.
fn until_made(average: Duration, making: Option<Duration>, ahead: u64, idle: u64, workers: u64) -> Duration {
    if average.is_zero() {
        return Duration::ZERO;
    }
    if let Some(elapsed) = making {
        return average.saturating_sub(elapsed);
    }
    if ahead < idle {
        return average;
    }
    // A worker takes the next one each time one finishes, `workers` of them
    // in an average.
    average + average.mul_f64((ahead - idle + 1) as f64 / workers.max(1) as f64)
}

/// [`until_made`] for `p` as the queue is now.
fn expected(p: &Pending) -> Duration {
    let average = Duration::from_micros(AVERAGE_US.load(Ordering::Relaxed));
    let workers = WORKERS.load(Ordering::Relaxed);
    let busy = BUSY.load(Ordering::Relaxed);
    let making = match p.queue_state.load(Ordering::Relaxed) {
        MAKING => {
            let at = p.making_at.load(Ordering::Relaxed);
            let now = Instant::now().duration_since(epoch()).as_micros() as u64 + 1;
            Some(Duration::from_micros(if at == 0 { 0 } else { now.saturating_sub(at) }))
        }
        _ => None,
    };
    let ahead = p.seq.load(Ordering::Relaxed).saturating_sub(RECENT_FRONT.load(Ordering::Relaxed));
    until_made(average, making, ahead, workers.saturating_sub(busy), workers)
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    if p.left_out.is_some() {
        return None;
    }
    // A still-pending pipeline was requested by the title too. Prewarm it
    // on the next launch, even if this run never got to use it.
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    if p.made.get().is_none() {
        start(p, REQUESTED);
        let frame = crate::frame::current();
        touch(p, frame);
        if waited_for(p) {
            wait_for_compute(p, compute_budget());
        } else {
            wait(p, frame);
        }
    }
    p.made.get()?.as_ref()
}

/// Makes `p` on this thread if it is not made yet (waiting for a worker
/// that is making it); whether it has a host pipeline.
pub(crate) fn now(p: &Arc<Pending>) -> bool {
    if p.left_out.is_some() {
        return false;
    }
    // The title's thread makes it, for as long as it takes.
    let _hold = p.made.get().is_none().then(|| {
        victory_game::watchdog::hold("host pipeline made on a title thread")
    });
    p.make();
    p.made.get().is_some_and(|made| made.is_some())
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it),
/// or if it is a confirmed opening-scene / bake producer.
pub(crate) fn early(p: &Arc<Pending>) {
    if p.left_out.is_some() {
        return;
    }
    if !p.shaders.is_empty() && (p.scene_input || super::library::wanted_early(&p.key)) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, if p.scene_input { REQUESTED } else { EARLY });
    }
}

/// Whether the frame's budget is one for all threads together: a thread
/// that waits takes what is left of it and gives back what it did not
/// use. Counted only when a wait had ended, the budget was one per thread:
/// each of the title's five recording threads began its first wait of a
/// frame with nothing counted yet and waited the whole half second, 2.5 s
/// of waiting in every frame in which pipelines were being made (runs
/// 34-49: "2504 ms waiting for pipelines" in each such frame).
/// On by default; `VICTORY_PIPELINE_WAIT_SHARED=0` counts as before.
fn shared_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WAIT_SHARED").is_none_or(|v| v != "0"))
}

/// Waits for the compute pipeline `p` for `limit` at most, on its own: not out
/// of the frame's budget, which the first thread to wait would take all of
/// (and the others, recording dispatches of their own, would drop them).
fn wait_for_compute(p: &Pending, limit: Duration) {
    let started = Instant::now();
    let _hold = victory_game::watchdog::hold("waiting for a compute pipeline");
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = limit.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed();
    if waited > Duration::from_secs(1) || p.made.get().is_none() {
        static SAID: AtomicU64 = AtomicU64::new(0);
        if SAID.fetch_add(1, Ordering::Relaxed) < 300 {
            tracing::info!(
                ms = waited.as_millis() as u64,
                made = p.made.get().is_some(),
                shaders = ?p.shaders,
                "a dispatch waited for its pipeline (VICTORY_COMPUTE_WAIT_MS=0: dropped past the frame's budget)"
            );
        }
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::PipelineWaitUs, waited.as_micros() as u64);
}

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    let screen = p.screen && screens_first();
    let budget = if screen { screen_budget() } else { wait_budget() };
    if budget.is_zero() {
        return;
    }
    // The frame the budget belongs to, and what was waited of it (or is
    // taken by a thread waiting now); of the passes over the screen too.
    static SPENT: [Mutex<(u64, Duration)>; 2] = [const { Mutex::new((u64::MAX, Duration::ZERO)) }; 2];
    let spent_of = &SPENT[usize::from(screen)];
    let shared = shared_budget();
    let left = {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 != frame {
            *spent = (frame, Duration::ZERO);
        }
        let left = budget.saturating_sub(spent.1);
        // Not worth a thread's time: the draw is dropped now, and drawn in
        // a later frame, when the pipeline is made.
        if !screen && !left.is_zero() && prioritised() && expected(p) > left {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::PipelineSkips, 1);
            return;
        }
        if shared {
            spent.1 += left;
        }
        left
    };
    if left.is_zero() {
        return;
    }
    let started = Instant::now();
    let _hold = victory_game::watchdog::hold("waiting for a host pipeline");
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    if screen && waited > Duration::from_millis(500) {
        tracing::info!(
            ms = waited.as_millis() as u64,
            made = p.made.get().is_some(),
            shaders = ?p.shaders,
            "a pass over the screen was waited for (VICTORY_SCREEN_WAIT_MS=0: dropped past half a second a frame)"
        );
    }
    {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        // A frame that began meanwhile has its own budget.
        if spent.0 == frame {
            if shared {
                spent.1 = spent.1.saturating_sub(left - waited);
            } else {
                spent.1 += waited;
            }
        }
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

/// How many workers may make pipelines at once that are known to take long
/// and that no draw has asked for: a quarter of them. On a fresh cache (a
/// change of the translator or the driver) the early list begins with
/// programs that take tens of seconds each: all 16 workers were in them
/// through the whole of the title's loading, 97 pipelines made in 20 s, and
/// the first minute and a half of the world ran at 0.5 to 1.6 s a frame,
/// dropping up to 2,900 draws a frame (run shaders-1).
/// `VICTORY_PIPELINE_SLOW=0`: in the order the title creates them, as before.
fn slow_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        if std::env::var_os("VICTORY_PIPELINE_SLOW").is_some_and(|v| v == "0") {
            usize::MAX
        } else {
            (worker_count(None, std::thread::available_parallelism().map_or(2, |n| n.get())) / 4).max(1)
        }
    })
}

struct Queue {
    /// Requested passes over the screen: before all others, and the only ones
    /// [`SCREEN_WORKERS`] take.
    screen: VecDeque<Arc<Pending>>,
    early: VecDeque<Arc<Pending>>,
    /// Those of the early ones known to take long, and how many of them are
    /// being made ([`slow_limit`]).
    slow: VecDeque<Arc<Pending>>,
    slow_now: usize,
    requested: VecDeque<Arc<Pending>>,
    /// What draws of frame `recent_frame` asked for and is not made, in the
    /// order they asked: before `requested`, which has them too and those
    /// of frames before, whose draws may be done with them.
    recent: VecDeque<Arc<Pending>>,
    recent_frame: u64,
    /// How many were ever put in `recent`.
    next_seq: u64,
}

/// The place in `Queue::recent` of its first entry (see [`Pending::seq`]).
static RECENT_FRONT: AtomicU64 = AtomicU64::new(0);

impl Queue {
    const fn new() -> Self {
        Self {
            screen: VecDeque::new(),
            early: VecDeque::new(),
            slow: VecDeque::new(),
            slow_now: 0,
            requested: VecDeque::new(),
            recent: VecDeque::new(),
            recent_frame: 0,
            next_seq: 0,
        }
    }

    /// Notes that a draw of `frame` asked for `p`, queued as requested.
    fn touch(&mut self, p: &Arc<Pending>, frame: u64) {
        if p.queue_state.load(Ordering::Relaxed) != REQUESTED || frame < self.recent_frame {
            return;
        }
        if frame > self.recent_frame {
            self.recent.clear();
            self.recent_frame = frame;
        }
        p.seq.store(self.next_seq, Ordering::Relaxed);
        self.next_seq += 1;
        self.recent.push_back(p.clone());
        self.publish();
    }

    fn publish(&self) {
        RECENT_FRONT.store(self.next_seq - self.recent.len() as u64, Ordering::Relaxed);
    }

    /// Queues an unrequested pipeline or promotes one queued at creation.
    /// Returns whether it is new to the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        self.push_as(p, priority, slow_limit() != usize::MAX && super::library::slow(&p.key))
    }

    fn push_as(&mut self, p: &Arc<Pending>, priority: u8, slow: bool) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED && p.screen && screens_first() {
            self.screen.push_back(p.clone());
        } else if priority == REQUESTED {
            self.requested.push_back(p.clone());
        } else if slow {
            self.slow.push_back(p.clone());
        } else {
            self.early.push_back(p.clone());
        }
        Some(prev == NOT_QUEUED)
    }

    fn pop(&mut self, screens_only: bool, slow_limit: usize) -> Option<Arc<Pending>> {
        // An early entry can have been promoted and claimed already.
        let unclaimed = |list: &mut VecDeque<Arc<Pending>>, state| {
            std::iter::from_fn(|| list.pop_front()).find(|p| p.queue_state.load(Ordering::Relaxed) == state)
        };
        let mut p = self.screen.pop_front();
        if p.is_none() && !screens_only {
            p = unclaimed(&mut self.recent, REQUESTED);
            self.publish();
            p = p
                .or_else(|| unclaimed(&mut self.requested, REQUESTED))
                .or_else(|| unclaimed(&mut self.early, EARLY));
            if p.is_none() && self.slow_now < slow_limit {
                p = unclaimed(&mut self.slow, EARLY);
                if let Some(p) = &p {
                    p.was_slow.store(true, Ordering::Relaxed);
                    self.slow_now += 1;
                }
            }
        }
        let p = p?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        // (One woken could be a worker that takes passes over the screen
        // alone: it would leave the pipeline to nobody.)
        AVAILABLE.notify_all();
    }
}

/// Notes that a draw of `frame` asked for `p`, which is queued: the frame's
/// requests are made before the ones of frames before.
fn touch(p: &Arc<Pending>, frame: u64) {
    if !prioritised()
        || p.touched.load(Ordering::Relaxed) == frame + 1
        || p.queue_state.load(Ordering::Relaxed) != REQUESTED
        || (p.screen && screens_first())
    {
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if p.touched.swap(frame + 1, Ordering::Relaxed) != frame + 1 {
        queue.touch(p, frame);
    }
}

/// Workers that make passes over the screen alone, besides the others.
const SCREEN_WORKERS: usize = 2;

fn next(screens_only: bool) -> Arc<Pending> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.pop(screens_only, slow_limit()) {
            return p;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// How many workers make pipelines: `asked` (`VICTORY_PIPELINE_WORKERS`, 1 to
/// 64), else two thirds of the logical processors, 2 to 16. They run below the
/// title's own threads, and a cold cache queues hundreds at once.
fn worker_count(asked: Option<usize>, processors: usize) -> usize {
    worker_count_for(asked, processors, false)
}

fn worker_count_for(asked: Option<usize>, processors: usize, limited_ram: bool) -> usize {
    match asked {
        Some(n) => n.clamp(1, 64),
        None if limited_ram => (processors * 3 / 4).clamp(2, 8),
        None => (processors * 2 / 3).clamp(2, 16),
    }
}

/// Starts the workers ([`worker_count`]) on first use.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        let workers = worker_count_for(
            std::env::var("VICTORY_PIPELINE_WORKERS")
                .ok()
                .and_then(|v| v.trim().parse().ok()),
            std::thread::available_parallelism().map_or(2, |n| n.get()),
            crate::residency::limited_host_ram(),
        );
        tracing::info!(workers, "pipeline workers");
        WORKERS.store(workers as u64, Ordering::Relaxed);
        let screens = if screens_first() { SCREEN_WORKERS } else { 0 };
        for i in 0..workers + screens {
            let screens_only = i >= workers;
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(move || {
                    // Below the title's own threads (loading, recording),
                    // and the streaming one, which is below normal itself
                    // (`VICTORY_PIPELINE_LOWEST=0`: level with it, as before).
                    let lowest = !screens_only && std::env::var_os("VICTORY_PIPELINE_LOWEST").is_none_or(|v| v != "0");
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let priority = if lowest { THREAD_PRIORITY_LOWEST } else { THREAD_PRIORITY_BELOW_NORMAL };
                        let _ = SetThreadPriority(GetCurrentThread(), priority);
                    }
                    loop {
                        let p = next(screens_only);
                        if !screens_only {
                            BUSY.fetch_add(1, Ordering::Relaxed);
                        }
                        p.make();
                        if !screens_only {
                            BUSY.fetch_sub(1, Ordering::Relaxed);
                        }
                        if p.was_slow.load(Ordering::Relaxed) {
                            let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
                            queue.slow_now -= 1;
                            AVAILABLE.notify_all();
                        }
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save(false);
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            } else {
                running = true;
            }
        }
        running
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ray_tracing_passes_follow_the_builder_and_their_switches() {
        // (hits left out, activation left out)
        // Default, without the builder: both out.
        assert_eq!(passes_left_out(false, None, None), (true, true));
        assert_eq!(passes_left_out(false, Some("0"), Some("0")), (true, true));
        // Each has its own switch.
        assert_eq!(passes_left_out(false, None, Some("1")), (true, false));
        assert_eq!(passes_left_out(false, Some("1"), None), (false, true));
        assert_eq!(passes_left_out(false, Some("1"), Some("1")), (false, false));
        // The builder brings both back.
        assert_eq!(passes_left_out(true, None, None), (false, false));
        assert_eq!(passes_left_out(true, Some("0"), Some("1")), (false, false));
        // Only the activation has a switch that takes it out again.
        assert_eq!(passes_left_out(true, None, Some("0")), (false, true));
    }

    #[test]
    fn worker_count_follows_the_processors_and_the_switch() {
        assert_eq!(worker_count(None, 1), 2);
        assert_eq!(worker_count(None, 24), 16);
        assert_eq!(worker_count(None, 64), 16);
        assert_eq!(worker_count(Some(0), 24), 1);
        assert_eq!(worker_count(Some(20), 24), 20);
        assert_eq!(worker_count_for(None, 8, true), 6);
        assert_eq!(worker_count_for(None, 16, true), 8);
        assert_eq!(worker_count_for(None, 24, true), 8);
        assert_eq!(worker_count_for(Some(12), 8, true), 12);
    }

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn pending_pipeline_is_recorded_for_the_next_launch() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "pending-pipeline-test".to_owned();
        // Simulate a worker that has taken the job but cannot finish it yet.
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);

        assert!(!super::super::library::wanted_early(&p.key));
        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(super::super::library::wanted_early(&p.key));
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &earlier));
        assert!(queue.pop(false, 1).is_none());
    }

    #[test]
    fn pipelines_known_to_take_long_leave_workers_for_the_others() {
        let new = || Arc::new(Pending::new(Vec::new(), None));
        let (long, longer, asked, quick) = (new(), new(), new(), new());
        let mut queue = Queue::new();
        for p in [&long, &longer, &asked] {
            assert_eq!(queue.push_as(p, EARLY, true), Some(true));
        }
        assert_eq!(queue.push_as(&quick, EARLY, false), Some(true));
        // The quick one first, then one long one at a time; one a draw asks
        // for waits for nobody.
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &quick));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &long));
        assert!(queue.pop(false, 1).is_none());
        assert_eq!(queue.push_as(&asked, REQUESTED, true), Some(false));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &asked));
        assert!(!asked.was_slow.load(Ordering::Relaxed));
        assert!(queue.pop(false, 1).is_none());
        queue.slow_now -= 1;
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &longer));
        assert!(queue.pop(false, 1).is_none());
    }

    /// Passes over the screen go before all others, and workers of their own
    /// take nothing else.
    #[test]
    fn passes_over_the_screen_go_first() {
        let of = |screen| {
            let mut p = Pending::new(Vec::new(), None);
            p.screen = screen;
            Arc::new(p)
        };
        let (world, screen) = (of(false), of(true));
        let mut queue = Queue::new();
        assert_eq!(queue.push(&world, REQUESTED), Some(true));
        assert_eq!(queue.push(&screen, REQUESTED), Some(true));
        assert!(Arc::ptr_eq(&queue.pop(true, 1).unwrap(), &screen));
        assert!(queue.pop(true, 1).is_none());
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &world));
    }

    /// What the frame asks for goes before the queue of earlier requests, in
    /// the order it asked, and a request of an older frame does not go back.
    #[test]
    fn the_frames_requests_go_before_earlier_ones() {
        let new = || Arc::new(Pending::new(Vec::new(), None));
        let (old, a, b, c) = (new(), new(), new(), new());
        let mut queue = Queue::new();
        for p in [&old, &a, &b, &c] {
            assert_eq!(queue.push(p, REQUESTED), Some(true));
        }
        queue.touch(&b, 7);
        queue.touch(&a, 7);
        assert_eq!(b.seq.load(Ordering::Relaxed) + 1, a.seq.load(Ordering::Relaxed));
        queue.touch(&c, 6);
        assert_eq!(queue.recent.len(), 2);
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &b));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &a));
        // The rest in the order they were queued; the two taken are skipped
        // where they sit in `requested`.
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &old));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &c));
        assert!(queue.pop(false, 1).is_none());
        // One a worker has taken is not listed again.
        queue.touch(&old, 8);
        assert_eq!(queue.recent.len(), 0);
    }

    #[test]
    fn a_new_frame_replaces_the_recent_list() {
        let new = || Arc::new(Pending::new(Vec::new(), None));
        let (a, b) = (new(), new());
        let mut queue = Queue::new();
        queue.push(&a, REQUESTED);
        queue.push(&b, REQUESTED);
        queue.touch(&a, 3);
        queue.touch(&b, 4);
        assert_eq!(queue.recent.len(), 1);
        assert!(Arc::ptr_eq(&queue.recent[0], &b));
    }

    /// A thread waits only for a pipeline the workers will have made within
    /// what is left of the frame's budget.
    #[test]
    fn waits_are_for_pipelines_that_can_be_made_in_time() {
        let ms = Duration::from_millis;
        let budget = ms(500);
        // Nothing known yet: wait, as before.
        assert!(until_made(Duration::ZERO, None, 500, 0, 8) <= budget);
        // Cheap pipelines (cached ones) and a free worker: wait.
        assert!(until_made(ms(20), None, 3, 8, 8) <= budget);
        // A compile flood, 2.7 s each, hundreds ahead of it: never.
        assert!(until_made(ms(2700), None, 300, 0, 8) > budget);
        // Even first in line with every worker busy, a 2.7 s pipeline is not.
        assert!(until_made(ms(2700), None, 0, 0, 8) > budget);
        // Cheap but far back: 400 ahead at 8 workers of 20 ms is 1 s.
        assert!(until_made(ms(20), None, 400, 0, 8) > budget);
        // One a worker is at: wait only when it is close to done.
        assert!(until_made(ms(2700), Some(ms(100)), 0, 0, 8) > budget);
        assert!(until_made(ms(2700), Some(ms(2400)), 0, 0, 8) <= budget);
        // Past the average, it may end any moment.
        assert!(until_made(ms(2700), Some(ms(5000)), 0, 0, 8) <= budget);
        // Idle workers take the first of the line at once, no more.
        assert_eq!(until_made(ms(100), None, 3, 4, 8), ms(100));
        assert!(until_made(ms(100), None, 4, 4, 8) > ms(100));
    }

    #[test]
    fn the_average_follows_what_making_takes() {
        let mut average = averaged(0, 80_000);
        assert_eq!(average, 80_000);
        for _ in 0..40 {
            average = averaged(average, 8_000);
        }
        assert!((8_000..10_000).contains(&average));
    }

    fn of_stage(stage: &'static str, code: &[u8]) -> Arc<Pending> {
        Arc::new(Pending::new(vec![super::super::shader(stage, code)], None))
    }

    /// Only compute programs are the ones whose dispatches wait for them.
    #[test]
    fn compute_programs_are_the_ones_waited_for() {
        let cs = of_stage("compute", b"cs for the policy");
        let vs = of_stage("vertex", b"vs for the policy");
        let ps = of_stage("pixel", b"ps for the policy");
        assert!(cs.compute && !vs.compute && !ps.compute);
        assert!(waited_for(&cs));
        assert!(!waited_for(&vs));
        assert!(!waited_for(&Pending::new(Vec::new(), None)));
    }

    /// A dispatch waits for as long as its pipeline takes, past the frame's
    /// budget, and several threads each wait for their own.
    #[test]
    fn a_dispatch_waits_for_its_pipeline_past_the_frames_budget() {
        let finishes = |p: &Arc<Pending>, after: Duration| {
            let p = p.clone();
            std::thread::spawn(move || {
                std::thread::sleep(after);
                p.made.get_or_init(|| None);
                let _held = DONE.0.lock().unwrap_or_else(|e| e.into_inner());
                DONE.1.notify_all();
            })
        };
        let (a, b) = (of_stage("compute", b"cs waited a"), of_stage("compute", b"cs waited b"));
        let (fa, fb) = (finishes(&a, Duration::from_millis(700)), finishes(&b, Duration::from_millis(900)));
        let started = Instant::now();
        let waiters: Vec<_> = [a.clone(), b.clone()]
            .into_iter()
            .map(|p| std::thread::spawn(move || wait_for_compute(&p, Duration::from_secs(30))))
            .collect();
        for w in waiters {
            w.join().unwrap();
        }
        assert!(a.made.get().is_some() && b.made.get().is_some());
        assert!(started.elapsed() >= Duration::from_millis(850), "{:?}", started.elapsed());
        fa.join().unwrap();
        fb.join().unwrap();
    }

    /// The wait is bounded: a pipeline that never comes is dropped at the end of it.
    #[test]
    fn the_wait_for_a_dispatch_is_bounded() {
        let p = of_stage("compute", b"cs that never comes");
        let started = Instant::now();
        wait_for_compute(&p, Duration::from_millis(150));
        assert!(p.made.get().is_none());
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(feature = "diagnostics")]
    #[test]
    fn pipelines_left_out_are_not_called_being_made() {
        let mut p = Pending::new(Vec::new(), None);
        assert_eq!(missing(&p).as_deref(), Some("being made"));
        p.left_out = Some("shades ray-tracing hits without BVH, left out");
        assert!(missing(&p).unwrap().starts_with("not made: shades"));
    }
}
