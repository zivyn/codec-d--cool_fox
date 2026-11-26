//! Where each host submission would go if the title's compute queues had a
//! host queue of their own, and what it would have to wait for there
//! (`VICTORY_ASYNC_QUEUE=plan`). Only a plan: everything still runs on the one
//! direct queue, as before; the frame line says how the plan came out.
//!
//! Why a plan and not the queue: a list runs only on a queue of its own type
//! (D3D12 refuses a compute list on a direct queue and a direct list on a
//! compute queue), and the type is chosen when the list is made, before
//! anything is recorded into it. Every host list is a direct list now, and
//! what the layer records into the title's compute lists is not all
//! something a compute list may hold: a texture a shader reads is put in both
//! read states (0x40 | 0x80, `crate::draw`), and the pixel one is a drawing
//! state; a depth texture with a colour twin is copied through a drawn pass.
//! A submission cannot be sent to the direct queue instead once its lists
//! are compute lists, so whatever a compute list cannot hold has to be known,
//! or cut out into a direct part, while it is recorded. This module is the
//! half that can be had without that: the order between two queues, and the
//! counts of what would stand in the way, so that a run shows whether the
//! rest is worth doing.
//!
//! What the plan keeps between the queues ([`Plan::place`]), each as a wait of
//! one queue for a value the other signals behind a submission (a wait for a
//! value the queue waits behind already is left out, and so is one for a
//! submission that has finished):
//!
//! - copies in the card's memory: the sends of title bytes and the compares
//!   behind the lists are numbered in the order they are sent (`relay`'s
//!   merge leaves out bytes sent after a compare's number, and each compare
//!   flags what changed since the one before it); so a submission with them
//!   waits for the last one of the other queue that had them;
//! - steps done on the card behind the flights (`card_steps`): they read what
//!   every submission sent before wrote, so they wait for the other queue's
//!   last submission;
//! - textures: their states are moved for a whole texture, in the order of
//!   the submissions, and only one queue may write a resource at a time; a
//!   submission that names a texture the other queue's last one named waits
//!   for that one;
//! - a wait for a memory value passed because a flight in the air will write
//!   it (`late_met_known`): behind that flight, if it is on the other queue;
//! - tile mappings made while a submission is prepared (they go to its
//!   queue): behind the other queue's last submission;
//! - a texture the title's compute queue names that is in a drawing state
//!   now: its state is moved by a list on the direct queue first, and the
//!   compute queue waits for that;
//! - submissions of ours (the present's copy): behind everything.
//!
//! What it does not keep: the title's own waits for fences and for memory
//! values that are in memory already need nothing, because what they wait
//! for has landed before the waiting part is sent at all (`crate::queue`'s
//! line). Two queues writing different bytes of one buffer (the title's heap
//! is one host copy) at the same time is outside D3D12's rules ("only one
//! queue can write to a resource at a time"), as it is on the console's
//! side; the plan does not order that.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

/// The list type of a title queue whose work would go to the compute queue.
pub(crate) const COMPUTE: u32 = 2;

/// A host queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Engine {
    Direct = 0,
    Compute = 1,
}

/// What a submission of the title's compute queue holds that a compute list
/// cannot hold, as the layer records it now (bits of [`Ask::holds`]).
/// A texture read in both read states (0x40 | 0x80): on a compute list it
/// would be the first alone. Counted; the plan takes it as done.
pub(crate) const READ: u8 = 1;
/// A list begins or ends with a texture in a drawing state (render target,
/// depth, resolve, index, the pixel shader's read, ...) other than that read.
/// The submission stays on the direct queue.
pub(crate) const STATE: u8 = 2;
/// A depth texture with a colour twin: copied through a drawn pass. The
/// submission stays on the direct queue.
pub(crate) const TWINS: u8 = 4;
/// A texture it names is in a drawing state now: the move out of it is
/// made on the direct queue first ([`Why::Handoff`]).
pub(crate) const NOW: u8 = 8;

/// The resource state bits a compute list may name: vertex and constant
/// buffer, unordered access, non-pixel shader resource, indirect argument,
/// copy destination and source, acceleration structure.
pub(crate) const COMPUTE_STATES: u32 = 0x1 | 0x8 | 0x40 | 0x200 | 0x400 | 0x800 | 0x40_0000;
/// The pixel shader's read state.
const PIXEL_READ: u32 = 0x80;

