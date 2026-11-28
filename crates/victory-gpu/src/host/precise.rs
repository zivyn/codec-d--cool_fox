//! Precise write sets: what the shaders of a list write, by what their code says and what the dispatch gives them,
//! in place of the whole view they store through.
//!
//! A list declares written ([`crate::host::Step::GpuWrite`]) the whole of every unordered-access view its shaders may
//! store through: after the card has run it those bytes are compared and copied back into title memory, no refresh of
//! the title's bytes goes over them while the flights are in the air, and a later part may not be prepared together
//! with it if it reads them on the CPU ([`crate::host::joinable`]). The title binds views of its 182 MB heap, which holds
//! the rings of constants that the CPU writes and the next parts' fills read: by construction nearly any part after a
//! part that dispatches into that heap "reads what the part in front writes", and the 13.4 ms a frame of "title bytes
//! under flights' writes" is the merging of the title's constants into what was declared.
//!
//! `victory_shader::footprint` knows per shader which descriptors its stores go through and at which byte offsets,
//! as expressions of thread ids, group ids and root constants. Here they are evaluated when the draw is bound (the
//! groups of the dispatch, the root constants the title set), for each unordered-access entry the shader names, and
//! [`declare`] takes the place of the whole-view declaration at the one place the views are made readable:
//!
//! - `VICTORY_PRECISE_LOOK=1`: the declaration is the whole view as before, and the precise ranges are kept beside it
//!   ([`Side`]), so that the frame line can say how many of the refusals to prepare a part together with the ones in
//!   front the precise ranges would not make ([`refused`]). No behaviour changes.
//! - `VICTORY_PRECISE_WRITES=1`: the declaration is the precise ranges (a view the shader does not store through
//!   is not declared at all). A view whose stores cannot be bounded is declared whole, as before.
//! - `VICTORY_JOIN_WAITS=1` (stage 3): a part behind a wait for a fence that a signal among the flights in the air
//!   meets, or for a value that a write of a list taken along covers in part, is taken along ([`signalled`],
//!   [`covers`]); what ends the joining at a value wait is counted by cause ([`value_why`]).
//!
//! Every switch reads at each use (the A/B in one process changes it while the title runs): a list keeps the
//! declaration it was recorded with, whichever the switch is when it is submitted.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

use victory_shader::footprint::{Bind, Target};
use victory_shader::{ResourceType, RootType, UsedResource};

use super::{HostList, join_ranges};
use crate::pipeline::host::Stage;

/// Whether the precise ranges are made and kept beside the declarations (`VICTORY_PRECISE_LOOK=1`).
pub(crate) fn look() -> bool {
    crate::ab::live_flag!("VICTORY_PRECISE_LOOK")
}

/// Whether the declarations are the precise ranges (`VICTORY_PRECISE_WRITES=1`).
pub(crate) fn narrow() -> bool {
    crate::ab::live_flag!("VICTORY_PRECISE_WRITES")
}

/// Whether a part behind a wait that is met in a way not seen before is taken along (`VICTORY_JOIN_WAITS=1`).
pub(crate) fn waits() -> bool {
    crate::ab::live_flag!("VICTORY_JOIN_WAITS")
}

/// A range of a title view is declared to the nearest 64 bytes out: the analysis is exact by the bytes, this is for
/// the narrow stores that go through the dword around them.
const GRAIN: u64 = 64;

/// The ranges of an unordered-access entry, as `Footprint::ranges` gave them for the draw being bound: `None` is
/// the whole view.
type Found = Option<Vec<(u64, u64)>>;

thread_local! {
    /// The groups of the dispatch being bound (none for draws and indirect dispatches).
    static GROUPS: Cell<Option<[u32; 3]>> = const { Cell::new(None) };
    /// The entries of the stage being bound, by target.
    static ENTRIES: RefCell<Vec<(Target, Found)>> = const { RefCell::new(Vec::new()) };
    /// While [`declare`] gives the whole view to the list for the ranges it keeps itself ([`whole`]).
    static SKIP: Cell<bool> = const { Cell::new(false) };
}