/// What [`READ`] and [`STATE`] say of a texture a list begins with `first`
/// and ends with `last`.
pub(crate) fn holds_of(first: u32, last: u32) -> u8 {
    let drawing = (first | last) & !COMPUTE_STATES;
    if drawing == 0 {
        0
    } else if drawing == PIXEL_READ {
        READ
    } else {
        STATE
    }
}

/// Why a submission waits for the other queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Why {
    Card = 0,
    Steps,
    Texture,
    Value,
    Tiles,
    Handoff,
    Ours,
}
const WHYS: usize = 7;

/// What is known of a submission when it is sent.
pub(crate) struct Ask<'a> {
    /// Its number among the submissions started.
    pub(crate) number: u64,
    /// The list type of the title queue it was given to (`None`: ours).
    pub(crate) title: Option<u32>,
    /// [`READ`], [`STATE`], [`TWINS`], [`NOW`].
    pub(crate) holds: u8,
    /// It has sends to copies in the card's memory or a compare behind it.
    pub(crate) card: bool,
    /// Steps of it are done on the card behind the flights.
    pub(crate) steps: bool,
    /// Tiles were mapped while it was prepared.
    pub(crate) tiles: bool,
    /// The host textures its lists name.
    pub(crate) textures: &'a [usize],
    /// The number of the submission whose bottom-of-pipe write met a wait
    /// it passed (0: none).
    pub(crate) after: u64,
}

/// Where a submission would go: the queue, the value signalled behind it
/// there, and the wait put before it, if one is needed (the other queue's
/// value, and why).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Placed {
    pub(crate) engine: Engine,
    pub(crate) value: u64,
    pub(crate) wait: Option<(u64, Why)>,
}

/// A submission as the plan remembers it: number, queue, value.
type Sent = (u64, Engine, u64);

/// How many submissions are remembered for the waits for memory values
/// (a frame has about thirty).
const REMEMBERED: usize = 4096;

/// The two queues' order ([`Plan::place`]).
#[derive(Default)]
pub(crate) struct Plan {
    /// The value signalled behind the last submission on each queue.
    sent: [u64; 2],
    /// The other queue's value each queue waits behind already.
    waited: [u64; 2],
    /// Each queue's last submission, and its last with copies in the
    /// card's memory.
    last: [Option<Sent>; 2],
    card: [Option<Sent>; 2],
    /// The last submission that named each texture.
    textures: HashMap<usize, Sent>,
    /// The submissions lately, in order.
    recent: VecDeque<Sent>,
}

impl Plan {
    /// Places the submission `ask` sent now; `finished`: the submissions up to
    /// this number have finished (nothing waits for them).
    pub(crate) fn place(&mut self, ask: &Ask, finished: u64) -> Placed {
        let engine = match ask.title {
            Some(COMPUTE) if ask.holds & (STATE | TWINS) == 0 => Engine::Compute,
            _ => Engine::Direct,
        };
        let (q, o) = (engine as usize, 1 - engine as usize);
        // What it has to come behind, on the other queue: the highest value.
        let mut need: Option<(u64, Why)> = None;
        let mut behind = |dep: Option<Sent>, why: Why| {
            if let Some((number, on, value)) = dep
                && on != engine
                && number > finished
                && need.is_none_or(|(v, _)| value > v)
            {
                need = Some((value, why));
            }
        };
        if ask.title.is_none() {
            behind(self.last[o], Why::Ours);
        }
        if ask.steps {
            behind(self.last[o], Why::Steps);
        }
        if ask.tiles {
            behind(self.last[o], Why::Tiles);
        }
        if ask.card {
            behind(self.card[o], Why::Card);
        }
        for t in ask.textures {
            behind(self.textures.get(t).copied(), Why::Texture);
        }
        if ask.after > finished {
            let at = self.recent.partition_point(|s| s.0 < ask.after);
            behind(self.recent.get(at).copied().filter(|s| s.0 == ask.after), Why::Value);
        }
        if engine == Engine::Compute && ask.holds & NOW != 0 {
            // A list of the direct queue's, after everything sent there,
            // with a signal of its own.
            self.sent[Engine::Direct as usize] += 1;
            let handoff = (u64::MAX, Engine::Direct, self.sent[Engine::Direct as usize]);
            self.last[Engine::Direct as usize] = Some(handoff);
            behind(Some(handoff), Why::Handoff);
        }
        let wait = need.filter(|&(value, _)| value > self.waited[q]);
        if let Some((value, _)) = wait {
            self.waited[q] = value;
        }
        self.sent[q] += 1;
        let sent = (ask.number, engine, self.sent[q]);
        self.last[q] = Some(sent);
        if ask.card {
            self.card[q] = Some(sent);
        }
        for &t in ask.textures {
            self.textures.insert(t, sent);
        }
        if self.textures.len() > 4 * REMEMBERED {
            self.textures.retain(|_, s| s.0 > finished);
        }
        self.recent.push_back(sent);
        if self.recent.len() > REMEMBERED {
            self.recent.pop_front();
        }
        Placed {
            engine,
            value: self.sent[q],
            wait,
        }
    }
}

/// Whether submissions are planned for two queues
/// (`VICTORY_ASYNC_QUEUE=plan`; off by default).
///
/// The gameplay frame is about 45 ms, and the card is busy for most of it
/// running about 11,000 small commands at 130-140 W of its 220: it waits on
/// itself more than it works. A frame timeline (tl.190) had the submission
/// the title waited for stand 7.2 ms sent and not begun, behind other work
/// on the one queue. The title runs part of its work on compute queues of
/// its own, beside its drawing, and on the console that work overlaps.
/// Whether giving it a host queue of its own is worth the work it takes is
/// what this plan is for: how many submissions would go there, how many
/// would stay for what they hold, and how many waits between the queues
/// it would take. `VICTORY_ASYNC_QUEUE=1` (the queue itself) is not built:
/// it plans and says so.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_ASYNC_QUEUE").as_deref() {
        Ok("plan") => {
            tracing::info!("submissions are planned for a compute queue beside the direct queue (VICTORY_ASYNC_QUEUE=plan); everything runs as before");
            true
        }
        Ok("1") => {
            tracing::warn!(
                "VICTORY_ASYNC_QUEUE=1: the title's compute work on a host compute queue is not built (its lists are recorded as \
                 direct lists); planned only, as with VICTORY_ASYNC_QUEUE=plan, and everything runs on the direct queue"
            );
            true
        }
        _ => false,
    })
}

thread_local! {
    /// The title queue whose submission this thread sends now: its list
    /// type, and the submission a wait it passed relies on ([`for_title`]).
    static TITLE: std::cell::Cell<Option<(u32, u64)>> = const { std::cell::Cell::new(None) };
}

/// Runs `send`, which sends a submission of a title queue whose lists are of
/// type `kind`; `after` as in [`Ask::after`].
pub(crate) fn for_title<R>(kind: Option<u32>, after: u64, send: impl FnOnce() -> R) -> R {
    let was = TITLE.replace(kind.map(|k| (k, after)));
    let sent = send();
    TITLE.set(was);
    sent
}

/// The list type of the title queue whose submission this thread sends.
#[cfg(feature = "diagnostics")]
pub(crate) fn title_kind() -> Option<u32> {
    TITLE.get().map(|t| t.0)
}

/// Waits for memory values passed because a flight will write them, by the
/// title queue that passed them (its address) and the flight's number.
static AFTER: Mutex<Vec<(usize, u64)>> = Mutex::new(Vec::new());

/// Queue `queue` passed `wait`: if a flight in the air meets it, its next
/// submission comes behind that flight.
pub(crate) fn value_passed(queue: usize, wait: &crate::list::ValueWait) {
    if !on() {
        return;
    }
    let Some(by) = super::late_met_by(wait) else {
        return;
    };
    let mut after = AFTER.lock().unwrap_or_else(|e| e.into_inner());
    match after.iter_mut().find(|a| a.0 == queue) {
        Some(a) => a.1 = a.1.max(by),
        None => after.push((queue, by)),
    }
}

/// What queue `queue`'s next submission comes behind ([`value_passed`]; 0:
/// nothing), taken.
pub(crate) fn take_after(queue: usize) -> u64 {
    if !on() {
        return 0;
    }
    let mut after = AFTER.lock().unwrap_or_else(|e| e.into_inner());
    match after.iter().position(|a| a.0 == queue) {
        Some(i) => after.swap_remove(i).1,
        None => 0,
    }
}

/// Tile mappings put on the direct queue so far ([`Ask::tiles`]).
static TILES: AtomicU64 = AtomicU64::new(0);

/// Tiles were mapped on the direct queue.
pub(crate) fn tiles_mapped() {
    TILES.fetch_add(1, Relaxed);
}

/// What [`Host::submit`](super::Host) finds out about a submission before
/// its lists' preludes move the textures' states.
pub(crate) struct Probe {
    title: Option<(u32, u64)>,
    holds: u8,
    textures: Vec<usize>,
    tiles: u64,
}