/// Since the last frame-rate line: views declared, the bytes of those views, the bytes declared by the precise
/// ranges (the same in `VICTORY_PRECISE_LOOK`, where they are not what is declared), views whose ranges were
/// bounded, views left whole because their shader's stores could not be bounded, views left whole because the
/// shader has no analysis, views that no store goes through.
static VIEWS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
/// Since the last frame-rate line: refusals to prepare a part with the ones in front for what they write, and
/// those the precise ranges would not have made; waits for fences that a signal among the flights met and the
/// part taken along, values partly covered by a write along and the part taken along.
static JOINS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// Since the last frame-rate line: why a wait for a value ended the joining: nothing in the air and nothing along
/// writes there (the title's CPU or another queue must); flights in the air that do not touch it; flights whose
/// shaders may write there; a list along writes there but not what is asked and not covering it; one whose write
/// covers it and does not meet it; one that covers it and meets it (what `VICTORY_JOIN_WAITS` takes along).
static WHY: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
/// Since the last frame-rate line: why a wait for a fence ended the joining: none of the flights' signals meets
/// it; one does (what `VICTORY_JOIN_WAITS` takes along).
static FENCES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The ranges a list's shaders write by their code, beside the whole views it declares (`VICTORY_PRECISE_LOOK`):
/// title addresses, each once.
#[derive(Default)]
pub(crate) struct Side {
    seen: HashSet<(usize, usize)>,
    ranges: Vec<(usize, usize)>,
    /// Whether anything was kept: a list recorded with the switch off declares what it declares.
    on: bool,
}

impl Side {
    fn add(&mut self, from: usize, to: usize) {
        if from < to {
            self.on = true;
            if self.seen.insert((from, to)) {
                self.ranges.push((from, to));
            }
        }
    }
}

/// A declaration of the whole of `[address, address + len)` that [`declare`] did not make: kept as it is.
pub(crate) fn whole(side: &mut Side, address: usize, len: usize) {
    if !SKIP.get() && look() {
        side.add(address, address.saturating_add(len));
    }
}

/// The target of a table entry of the used resources.
pub(crate) fn table(u: &UsedResource) -> Target {
    Target::Table { user_data: u.user_data, dword: u.table_offset_dwords }
}

/// The target of a root view.
pub(crate) fn root(slot: u8) -> Target {
    Target::Root { slot }
}

/// Keeps the groups of a dispatch while it is bound; dropped, they are unknown again.
pub(crate) struct Extent;

impl Drop for Extent {
    fn drop(&mut self) {
        GROUPS.set(None);
    }
}

pub(crate) fn extent(groups: [u32; 3]) -> Extent {
    GROUPS.set(Some(groups));
    Extent
}

/// Keeps the ranges of a stage's entries while it is bound.
pub(crate) struct Entered;

impl Drop for Entered {
    fn drop(&mut self) {
        ENTRIES.with(|e| e.borrow_mut().clear());
    }
}

/// Evaluates what `stage`'s shader stores through each of its unordered-access entries for the draw being bound:
/// `constants` gives the root constants the title set. Nothing is done unless a switch is on and the shader can
/// store to buffers at all.
pub(crate) fn enter(stage: &Stage, compute: bool, constants: &dyn Fn(u8, u32) -> Option<u32>) -> Option<Entered> {
    if !(narrow() || look()) {
        return None;
    }
    let t = stage.translated();
    if !t.stores {
        return None;
    }
    let footprint = stage.shader.footprint()?;
    // (A compute program's threads in a group are not known for a few: then nothing is bounded.)
    // (A draw's stages have no thread ids the code could name: if it did, they would be anything.)
    let threads = if compute { t.threads? } else { [u32::MAX; 3] };
    let bind = Bind { threads, groups: GROUPS.get(), constants };
    ENTRIES.with(|e| {
        let mut e = e.borrow_mut();
        e.clear();
        for u in t.used.iter().filter(|u| u.kind == ResourceType::Uav && u.root == RootType::Table) {
            let target = table(u);
            e.push((target, footprint.ranges(target, &bind)));
        }
        for &(slot, _) in &stage.root_uavs {
            let target = root(slot);
            e.push((target, footprint.ranges(target, &bind)));
        }
    });
    Some(Entered)
}

/// What the stage being bound stores through `target`, if it was gone through.
fn found(target: Option<Target>) -> Option<Found> {
    let target = target?;
    ENTRIES.with(|e| e.borrow().iter().find(|(t, _)| *t == target).map(|(_, f)| f.clone()))
}

/// The unordered-access view `[address, address + len)` of `shadow` is written by the shaders of `list`: declared
/// (`Shadow::gpu_writes`) whole, or by the ranges of its target in the stage being bound.
pub(crate) fn declare(
    list: &HostList,
    shadow: &Arc<crate::shadow::Shadow>,
    address: u64,
    len: u64,
    target: Option<Target>,
) {
    let (look, narrow) = (look(), narrow());
    if !look && !narrow {
        shadow.gpu_writes(list, address, len);
        return;
    }
    declare_as(list, shadow, address, len, target, (look, narrow));
}

/// [`declare`] with the switches as given: looking, or narrowing.
fn declare_as(
    list: &HostList,
    shadow: &Arc<crate::shadow::Shadow>,
    address: u64,
    len: u64,
    target: Option<Target>,
    (_, narrow): (bool, bool),
) {
    let answer = found(target);
    VIEWS[0].fetch_add(1, Relaxed);
    VIEWS[1].fetch_add(len, Relaxed);
    let ranges: Option<Vec<(u64, u64)>> = match &answer {
        Some(Some(ranges)) => Some(
            ranges
                .iter()
                .filter_map(|&(lo, hi)| {
                    let from = lo / GRAIN * GRAIN;
                    let to = hi.div_ceil(GRAIN).saturating_mul(GRAIN);
                    (from.min(len) < to.min(len)).then_some((from.min(len), to.min(len)))
                })
                .collect(),
        ),
        _ => None,
    };
    match (&answer, &ranges) {
        (Some(Some(_)), Some(r)) if r.is_empty() => VIEWS[6].fetch_add(1, Relaxed),
        (Some(Some(_)), _) => VIEWS[3].fetch_add(1, Relaxed),
        (Some(None), _) => VIEWS[4].fetch_add(1, Relaxed),
        (None, _) => VIEWS[5].fetch_add(1, Relaxed),
    };
    VIEWS[2].fetch_add(ranges.as_ref().map_or(len, |r| r.iter().map(|&(a, b)| b - a).sum()), Relaxed);
    if narrow {
        match ranges {
            Some(ranges) => {
                for (from, to) in ranges {
                    shadow.gpu_writes(list, address + from, to - from);
                }
            }
            None => shadow.gpu_writes(list, address, len),
        }
        return;
    }
    // Looking: the list declares what it always did, and keeps the precise ranges beside it.
    SKIP.set(true);
    shadow.gpu_writes(list, address, len);
    SKIP.set(false);
    let mut deferred = list.deferred();
    deferred.precise.on = true;
    match ranges {
        Some(ranges) => {
            for (from, to) in ranges {
                deferred.precise.add((address + from) as usize, (address + to) as usize);
            }
        }
        None => deferred.precise.add(address as usize, (address + len) as usize),
    }
}

/// A part could not be prepared together with `ahead` because `next` reads what they write: whether it could have
/// been by the precise ranges the lists in front kept (`VICTORY_PRECISE_LOOK`; without them, no).
pub(crate) fn refused<'a>(why: super::NoJoin, ahead: impl Iterator<Item = &'a HostList>, next: &[&HostList]) {
    use super::NoJoin;
    if !look() || why == NoJoin::CopyOrRun {
        return;
    }
    JOINS[0].fetch_add(1, Relaxed);
    let mut sure: Vec<(usize, usize)> = Vec::new();
    let mut declared: Vec<(usize, usize)> = Vec::new();
    let mut kept = false;
    for list in ahead {
        let d = list.deferred();
        if d.precise.on {
            kept = true;
            declared.extend(d.precise.ranges.iter().copied());
        } else {
            declared.extend(d.copies.iter().filter_map(|s| match s {
                super::Step::GpuWrite(_, at, len) => Some((*at, *at + *len)),
                _ => None,
            }));
        }
        sure.extend(d.writes.iter().map(|&(at, _, bytes)| (at, at + bytes)));
        sure.extend(
            d.backs
                .iter()
                .map(|&(_, at, [row, rows, pitch])| (at, at + rows.saturating_sub(1) * pitch + row)),
        );
    }
    if !kept {
        return;
    }
    let mut joined = Vec::new();
    join_ranges(&mut joined, declared.into_iter().chain(sure));
    if super::blocked_by(&joined, next, false).is_none() {
        JOINS[1].fetch_add(1, Relaxed);
    }
}