/// Looks at the lists of a submission about to be prepared.
pub(crate) fn probe(runs: &[&super::HostList]) -> Probe {
    let mut holds = 0;
    let mut textures = Vec::new();
    for list in runs {
        let states = list.states.lock().unwrap_or_else(|e| e.into_inner());
        for (key, (first, last, copy)) in states.iter() {
            textures.push(*key);
            holds |= holds_of(*first, *last);
            if copy.state.load(Relaxed) & !COMPUTE_STATES != 0 {
                holds |= NOW;
            }
        }
        drop(states);
        if !list.twins.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
            holds |= TWINS;
        }
    }
    textures.sort_unstable();
    textures.dedup();
    Probe {
        title: TITLE.get(),
        holds,
        textures,
        tiles: TILES.load(Relaxed),
    }
}

static PLAN: Mutex<Option<Plan>> = Mutex::new(None);

/// Since the last frame line: submissions by the title queue's type (direct,
/// compute, copy, ours or not known); lists of the compute queues'; of
/// those submissions, the ones with [`READ`], [`STATE`], [`TWINS`], [`NOW`],
/// the ones placed on the compute queue, and those of them without a wait;
/// waits by [`Why`]; waits of the compute queue for the direct one.
static STATS: [AtomicU64; 12 + WHYS] = [const { AtomicU64::new(0) }; 12 + WHYS];
const LISTS: usize = 4;
const HELD: usize = 5;
const PLACED: usize = 9;
const FREE: usize = 10;
const COMPUTE_WAITS: usize = 11;
const WAITS: usize = 12;

/// Places a submission ([`Plan::place`]): `number` as in [`Ask::number`],
/// `card` and `steps` as there, `lists` its host lists, `finished` as there.
pub(crate) fn place(probe: Probe, number: u64, card: bool, steps: bool, lists: usize, finished: u64) {
    let title = probe.title.map(|t| t.0);
    let ask = Ask {
        number,
        title,
        holds: probe.holds,
        card,
        steps,
        tiles: TILES.load(Relaxed) != probe.tiles,
        textures: &probe.textures,
        after: probe.title.map_or(0, |t| t.1),
    };
    let placed = {
        let mut plan = PLAN.lock().unwrap_or_else(|e| e.into_inner());
        plan.get_or_insert_with(Plan::default).place(&ask, finished)
    };
    let by = match title {
        Some(0) => 0,
        Some(COMPUTE) => 1,
        Some(3) => 2,
        _ => 3,
    };
    STATS[by].fetch_add(1, Relaxed);
    if title == Some(COMPUTE) {
        STATS[LISTS].fetch_add(lists as u64, Relaxed);
        for (i, bit) in [READ, STATE, TWINS, NOW].into_iter().enumerate() {
            if ask.holds & bit != 0 {
                STATS[HELD + i].fetch_add(1, Relaxed);
            }
        }
    }
    if placed.engine == Engine::Compute {
        STATS[PLACED].fetch_add(1, Relaxed);
        if placed.wait.is_none() {
            STATS[FREE].fetch_add(1, Relaxed);
        }
    }
    if let Some((_, why)) = placed.wait {
        STATS[WAITS + why as usize].fetch_add(1, Relaxed);
        if placed.engine == Engine::Compute {
            STATS[COMPUTE_WAITS].fetch_add(1, Relaxed);
        }
    }
}