/// Whether a signal of a fence that the flights in the air hold (behind the flights before it) meets a wait for
/// `value`: a part sent behind them runs after the work the signal stands behind, on the host's one queue.
pub(crate) fn signalled(fence: usize, value: u64) -> bool {
    let line = super::line();
    line.line.iter().any(|late| matches!(late, super::Late::Signal(f, v) if *f == fence && *v >= value))
}

/// A wait for a fence that nothing along signals ends the joining: counted by whether the flights' signals meet it.
/// Whether it is taken along instead (`VICTORY_JOIN_WAITS`).
pub(crate) fn fence_wait(fence: usize, value: u64) -> bool {
    let met = signalled(fence, value);
    if met && waits() {
        JOINS[2].fetch_add(1, Relaxed);
        return true;
    }
    FENCES[usize::from(met)].fetch_add(1, Relaxed);
    false
}

/// How the last write of `list` that touches the place of `wait` stands to it: `Some(true)` if it is a value write that
/// covers the place (not only one that begins there) with the bytes that meet the wait, `Some(false)` if it covers
/// it and they do not; `None` if none touches it, or it is a timestamp or leaves part of the place out.
fn covering(list: &HostList, wait: &crate::list::ValueWait) -> Option<bool> {
    let (address, bytes) = wait.place();
    let deferred = list.deferred();
    let last = deferred.writes.iter().rev().find(|(at, _, n)| *at < address + bytes && address < *at + *n);
    match last {
        Some((at, crate::queue::Written::Value(v), n)) if *at <= address && address + bytes <= *at + *n => {
            let shift = 8 * (address - *at) as u32;
            (shift < 64).then(|| wait.holds_for(*v >> shift))
        }
        _ => None,
    }
}

/// Whether a write of `list` that covers the place of `wait` meets it ([`covering`]).
pub(crate) fn covers(list: &HostList, wait: &crate::list::ValueWait) -> bool {
    covering(list, wait) == Some(true)
}

/// A wait for a value that is not met and not known to be met by the flights or by what is along ends the joining:
/// why, counted ([`WHY`]); whether a write of a list along covers it and meets it (`VICTORY_JOIN_WAITS` takes the
/// part along then).
pub(crate) fn value_wait<'a>(wait: &crate::list::ValueWait, along: impl Iterator<Item = &'a HostList>) -> bool {
    let (address, bytes) = wait.place();
    let (mut writes, mut asked, mut covered) = (false, false, false);
    for list in along {
        match covering(list, wait) {
            Some(true) => {
                asked = true;
                break;
            }
            Some(false) => covered = true,
            None => writes |= list.deferred().writes.iter().any(|(at, _, n)| *at < address + bytes && address < *at + *n),
        }
    }
    if asked && waits() {
        JOINS[3].fetch_add(1, Relaxed);
        return true;
    }
    let class = if asked {
        5
    } else if covered {
        4
    } else if writes {
        3
    } else {
        let flying = {
            let line = super::line();
            !(line.line.is_empty() && line.in_hand.is_empty())
        };
        match super::late_met_known(wait) {
            None => 2,
            Some(_) if flying => 1,
            Some(_) => 0,
        }
    };
    WHY[class].fetch_add(1, Relaxed);
    false
}

/// For the frame-rate line.
pub(crate) fn line(frames: f64) {
    let views: Vec<u64> = VIEWS.iter().map(|v| v.swap(0, Relaxed)).collect();
    let joins: Vec<u64> = JOINS.iter().map(|v| v.swap(0, Relaxed)).collect();
    let why: Vec<u64> = WHY.iter().map(|v| v.swap(0, Relaxed)).collect();
    let fences: Vec<u64> = FENCES.iter().map(|v| v.swap(0, Relaxed)).collect();
    if views[0] + joins[0] + why.iter().sum::<u64>() + fences.iter().sum::<u64>() + joins[2] + joins[3] == 0 {
        return;
    }
    let mb = |b: u64| b as f64 / (1 << 20) as f64 / frames;
    let n = |c: u64| c as f64 / frames;
    tracing::info!(
        "precise write sets, per frame (VICTORY_PRECISE_LOOK, VICTORY_PRECISE_WRITES): {:.1} views declared, {:.1} MB of them whole, {:.1} MB by the code of the shaders \
         ({:.1} bounded, {:.1} left whole as their stores cannot be bounded, {:.1} left whole as the shader has no analysis, {:.1} not stored through); of the {:.1} \
         refusals to prepare a part with the ones in front for what they write, the precise ranges would have made {:.1} join",
        n(views[0]),
        mb(views[1]),
        mb(views[2]),
        n(views[3]),
        n(views[4]),
        n(views[5]),
        n(views[6]),
        n(joins[0]),
        n(joins[1]),
    );
    tracing::info!(
        "waits that end the joining of parts, per frame (VICTORY_JOIN_WAITS): for a value, with nothing in the air and nothing along writing there {:.1}, with flights \
         in the air that do not touch it {:.1}, with flights whose shaders may write there {:.1}, with a list along writing there but not covering it or not as asked {:.1}, \
         with a write along that covers it but not as asked {:.1}, with one that covers it as asked {:.1}; for a fence, that no signal in the air meets {:.1} and that one \
         does {:.1}; taken along with the switch: {:.1} fences, {:.1} values",
        n(why[0]),
        n(why[1]),
        n(why[2]),
        n(why[3]),
        n(why[4]),
        n(why[5]),
        n(fences[0]),
        n(fences[1]),
        n(joins[2]),
        n(joins[3]),
    );
}