/// For the frame-rate line.
pub(crate) fn line(frames: f64) {
    if !on() {
        return;
    }
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Relaxed) as f64 / frames).collect();
    let waits: f64 = s[WAITS..].iter().sum();
    tracing::info!(
        "two host queues, planned per frame (VICTORY_ASYNC_QUEUE; everything runs on the direct queue): submissions of the \
         title's direct queue {:.1}, of its compute queues {:.1} ({:.1} lists), of its copy queues {:.1}, ours or not known {:.1}; \
         of the compute queues' submissions, {:.1} read a texture in both read states (a compute list would name the first \
         alone), {:.1} begin or end a texture in a drawing state and {:.1} copy depth twins (both stay on the direct queue), \
         {:.1} name a texture in a drawing state now (moved on the direct queue first); {:.1} would go to a compute queue, \
         {:.1} of them without a wait; waits between the queues {waits:.1}, {:.1} of them by the compute queue: for copies in \
         the card's memory {:.1}, steps on the card {:.1}, textures {:.1}, memory values a flight of the other queue writes \
         {:.1}, tile mappings {:.1}, a texture's state moved first {:.1}, ours {:.1}",
        s[0],
        s[1],
        s[LISTS],
        s[2],
        s[3],
        s[HELD],
        s[HELD + 1],
        s[HELD + 2],
        s[HELD + 3],
        s[PLACED],
        s[FREE],
        s[COMPUTE_WAITS],
        s[WAITS + Why::Card as usize],
        s[WAITS + Why::Steps as usize],
        s[WAITS + Why::Texture as usize],
        s[WAITS + Why::Value as usize],
        s[WAITS + Why::Tiles as usize],
        s[WAITS + Why::Handoff as usize],
        s[WAITS + Why::Ours as usize],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(number: u64, title: Option<u32>) -> Ask<'static> {
        Ask {
            number,
            title,
            holds: 0,
            card: false,
            steps: false,
            tiles: false,
            textures: &[],
            after: 0,
        }
    }

    const DIRECT: Option<u32> = Some(0);
    const COMPUTE_QUEUE: Option<u32> = Some(COMPUTE);

    /// The title's direct and copy queues stay on the direct queue, its
    /// compute queues go to the compute queue; each queue counts its own
    /// values, and nothing waits where nothing is shared.
    #[test]
    fn queues_are_kept_apart_and_count_their_own_values() {
        let mut plan = Plan::default();
        let a = plan.place(&ask(1, DIRECT), 0);
        let b = plan.place(&ask(2, COMPUTE_QUEUE), 0);
        let c = plan.place(&ask(3, Some(3)), 0);
        let d = plan.place(&ask(4, COMPUTE_QUEUE), 0);
        assert_eq!((a.engine, a.value, a.wait), (Engine::Direct, 1, None));
        assert_eq!((b.engine, b.value, b.wait), (Engine::Compute, 1, None));
        assert_eq!((c.engine, c.value), (Engine::Direct, 2));
        assert_eq!((d.engine, d.value, d.wait), (Engine::Compute, 2, None));
    }

    /// A texture named on both queues: the second comes behind the first,
    /// on the other queue; on the same queue the order is the queue's.
    #[test]
    fn a_texture_shared_between_the_queues_is_waited_for() {
        let mut plan = Plan::default();
        let t = [0x1000usize];
        plan.place(&Ask { textures: &t, ..ask(1, DIRECT) }, 0);
        plan.place(&Ask { textures: &t, ..ask(2, DIRECT) }, 0);
        let c = plan.place(&Ask { textures: &t, ..ask(3, COMPUTE_QUEUE) }, 0);
        assert_eq!(c.wait, Some((2, Why::Texture)));
        let d = plan.place(&Ask { textures: &t, ..ask(4, DIRECT) }, 0);
        assert_eq!((d.engine, d.wait), (Engine::Direct, Some((1, Why::Texture))));
        let e = plan.place(&Ask { textures: &t, ..ask(5, DIRECT) }, 0);
        assert_eq!(e.wait, None, "named last on the same queue");
    }

    /// A wait for a value the queue waits behind already is left out, and so
    /// is one for a submission that has finished.
    #[test]
    fn waits_are_only_put_where_they_are_new() {
        let mut plan = Plan::default();
        let (t, u) = ([1usize], [2usize]);
        plan.place(&Ask { textures: &t, ..ask(1, DIRECT) }, 0);
        plan.place(&Ask { textures: &u, ..ask(2, DIRECT) }, 0);
        let c = plan.place(&Ask { textures: &u, ..ask(3, COMPUTE_QUEUE) }, 0);
        assert_eq!(c.wait, Some((2, Why::Texture)));
        // Texture 1 was last named by the direct queue's 1: behind it already.
        let d = plan.place(&Ask { textures: &t, ..ask(4, COMPUTE_QUEUE) }, 0);
        assert_eq!(d.wait, None);
        plan.place(&Ask { textures: &t, ..ask(5, DIRECT) }, 0);
        // Submission 5 has finished: nothing to wait for.
        let f = plan.place(&Ask { textures: &t, ..ask(6, COMPUTE_QUEUE) }, 5);
        assert_eq!(f.wait, None);
    }

    /// Copies in the card's memory chain the queues: a submission with them
    /// comes behind the other queue's last one with them, not behind the rest.
    #[test]
    fn copies_in_the_cards_memory_are_sent_in_one_order() {
        let mut plan = Plan::default();
        plan.place(&Ask { card: true, ..ask(1, DIRECT) }, 0);
        plan.place(&ask(2, DIRECT), 0);
        let c = plan.place(&Ask { card: true, ..ask(3, COMPUTE_QUEUE) }, 0);
        assert_eq!(c.wait, Some((1, Why::Card)));
        let d = plan.place(&ask(4, COMPUTE_QUEUE), 0);
        assert_eq!(d.wait, None);
        let e = plan.place(&Ask { card: true, ..ask(5, DIRECT) }, 0);
        assert_eq!(e.wait, Some((1, Why::Card)), "behind the compute queue's first, its only one with copies");
    }

    /// Steps on the card, tile mappings and submissions of ours come behind
    /// everything the other queue was given.
    #[test]
    fn steps_tiles_and_ours_wait_for_everything_before() {
        let mut plan = Plan::default();
        plan.place(&ask(1, COMPUTE_QUEUE), 0);
        plan.place(&ask(2, COMPUTE_QUEUE), 0);
        let s = plan.place(&Ask { steps: true, ..ask(3, DIRECT) }, 0);
        assert_eq!(s.wait, Some((2, Why::Steps)));
        plan.place(&ask(4, COMPUTE_QUEUE), 0);
        let o = plan.place(&ask(5, None), 0);
        assert_eq!((o.engine, o.wait), (Engine::Direct, Some((3, Why::Ours))));
        plan.place(&ask(6, DIRECT), 0);
        let t = plan.place(&Ask { tiles: true, ..ask(7, COMPUTE_QUEUE) }, 0);
        assert_eq!(t.wait, Some((3, Why::Tiles)));
    }

    /// A wait for a memory value passed because a flight will write it: behind
    /// that flight if it is on the other queue, nothing if it is on the same
    /// one or has finished.
    #[test]
    fn a_value_a_flight_writes_is_waited_for_on_the_other_queue() {
        let mut plan = Plan::default();
        plan.place(&ask(1, DIRECT), 0);
        plan.place(&ask(2, COMPUTE_QUEUE), 0);
        plan.place(&ask(3, DIRECT), 0);
        let a = plan.place(&Ask { after: 1, ..ask(4, COMPUTE_QUEUE) }, 0);
        assert_eq!(a.wait, Some((1, Why::Value)));
        let b = plan.place(&Ask { after: 2, ..ask(5, COMPUTE_QUEUE) }, 0);
        assert_eq!(b.wait, None, "the same queue");
        let c = plan.place(&Ask { after: 4, ..ask(6, DIRECT) }, 4);
        assert_eq!(c.wait, None, "finished");
        let d = plan.place(&Ask { after: 5, ..ask(7, DIRECT) }, 4);
        assert_eq!(d.wait, Some((3, Why::Value)));
    }

    /// A compute submission whose lists hold what a compute list cannot stays
    /// on the direct queue; one that names a texture in a drawing state has
    /// that state moved on the direct queue first, behind everything there.
    #[test]
    fn what_a_compute_list_cannot_hold_keeps_it_on_the_direct_queue() {
        let mut plan = Plan::default();
        plan.place(&ask(1, DIRECT), 0);
        for holds in [STATE, TWINS, STATE | READ] {
            let p = plan.place(&Ask { holds, ..ask(2, COMPUTE_QUEUE) }, 0);
            assert_eq!(p.engine, Engine::Direct);
        }
        let r = plan.place(&Ask { holds: READ, ..ask(3, COMPUTE_QUEUE) }, 0);
        assert_eq!((r.engine, r.wait), (Engine::Compute, None));
        let n = plan.place(&Ask { holds: NOW, ..ask(4, COMPUTE_QUEUE) }, 0);
        // Direct: 1, then the three kept, then the handoff's own signal.
        assert_eq!((n.engine, n.wait), (Engine::Compute, Some((5, Why::Handoff))));
        let after = plan.place(&ask(5, DIRECT), 0);
        assert_eq!(after.value, 6);
    }

    /// The states a list begins and ends a texture with: the compute ones
    /// pass, the pixel shader's read alone is a read, anything else drawing.
    #[test]
    fn states_are_sorted_into_what_a_compute_list_may_name() {
        assert_eq!(holds_of(0x8, 0x40), 0);
        assert_eq!(holds_of(0x40 | 0x80, 0x8), READ);
        assert_eq!(holds_of(0x4, 0x40), STATE);
        assert_eq!(holds_of(0x20 | 0x40 | 0x80, 0x40), STATE);
        assert_eq!(holds_of(0x0, 0x800), 0);
    }
}