#[cfg(test)]
mod device_tests {
    use super::super::*;
    use super::{ENTRIES, Found, Target, declare_as};
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};

    const WRITE_WATCH: u32 = 0x20_0000;
    const LEN: usize = 4 << 20;
    /// Written by the first part's shader, which declares it; read by the second part's fill; written by the second.
    const X: usize = 1 << 20;
    const C: usize = 3 << 20;
    const Z: usize = 2 << 20;
    const N: usize = 256;

    /// Title memory of our own and the copy of the title buffer it is.
    fn title(resource: usize) -> (&'static mut [u8], usize, std::sync::Arc<crate::shadow::Shadow>) {
        // SAFETY: memory of our own, never freed.
        let base = unsafe { VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE | MEM_COMMIT | WRITE_WATCH, PAGE_READWRITE) }.cast::<u8>();
        assert!(!base.is_null());
        crate::shadow::register(resource, base as u64, LEN as u64, false);
        let shadow = crate::shadow::find(base as u64, 64).expect("a copy");
        // SAFETY: `LEN` bytes just committed.
        (unsafe { std::slice::from_raw_parts_mut(base, LEN) }, base as usize, shadow)
    }

    /// A shader's stand-in: `bytes` go to offset `at` of the copy.
    fn write(host: &Host, list: &HostList, shadow: &crate::shadow::Shadow, at: usize, bytes: &[u8]) {
        let (upload, data) = host.upload_buffer(bytes.len() as u64).unwrap();
        // SAFETY: the mapping; an open list and live buffers.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
            list.list.CopyBufferRegion(&shadow.buffer, at as u64, &upload, 0, bytes.len() as u64);
            list.list
                .ResourceBarrier(&[transition(&shadow.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
        }
        list.keep(&upload);
    }

    /// Two parts: the first writes `[X, X + N)` of a buffer through a view of all of it; the second fills upload
    /// memory from the title's constants at `C`, in the same view, and writes them to `Z` with a shader of its own.
    /// With `narrow` the first declares what its shader writes by its code, else the whole view. Returns what title
    /// memory holds at `X` and `Z` afterwards, and whether the parts may be prepared together.
    fn parts(host: &Host, resource: usize, narrow: bool, together: bool) -> (Vec<u8>, Vec<u8>, bool) {
        let (title, base, shadow) = title(resource);
        title[C..C + N].fill(0xC5);
        let target = Target::Table { user_data: 4, dword: 0 };
        ENTRIES.with(|e| *e.borrow_mut() = vec![(target, Some(vec![(X as u64, (X + N) as u64)]))]);
        let a = host.new_list().unwrap();
        shadow.refresh(&a, base as u64, LEN as u64);
        if narrow {
            declare_as(&a, &shadow, base as u64, LEN as u64, Some(target), (false, true));
        } else {
            shadow.gpu_writes(&a, base as u64, LEN as u64);
        }
        write(host, &a, &shadow, X, &[0xA7; N]);
        a.close();
        let b = host.new_list().unwrap();
        shadow.refresh(&b, base as u64, LEN as u64);
        let (staging, staged) = b.upload(host, 4096).unwrap();
        b.defer_fill_buffer(&staging, staged, (base + C) as *const u8, N);
        // SAFETY: an open list and live buffers.
        unsafe {
            b.list.CopyBufferRegion(&shadow.buffer, Z as u64, &staging, 0, N as u64);
            b.list
                .ResourceBarrier(&[transition(&shadow.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
        }
        shadow.gpu_writes(&b, (base + Z) as u64, N as u64);
        b.close();
        ENTRIES.with(|e| e.borrow_mut().clear());
        let joins = joinable(std::iter::once(&a), &[&b]);
        if together {
            host.execute(&[&a, &b]);
        } else {
            host.execute(&[&a]);
            host.execute(&[&b]);
        }
        (title[X..X + N].to_vec(), title[Z..Z + N].to_vec(), joins)
    }

    /// The part that fills constants joins the one in front when that declares by its code what it writes and not
    /// when it declares the whole view, and prepared together the two give what they give prepared one after the
    /// other.
    #[test]
    fn parts_prepared_together_give_what_they_give_one_after_the_other() {
        let Some(host) = host() else {
            return;
        };
        let mut seen = Vec::new();
        for (n, (narrow, together)) in [(true, false), (true, true), (false, false), (false, true)].into_iter().enumerate() {
            let (x, z, joins) = parts(host, 0x7a30 + n, narrow, together);
            assert_eq!(joins, narrow, "narrow {narrow}");
            seen.push((x, z));
        }
        for (x, z) in &seen {
            assert_eq!(&x[..], &[0xA7; N][..], "the first part's result is in title memory");
            assert_eq!(&z[..], &[0xC5; N][..], "the second part's fill read the title's constants");
        }
    }

    /// A write that covers the place of a wait, and does not only begin there, meets it with the bytes of that
    /// place; a write that does not cover it, a wider wait than the write, a timestamp and a value that is not
    /// the one asked for do not.
    #[test]
    fn a_write_that_covers_the_place_of_a_wait_meets_it() {
        let Some(host) = host() else {
            return;
        };
        let at = 0x7000_0000usize;
        let list = host.new_list().unwrap();
        // Eight bytes: 5 in the low half, 7 in the high one.
        list.defer_write(at as *mut u8, crate::queue::Written::Value(7 << 32 | 5), 8);
        let wait = |address: usize, reference: u64, wide: bool| crate::list::ValueWait::test(address as u64, 3, reference, wide);
        assert!(covers(&list, &wait(at, 5, false)), "the low half, where it begins");
        assert!(covers(&list, &wait(at + 4, 7, false)), "the high half, inside it");
        assert!(covers(&list, &wait(at, 7 << 32 | 5, true)), "all of it");
        assert!(!covers(&list, &wait(at + 4, 5, false)), "not the value asked for");
        assert!(!covers(&list, &wait(at + 4, 7, true)), "wider than what is written from there");
        assert!(!covers(&list, &wait(at + 8, 0, false)), "past it");
        assert!(super::super::will_meet(&list, &wait(at, 5, false)));
        assert!(!super::super::will_meet(&list, &wait(at + 4, 7, false)), "only a write that begins there is seen without the switch");
        let stamps = host.new_list().unwrap();
        stamps.defer_write(at as *mut u8, crate::queue::Written::GpuTimestamp, 8);
        assert!(!covers(&stamps, &wait(at, 5, false)));
    }

    /// What is declared under `VICTORY_PRECISE_WRITES` is the ranges, rounded out; a view left whole where nothing
    /// bounds it; nothing for a view no store goes through; with looking, the whole view and the ranges beside it.
    #[test]
    fn the_declaration_is_the_ranges_where_there_are_some_and_the_view_where_there_are_not() {
        let Some(host) = host() else {
            return;
        };
        let (_, base, shadow) = title(0x7a40);
        let declared = |list: &HostList| -> Vec<(usize, usize)> {
            let mut out = Vec::new();
            for step in &list.deferred().copies {
                if let Step::GpuWrite(_, at, len) = step {
                    out.push((*at - base, *at - base + *len));
                }
            }
            out
        };
        let one = Some(Target::Root { slot: 1 });
        let list = |look: bool, narrow: bool, ranges: Found, target: Option<Target>| {
            ENTRIES.with(|e| *e.borrow_mut() = vec![(Target::Root { slot: 1 }, ranges)]);
            let list = host.new_list().unwrap();
            declare_as(&list, &shadow, base as u64, LEN as u64, target, (look, narrow));
            list
        };
        // Narrowing: rounded out to 64 bytes, the two ends clamped to the view.
        let l = list(false, true, Some(vec![(100, 200), (LEN as u64 - 8, LEN as u64 + 50)]), one);
        assert_eq!(declared(&l), vec![(64, 256), (LEN - 64, LEN)]);
        // Nothing stored through it: nothing declared.
        let l = list(false, true, Some(Vec::new()), one);
        assert_eq!(declared(&l), Vec::new());
        // Not bounded, or not a target of the stage: the whole view.
        let l = list(false, true, None, one);
        assert_eq!(declared(&l), vec![(0, LEN)]);
        let l = list(false, true, Some(vec![(0, 64)]), Some(Target::Root { slot: 2 }));
        assert_eq!(declared(&l), vec![(0, LEN)]);
        let l = list(false, true, Some(vec![(0, 64)]), None);
        assert_eq!(declared(&l), vec![(0, LEN)]);
        // Looking: the whole view is declared, and the ranges are kept beside it.
        let l = list(true, false, Some(vec![(100, 200)]), one);
        assert_eq!(declared(&l), vec![(0, LEN)]);
        {
            let d = l.deferred();
            assert!(d.precise.on);
            assert_eq!(d.precise.ranges, vec![(base + 64, base + 256)]);
        }
        ENTRIES.with(|e| e.borrow_mut().clear());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_side_keeps_each_range_once_and_says_it_was_kept() {
        let mut side = Side::default();
        assert!(!side.on);
        side.add(10, 10);
        assert!(!side.on, "an empty range is nothing");
        side.add(0x1000, 0x2000);
        side.add(0x1000, 0x2000);
        side.add(0x3000, 0x3040);
        assert!(side.on);
        assert_eq!(side.ranges, vec![(0x1000, 0x2000), (0x3000, 0x3040)]);
    }

    #[test]
    fn the_groups_of_a_dispatch_are_known_only_while_it_is_bound() {
        assert_eq!(GROUPS.get(), None);
        {
            let _extent = extent([4, 2, 1]);
            assert_eq!(GROUPS.get(), Some([4, 2, 1]));
        }
        assert_eq!(GROUPS.get(), None);
    }

    #[test]
    fn a_target_not_among_the_stage_entries_is_not_analysed() {
        let target = Target::Root { slot: 3 };
        assert!(found(Some(target)).is_none());
        assert!(found(None).is_none());
        ENTRIES.with(|e| e.borrow_mut().push((target, Some(vec![(0, 64)]))));
        assert_eq!(found(Some(target)), Some(Some(vec![(0, 64)])));
        assert!(found(Some(Target::Root { slot: 4 })).is_none());
        drop(Entered);
        assert!(found(Some(target)).is_none());
    }
}
