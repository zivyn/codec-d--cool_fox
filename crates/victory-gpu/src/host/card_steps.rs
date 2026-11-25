//! Steps of a submission that read title memory flights in the air may
//! still write, done on the card in the flights' order instead of on the
//! CPU after a wait for them (`VICTORY_CARD_STEPS`, see [`stage`]).

use std::sync::Arc;

use super::*;
use crate::shadow::Shadow;

/// The stage (`VICTORY_CARD_STEPS`; off by default): `1` constant buffers
/// of descriptor tables (`Step::Fill` with a slot of its own), `2` the
/// title's buffer copies (`Step::Copy`) too. Only with the relay's second
/// stage (`relay_exact`).
///
/// Why: a root view has no size, so a pass that writes through one into the
/// title's 174 MB heap is declared to write all that is left of it, and
/// some pass of every flight does. The title's ring of constants lies in
/// the same heap. One constant buffer of 480 bytes and one copy of 20 KiB a
/// frame, at a new place of the ring each time, waited for nearly
/// everything in the air: 25-30 ms of a 105 ms frame, the submitting thread
/// standing (run record-1; with `VICTORY_FILL_TITLE=1` in run relaycopy-1
/// the copy alone took over the wait).
///
/// What is done: a list of ours goes to the card in front of the
/// submission's lists, in the same call. It copies the bytes read from the
/// host copy that the flights write into a buffer of ours, and from there
/// to the destination's host copy (a copy), or the fill's descriptor slot
/// is pointed at that buffer (a fill; the slot is the fill's alone, the
/// root signatures say such descriptors are volatile, and the list that
/// names it has not gone to the card).
///
/// Why that is the value the CPU would have read after the wait. After the
/// wait title memory holds, for every byte read, what the title wrote, or
/// what the last of the flights that wrote it wrote (the landings put it
/// there, in order). The host copy on the card holds the same when our list
/// runs: every flight that may write one of these bytes writes this same
/// host copy ([`holder`]: a write through another host copy, a read-back
/// or a value written behind a flight there, and the old wait is taken),
/// they are all ahead of our list on the queue, and the title's own bytes
/// are brought up to date by a refresh step of our list, which is prepared
/// like any other list's (also under flights' declared writes:
/// `stale_under_flights`, which settles if it cannot). Nothing of this
/// submission writes these bytes earlier than our list: the steps are done
/// in front of all its lists in either case, and [`plan`] refuses when
/// another step of the submission writes what is read. Every draw that
/// reads a buffer view of the heap relies on the same refresh and the same
/// order; what is new is only that a constant buffer and a copy do too.
/// Where a page is not mapped the host copy may hold other bytes than the
/// CPU's copy would have left: refused.
///
/// For a copy's destination our list declares a shader write, exactly the
/// bytes copied, after a refresh of them: from there on it is what any
/// shader's write is. The relay compares, and puts what changed into title
/// memory before the title is told anything of this flight (its
/// bottom-of-pipe writes and fences come after, in order); CPU steps of
/// later submissions that read it find it declared and wait, or are done
/// here. Refused: another step of the submission reads or writes the
/// destination on the CPU, or a refresh brings another host copy of it up
/// to date ([`plan`], [`elsewhere`]). While such a copy is in the air a
/// submission that refreshes another host copy of its destination, or
/// reads it in a step that is not checked otherwise, waits for it
/// ([`copied_read`]); with the copy done on the CPU it was there at once.
///
/// What stage 2 does not see: code of ours that reads title memory away
/// from a submission's steps (the pass over textures picked by index, while
/// a list is recorded). With the CPU's copy such a read of the destination
/// found the bytes at once; now it finds them when the flight has landed.
/// The landing tells the guards (`crate::watch::landing`), so a texture
/// made from older bytes is made again. This is why stage 2 is its own.
pub(super) fn stage() -> u32 {
    static STAGE: OnceLock<u32> = OnceLock::new();
    *STAGE.get_or_init(|| match std::env::var("VICTORY_CARD_STEPS").ok().as_deref() {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    })
}

/// The bytes of our buffer between reading and writing: what one submission
/// may do this way.
const BETWEEN: u64 = 1 << 20;

/// Title memory something in the air writes: the host copy it goes through
/// (`None`: none, the bytes reach title memory when the flight lands), from,
/// to, and whether it is the destination of a copy done here.
pub(super) struct Writer {
    shadow: Option<Arc<Shadow>>,
    from: usize,
    to: usize,
    copy: bool,
}

/// What `items` of the line write.
pub(super) fn writers(items: &[Late]) -> Vec<Writer> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Late::Run(flight) => {
                for (shadow, ranges) in &flight.back {
                    out.extend(ranges.iter().map(|&(from, to)| Writer {
                        shadow: Some(shadow.clone()),
                        from,
                        to,
                        copy: false,
                    }));
                }
                for &i in &flight.order {
                    for &(_, at, [row, rows, pitch]) in &flight.lists[i].host().deferred().backs {
                        out.push(Writer {
                            shadow: None,
                            from: at,
                            to: at + rows.saturating_sub(1) * pitch + row,
                            copy: false,
                        });
                    }
                }
                out.extend(flight.own.iter().flat_map(Taken::copied));
            }
            Late::Write(at, _, bytes) => out.push(Writer {
                shadow: None,
                from: *at,
                to: *at + *bytes,
                copy: false,
            }),
            Late::Signal(..) => {}
        }
    }
    out
}

/// The destinations of copies done here that have not landed.
pub(super) fn copies(line: &Line) -> Vec<Writer> {
    if stage() < 2 {
        return Vec::new();
    }
    let flying = line.line.iter().filter_map(|l| match l {
        Late::Run(flight) => flight.own.as_ref(),
        _ => None,
    });
    let landing = line.landing.iter().filter(|w| w.copy).map(|w| Writer {
        shadow: w.shadow.clone(),
        ..*w
    });
    flying.flat_map(Taken::copied).chain(landing).collect()
}

/// Whether `runs` refresh another host copy of a destination in `copies`,
/// or read it in a step that flights are not waited for otherwise
/// (`Step::Host`): that reads title memory, where the copy is not yet.
pub(super) fn copied_read(copies: &[Writer], runs: &[&HostList]) -> bool {
    if copies.is_empty() {
        return false;
    }
    runs.iter().any(|list| {
        list.deferred().copies.iter().any(|step| match step {
            Step::Refresh(shadow, at, len) => copies
                .iter()
                .any(|w| w.from < *at + *len && *at < w.to && w.shadow.as_ref().is_none_or(|s| !Arc::ptr_eq(s, shadow))),
            Step::Host(_, (from, to)) => copies.iter().any(|w| w.from < *to && *from < w.to),
            _ => false,
        })
    })
}

/// What a step of a submission does to title memory, as the plan sees it.
/// `asked`: flights in the air stop it from being done on the CPU now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Does {
    Copy { dst: usize, src: usize, len: usize, asked: bool },
    Fill { src: usize, len: usize, asked: bool, slot: Option<(usize, u32)> },
    /// Another read on the CPU (a texture, vertices expanded).
    Read { from: usize, to: usize, asked: bool },
    /// A refresh of a host copy (named by its address).
    Refresh { shadow: usize, from: usize, to: usize },
    /// Anything, anywhere.
    Run,
}

fn meet(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// The steps of `does` (by place) to be done on the card: all that are
/// asked for, or why not. Our list does them all in front of the
/// submission's, every read before every write, so that is right only if no
/// step, asked for or not, writes what one of them reads or writes, and no
/// CPU step reads what one of them writes.
fn plan(does: &[Does], stage: u32) -> Result<Vec<usize>, &'static str> {
    let mut asked = Vec::new();
    for (i, d) in does.iter().enumerate() {
        match *d {
            Does::Run => return Err("a step of ours runs"),
            Does::Read { asked: true, .. } => return Err("a texture reads it"),
            Does::Fill { asked: true, slot: None, .. } => return Err("a fill without a descriptor slot of its own"),
            Does::Copy { asked: true, .. } if stage < 2 => return Err("a buffer copy (VICTORY_CARD_STEPS=2 does it on the card)"),
            Does::Copy { asked: true, .. } | Does::Fill { asked: true, .. } => asked.push(i),
            _ => {}
        }
    }
    for &i in &asked {
        let (src, dst) = match does[i] {
            Does::Copy { dst, src, len, .. } => ((src, src + len), Some((dst, dst + len))),
            Does::Fill { src, len, .. } => ((src, src + len), None),
            _ => continue,
        };
        for (k, d) in does.iter().enumerate() {
            let (read, written) = match *d {
                Does::Copy { dst, src, len, .. } => (Some((src, src + len)), Some((dst, dst + len))),
                Does::Fill { src, len, .. } => (Some((src, src + len)), None),
                Does::Read { from, to, .. } => (Some((from, to)), None),
                Does::Refresh { .. } | Does::Run => (None, None),
            };
            if written.is_some_and(|w| meet(w, src)) {
                return Err("a copy of the submission writes what it reads");
            }
            let Some(dst) = dst else {
                continue;
            };
            if k != i && written.is_some_and(|w| meet(w, dst)) {
                return Err("another copy of the submission writes what it writes");
            }
            if read.is_some_and(|r| meet(r, dst)) {
                return Err("a step of the submission reads what it writes");
            }
        }
    }
    Ok(asked)
}

/// Whether a refresh of `does` brings a host copy other than `shadow` up to
/// date in `[from, to)`.
fn elsewhere(does: &[Does], shadow: usize, from: usize, to: usize) -> bool {
    does.iter().any(|d| match *d {
        Does::Refresh { shadow: s, from: a, to: b } => s != shadow && meet((a, b), (from, to)),
        _ => false,
    })
}

/// The host copy that holds `[from, to)` on the card once what `writers`
/// write is there: the one they all write it through, or without any the
/// one shaders would be given.
fn holder(writers: &[Writer], from: usize, to: usize) -> Result<Arc<Shadow>, &'static str> {
    let mut found: Option<&Arc<Shadow>> = None;
    for w in writers.iter().filter(|w| meet((w.from, w.to), (from, to))) {
        match (&w.shadow, found) {
            (None, _) => return Err("a read-back or a value behind a flight is written there"),
            (Some(s), Some(f)) if !Arc::ptr_eq(s, f) => return Err("flights write it through two host copies"),
            (Some(s), _) => found = Some(s),
        }
    }
    let shadow = match found {
        Some(s) => s.clone(),
        None => crate::shadow::find(from as u64, (to - from) as u64).ok_or("no host copy of it")?,
    };
    let (start, len) = shadow.span();
    if shadow.in_place() || (from as u64) < start || to as u64 > start + len {
        return Err("no host copy holds all of it");
    }
    Ok(shadow)
}

/// Whether every page of `[from, to)` can be read (through the kernel:
/// asking Windows walks the page tables, milliseconds in the title's heap).
fn mapped(from: usize, to: usize) -> bool {
    let mut bytes = vec![0u8; to - from];
    read_into(from as *const u8, &mut bytes)
}

/// A step done on the card.
struct Op {
    from: (Arc<Shadow>, usize),
    len: usize,
    /// Its place in the buffer between.
    at: u64,
    to: To,
}

enum To {
    /// A host copy, and the title address.
    Copy(Arc<Shadow>, usize),
    /// A descriptor slot (title heap, slot).
    Slot(usize, u32),
}

/// Records what [`stage`] describes into `list` (open): `reads` are copied
/// into `between` (source, offset in it, offset in `between`, bytes), then
/// `writes` out of it (destination, offset in it, offset in `between`,
/// bytes). `between` is left readable as constants, the others in the
/// common state, as the submission's lists expect them.
fn record(
    list: &ID3D12GraphicsCommandList,
    between: &ID3D12Resource,
    reads: &[(&ID3D12Resource, u64, u64, u64)],
    writes: &[(&ID3D12Resource, u64, u64, u64)],
) {
    let common = D3D12_RESOURCE_STATE_COMMON;
    let (source, dest) = (D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
    let mut sources: Vec<&ID3D12Resource> = Vec::new();
    let mut dests: Vec<&ID3D12Resource> = Vec::new();
    for (all, list) in [(reads, &mut sources), (writes, &mut dests)] {
        for &(buffer, ..) in all {
            if !list.iter().any(|b| b.as_raw() == buffer.as_raw()) {
                list.push(buffer);
            }
        }
    }
    let both = |b: &ID3D12Resource| dests.iter().any(|d| d.as_raw() == b.as_raw());
    let mut barriers = vec![transition(between, dest, source | D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER)];
    barriers.extend(sources.iter().map(|b| transition(b, source, if both(b) { dest } else { common })));
    let done: Vec<D3D12_RESOURCE_BARRIER> = dests.iter().map(|b| transition(b, dest, common)).collect();
    // SAFETY: an open list; live buffers, and the callers' ranges lie in
    // them. The buffers start in the common state (the list is the first of
    // its call) and are promoted by their first use.
    unsafe {
        // What earlier lists' shaders wrote, first.
        list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(None),
                }),
            },
        }]);
        for &(buffer, offset, at, len) in reads {
            list.CopyBufferRegion(between, at, buffer, offset, len);
        }
        list.ResourceBarrier(&barriers);
        for &(buffer, offset, at, len) in writes {
            list.CopyBufferRegion(buffer, offset, between, at, len);
        }
        if !done.is_empty() {
            list.ResourceBarrier(&done);
        }
    }
}

/// The list in front of a submission's, and its buffer ([`BETWEEN`] bytes).
struct Own {
    list: HostList,
    between: ID3D12Resource,
}

// SAFETY: D3D12 objects are free-threaded, and the list is touched by one
// thread at a time (the submitting one until it is sent, then the one that
// lands it, then nobody until it is taken out of `POOL`).
unsafe impl Send for Own {}

/// Those that have landed: closed, their steps dropped.
static POOL: Mutex<Vec<Own>> = Mutex::new(Vec::new());

/// Since the last frame-rate line: fills and copies done on the card, their
/// bytes, and submissions that waited instead, refused by a check.
static DONE: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// What [`Host::card_steps`] took off a submission: the list that does it,
/// for the submission to run first, and the steps the CPU leaves out (their
/// list by address, their place in it).
pub(super) struct Taken {
    own: Own,
    steps: std::collections::HashSet<(usize, usize)>,
    copied: Vec<(Arc<Shadow>, usize, usize)>,
}

impl Taken {
    pub(super) fn list(&self) -> &HostList {
        &self.own.list
    }

    /// Whether step `i` of `list` is done by this.
    pub(super) fn takes(&self, list: &HostList, i: usize) -> bool {
        self.steps.contains(&(std::ptr::from_ref(list) as usize, i))
    }

    fn copied(&self) -> impl Iterator<Item = Writer> {
        self.copied.iter().map(|(shadow, from, to)| Writer {
            shadow: Some(shadow.clone()),
            from: *from,
            to: *to,
            copy: true,
        })
    }

    /// The submission has run and landed: the list and its buffer are free.
    pub(super) fn landed(self) {
        // The host copies its steps held go; its commands stay until the
        // next use resets it.
        drop(std::mem::take(&mut *self.own.list.deferred()));
        POOL.lock().unwrap_or_else(|e| e.into_inner()).push(self.own);
    }
}

impl Host {
    /// With flights in the air: if what keeps `runs` from being prepared
    /// now is reads of title memory that can all be done on the card
    /// ([`stage`]), the list that does them. `None` if nothing is in the
    /// way or the old wait is due. Under the submission lock.
    pub(super) fn card_steps(&self, runs: &[&HostList]) -> Option<Taken> {
        if stage() == 0 || !relay_exact() {
            return None;
        }
        let (written, copies) = {
            let line = line();
            if line.written.is_empty() {
                return None;
            }
            (line.written.clone(), copies(&line))
        };
        blocked_by(&written, runs, false)?;
        let taken = if copied_read(&copies, runs) {
            Err("a copy done on the card is read through another host copy")
        } else {
            self.take_steps(&written, runs)
        };
        match taken {
            Ok(taken) => taken,
            Err(why) => {
                DONE[3].fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                refused(why);
                let _ = why;
                None
            }
        }
    }

    fn take_steps(&self, written: &[(usize, usize)], runs: &[&HostList]) -> Result<Option<Taken>, &'static str> {
        let mut does = Vec::new();
        // Which step each is. One list's lock at a time: a submission may
        // name a list twice.
        let mut steps = Vec::new();
        for list in runs {
            for (i, step) in list.deferred().copies.iter().enumerate() {
                does.push(match step {
                    &Step::Copy(dst, src, len) => Does::Copy {
                        dst,
                        src,
                        len,
                        asked: copy_blocked(written, dst, src, len).is_some(),
                    },
                    &Step::Fill(_, src, len, slot) => Does::Fill {
                        src,
                        len,
                        asked: read_blocked(written, src, len),
                        slot,
                    },
                    Step::Texture(staging) => {
                        let (at, len) = staging.memory();
                        Does::Read {
                            from: at,
                            to: at + len,
                            asked: touches(written, at, at + len),
                        }
                    }
                    &Step::Host(_, (from, to)) if from < to => Does::Read { from, to, asked: false },
                    Step::Refresh(shadow, at, len) => Does::Refresh {
                        shadow: Arc::as_ptr(shadow) as usize,
                        from: *at,
                        to: *at + *len,
                    },
                    Step::Run(_) => Does::Run,
                    Step::GpuWrite(..) | Step::Host(..) => continue,
                });
                steps.push((std::ptr::from_ref::<HostList>(list) as usize, i));
            }
        }
        let chosen = plan(&does, stage())?;
        if chosen.is_empty() {
            return Ok(None);
        }
        let writers = {
            let line = line();
            let mut all = writers(&line.line);
            all.extend(line.landing.iter().map(|w| Writer {
                shadow: w.shadow.clone(),
                ..*w
            }));
            all
        };
        let mut ops: Vec<Op> = Vec::with_capacity(chosen.len());
        let mut at = 0u64;
        for &i in &chosen {
            let (src, len) = match does[i] {
                Does::Copy { src, len, .. } | Does::Fill { src, len, .. } => (src, len),
                _ => continue,
            };
            if len == 0 {
                return Err("nothing to read");
            }
            if at + len as u64 > BETWEEN {
                return Err("too much at once");
            }
            let from = holder(&writers, src, src + len)?;
            if !mapped(src, src + len) {
                return Err("not all of what it reads is mapped");
            }
            let to = match does[i] {
                Does::Copy { dst, .. } => {
                    let to = holder(&writers, dst, dst + len)?;
                    if elsewhere(&does, Arc::as_ptr(&to) as usize, dst, dst + len) {
                        return Err("another host copy of what it writes is read");
                    }
                    if !mapped(dst, dst + len) {
                        return Err("not all of what it writes is mapped");
                    }
                    To::Copy(to, dst)
                }
                Does::Fill { slot: Some((heap, index)), .. } => To::Slot(heap, index),
                _ => continue,
            };
            ops.push(Op {
                from: (from, src),
                len,
                at,
                to,
            });
            at = (at + len as u64).next_multiple_of(256);
        }
        let Some(own) = self.own_list() else {
            return Err("no host list or buffer for it");
        };
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        let mut copied = Vec::new();
        // First every refresh, then the write: the bytes compared with are
        // the title's.
        for op in &ops {
            op.from.0.refresh(&own.list, op.from.1 as u64, op.len as u64);
            reads.push((&op.from.0.buffer, op.from.0.offset(op.from.1 as u64), op.at, op.len as u64));
            if let To::Copy(to, dst) = &op.to {
                to.refresh(&own.list, *dst as u64, op.len as u64);
                writes.push((&to.buffer, to.offset(*dst as u64), op.at, op.len as u64));
            }
        }
        for op in &ops {
            if let To::Copy(to, dst) = &op.to {
                to.gpu_writes(&own.list, *dst as u64, op.len as u64);
                copied.push((to.clone(), *dst, *dst + op.len));
            }
        }
        #[cfg(feature = "diagnostics")]
        own.list.note_op(|| format!("{} copies of title memory done on the card, {at} bytes (VICTORY_CARD_STEPS)", ops.len()));
        record(&own.list.list, &own.between, &reads, &writes);
        own.list.close();
        // SAFETY: a live buffer.
        let gpu = unsafe { own.between.GetGPUVirtualAddress() };
        for op in &ops {
            DONE[2].fetch_add(op.len as u64, Ordering::Relaxed);
            match &op.to {
                To::Slot(heap, index) => {
                    DONE[0].fetch_add(1, Ordering::Relaxed);
                    // (A heap that is gone: nothing can read the slot.)
                    crate::descriptor::constant_slot(*heap, *index, gpu + op.at, op.len as u32);
                }
                To::Copy(..) => {
                    DONE[1].fetch_add(1, Ordering::Relaxed);
                }
            }
            #[cfg(feature = "diagnostics")]
            noted(op);
        }
        Ok(Some(Taken {
            own,
            steps: chosen.iter().map(|&i| steps[i]).collect(),
            copied,
        }))
    }

    /// A list of ours open for recording, and its buffer.
    fn own_list(&self) -> Option<Own> {
        let pooled = POOL.lock().unwrap_or_else(|e| e.into_inner()).pop();
        if let Some(own) = pooled {
            own.list.reset();
            return Some(own);
        }
        let between = self.uav_buffer(BETWEEN)?;
        name(&between, || "copies of title memory on the card".into());
        let list = self.new_list().ok()?;
        Some(Own { list, between })
    }
}

/// Diagnostics: the copies and fills done on the card since the last
/// frame-rate line, by bytes, host copy read and host copy written (start
/// and length; zeros for a fill): how many, and the last title addresses.
#[cfg(feature = "diagnostics")]
type Noted = std::collections::HashMap<(usize, (u64, u64), (u64, u64)), (u64, usize, usize)>;
#[cfg(feature = "diagnostics")]
static NOTED: Mutex<Option<Noted>> = Mutex::new(None);

#[cfg(feature = "diagnostics")]
fn noted(op: &Op) {
    let (to, dst) = match &op.to {
        To::Copy(to, dst) => (to.span(), *dst),
        To::Slot(..) => ((0, 0), 0),
    };
    let mut noted = NOTED.lock().unwrap_or_else(|e| e.into_inner());
    let e = noted.get_or_insert_with(Default::default).entry((op.len, op.from.0.span(), to)).or_default();
    *e = (e.0 + 1, op.from.1, dst);
}

/// For the frame-rate line.
pub(super) fn log(frames: f64) {
    if stage() == 0 {
        return;
    }
    let d: Vec<f64> = DONE.iter().map(|d| d.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "steps done on the card instead of waiting for flights, per frame (VICTORY_CARD_STEPS={}): {:.2} fills, {:.2} buffer \
         copies, {:.1} KB; {:.2} submissions refused and waited",
        stage(),
        d[0],
        d[1],
        d[2] / 1024.0,
        d[3],
    );
    #[cfg(feature = "diagnostics")]
    {
        let noted = NOTED.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
        let mut noted: Vec<_> = noted.into_iter().collect();
        noted.sort_unstable_by_key(|(_, e)| std::cmp::Reverse(e.0));
        for ((len, from, to), (n, src, dst)) in noted.into_iter().take(5) {
            let n = n as f64 / frames;
            if to.1 == 0 {
                tracing::info!("done on the card, per frame: {n:.2} fills of {len:#x} bytes from the host copy of {:#x}+{:#x} (last {src:#x})", from.0, from.1);
            } else {
                tracing::info!(
                    "done on the card, per frame: {n:.2} copies of {len:#x} bytes from the host copy of {:#x}+{:#x} (last {src:#x}) \
                     to that of {:#x}+{:#x} (last {dst:#x})",
                    from.0,
                    from.1,
                    to.0,
                    to.1
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(dst: usize, src: usize, len: usize, asked: bool) -> Does {
        Does::Copy { dst, src, len, asked }
    }

    fn fill(src: usize, len: usize, asked: bool) -> Does {
        Does::Fill { src, len, asked, slot: Some((1, 2)) }
    }

    /// Only what is asked for is taken, all of it, and a step that stands
    /// in none of their ways changes nothing.
    #[test]
    fn steps_apart_from_one_another_are_taken() {
        let does = [
            copy(1000, 5000, 100, false),
            fill(2000, 64, true),
            Does::Refresh { shadow: 1, from: 0, to: 9000 },
            copy(3000, 6000, 100, true),
            fill(1000, 64, false),
            Does::Read { from: 3100, to: 4000, asked: false },
        ];
        assert_eq!(plan(&does, 2), Ok(vec![1, 3]));
        // Two fills of the same bytes.
        assert_eq!(plan(&[fill(0, 64, true), fill(32, 64, true)], 1), Ok(vec![0, 1]));
        assert_eq!(plan(&[fill(0, 64, false)], 1), Ok(vec![]));
    }

    /// Everything that could make the card's order another than the CPU's
    /// refuses the lot, wherever in the submission it stands.
    #[test]
    fn steps_in_one_anothers_way_are_refused() {
        let taken = copy(1000, 5000, 100, true);
        let refused = |other: Does| {
            assert!(plan(&[taken, other], 2).is_err(), "{other:?} after");
            assert!(plan(&[other, taken], 2).is_err(), "{other:?} before");
        };
        for asked in [false, true] {
            // Writes what it reads.
            refused(copy(5099, 9000, 1, asked));
            // Writes what it writes.
            refused(copy(999, 9000, 2, asked));
            // Reads what it writes.
            refused(copy(7000, 1050, 10, asked));
            refused(fill(1099, 64, asked));
            refused(Does::Read { from: 900, to: 1001, asked });
        }
        refused(Does::Run);
        // A texture that itself waits for flights; a fill with no slot.
        refused(Does::Read { from: 1, to: 2, asked: true });
        refused(Does::Fill { src: 1, len: 1, asked: true, slot: None });
        // Into itself, and the same list twice.
        assert!(plan(&[copy(1000, 1050, 100, true)], 2).is_err());
        assert!(plan(&[taken, taken], 2).is_err());
        // A fill of what a copy writes, and copies without stage 2.
        assert!(plan(&[fill(1000, 4, true), copy(1000, 5000, 100, false)], 2).is_err());
        assert!(plan(&[taken], 1).is_err());
        // Ends that touch are apart.
        assert!(plan(&[taken, copy(900, 9000, 100, false), copy(1100, 9000, 5, false), copy(4900, 9000, 100, false)], 2).is_ok());
    }

    #[test]
    fn a_refresh_of_another_host_copy_is_seen() {
        let does = [Does::Refresh { shadow: 7, from: 100, to: 200 }];
        assert!(!elsewhere(&does, 7, 100, 200));
        assert!(elsewhere(&does, 8, 199, 300));
        assert!(!elsewhere(&does, 8, 200, 300));
    }

    /// A value written behind a flight, or a read-back, is in no host copy
    /// before it lands.
    #[test]
    fn what_no_host_copy_holds_is_refused() {
        let writers = [Writer { shadow: None, from: 100, to: 108, copy: false }];
        assert!(holder(&writers, 104, 120).is_err());
    }

    /// Reads are all before writes, between two buffers and within one, and
    /// the buffers are usable in the lists that follow in the same call.
    #[test]
    fn the_card_copies_what_is_recorded() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let (a, a_data) = host.shared_buffer(1 << 16).unwrap();
        let (b, b_data) = host.shared_buffer(1 << 16).unwrap();
        // SAFETY: mappings of 64 KiB.
        let (a_bytes, b_bytes) =
            unsafe { (std::slice::from_raw_parts_mut(a_data, 1 << 16), std::slice::from_raw_parts_mut(b_data, 1 << 16)) };
        for (i, (x, y)) in a_bytes.iter_mut().zip(b_bytes.iter_mut()).enumerate() {
            (*x, *y) = (i as u8 ^ (i >> 8) as u8, 0xee);
        }
        let was = a_bytes.to_vec();
        let own = host.own_list().unwrap();
        // Within `a` (there and back), from `a` to `b`, and a read alone.
        record(
            &own.list.list,
            &own.between,
            &[(&a, 33, 0, 100), (&a, 2000, 256, 100), (&a, 5001, 512, 7), (&a, 9, 1024, 4)],
            &[(&a, 2000, 0, 100), (&a, 33, 256, 100), (&b, 77, 512, 7)],
        );
        own.list.close();
        // A list behind it that uses the buffers in other ways.
        let next = host.new_list().unwrap();
        let (out, out_data) = host.shared_buffer(1 << 16).unwrap();
        // SAFETY: an open list; live buffers.
        unsafe {
            next.list.CopyBufferRegion(&out, 0, &own.between, 1024, 4);
            next.list.CopyBufferRegion(&b, 4096, &a, 33, 100);
        }
        next.close();
        host.execute(&[&own.list, &next]);
        // With the debug layer (`VICTORY_D3D12_DEBUG=1`, diagnostics).
        assert_eq!(host.log_messages(), None);
        assert_eq!(a_bytes[2000..2100], was[33..133]);
        assert_eq!(a_bytes[33..133], was[2000..2100]);
        assert_eq!(b_bytes[77..84], was[5001..5008]);
        assert_eq!(b_bytes[4096..4196], was[2000..2100]);
        // SAFETY: a mapping of 64 KiB.
        assert_eq!(unsafe { std::slice::from_raw_parts(out_data, 4) }, &was[9..13]);
        assert!(b_bytes[..77].iter().all(|&x| x == 0xee));
        a_bytes[2000..2100].copy_from_slice(&was[2000..2100]);
        a_bytes[33..133].copy_from_slice(&was[33..133]);
        assert_eq!(a_bytes, &was[..]);
        // As the landing does: its next use records anew.
        POOL.lock().unwrap().push(own);
        let own = host.own_list().unwrap();
        own.list.close();
    }

    /// The whole way, in a process of its own with the relay on:
    /// `VICTORY_RELAY=2 VICTORY_CARD_STEPS=2 cargo test -p victory-gpu --lib
    /// card_steps -- --ignored --test-threads=1`. A flight's list writes
    /// part of what the next submission's copy reads, the title wrote the
    /// rest; the copy is done on the card behind the flight, and a second
    /// one reads what the first wrote. Title memory ends as on the CPU.
    #[test]
    #[ignore = "wants VICTORY_RELAY=2 VICTORY_CARD_STEPS=2 and a process of its own"]
    fn a_copy_behind_a_flight_reads_what_the_flight_wrote() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        const LEN: usize = 4 << 20;
        const WRITE_WATCH: u32 = 0x20_0000;
        let Some(host) = host() else {
            return;
        };
        assert!(relay_exact() && stage() == 2, "set VICTORY_RELAY=2 VICTORY_CARD_STEPS=2");
        // Two title buffers: in the first a copy within it, from it into the
        // second, and back.
        let (title, start) = {
            // SAFETY: memory of our own, never freed.
            let base = unsafe { VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE | MEM_COMMIT | WRITE_WATCH, PAGE_READWRITE) }.cast::<u8>();
            assert!(!base.is_null());
            // SAFETY: `LEN` bytes just committed.
            (unsafe { std::slice::from_raw_parts_mut(base, LEN) }, base as usize)
        };
        const HALF: usize = LEN / 2;
        crate::shadow::register(0x1111, start as u64, HALF as u64);
        crate::shadow::register(0x2222, (start + HALF) as u64, HALF as u64);
        let first = crate::shadow::find(start as u64, 64).unwrap();
        let (slow_from, slow_to) = (host.uav_buffer(64 << 20).unwrap(), host.uav_buffer(64 << 20).unwrap());
        let before = DONE[1].load(Ordering::Relaxed);
        let mut lists = Vec::new();
        for round in 0..8usize {
            // The title writes; then a "shader" of a flight writes 300 bytes
            // of what the copy reads, behind some work.
            let seed = |i: usize| (i * 7 + round * 13 + i / 251) as u8;
            for (i, b) in title[4000..6000].iter_mut().enumerate() {
                *b = seed(i);
            }
            let (upload, data) = host.upload_buffer(4096).unwrap();
            for i in 0..300 {
                // SAFETY: inside the mapping.
                unsafe { data.add(i).write(0x80 | (round * 31 + i) as u8) };
            }
            let a = host.new_list().unwrap();
            first.refresh(&a, start as u64, HALF as u64);
            first.gpu_writes(&a, start as u64, HALF as u64);
            // SAFETY: an open list; live buffers.
            unsafe {
                for _ in 0..24 {
                    a.list.CopyBufferRegion(&slow_to, 0, &slow_from, 0, 64 << 20);
                }
                a.list.CopyBufferRegion(&first.buffer, 4500, &upload, 0, 300);
                a.list
                    .ResourceBarrier(&[transition(&first.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
            }
            a.close();
            host.execute_own(&a);
            // With the flight in the air, beside what it writes.
            for (i, b) in title[5300..5400].iter_mut().enumerate() {
                *b = seed(1300 + i) ^ 0x5c;
            }
            // Into the second buffer, and within the first.
            let b = host.new_list().unwrap();
            b.defer_copy((start + HALF + 100) as *mut u8, (start + 4001) as *const u8, 1500);
            b.defer_copy((start + 900_001) as *mut u8, (start + 4400) as *const u8, 777);
            b.close();
            host.execute_own(&b);
            // What the first wrote, read by the next submission.
            let c = host.new_list().unwrap();
            c.defer_copy((start + 1_200_000) as *mut u8, (start + HALF + 100) as *const u8, 1500);
            c.close();
            let number = host.execute_own(&c);
            settle_now(WHY_SYNC);
            assert!(FINISHED.load(Ordering::Acquire) >= number);
            let wanted: Vec<u8> = (0..2000)
                .map(|i| match i {
                    500..800 => 0x80 | (round * 31 + i - 500) as u8,
                    1300..1400 => seed(i) ^ 0x5c,
                    _ => seed(i),
                })
                .collect();
            assert!(title[4000..6000] == wanted[..], "what the flight wrote, round {round}");
            assert!(title[HALF + 100..HALF + 1600] == wanted[1..1501], "the copy to the other buffer, round {round}");
            assert!(title[900_001..900_778] == wanted[400..1177], "the copy within the buffer, round {round}");
            assert!(title[1_200_000..1_201_500] == wanted[1..1501], "the copy of the copy, round {round}");
            assert!(title[HALF..HALF + 100].iter().all(|&x| x == 0));
            assert!(title[HALF + 1600..].iter().all(|&x| x == 0));
            a.keep(&upload);
            lists.extend([a, b, c]);
        }
        assert_eq!(host.log_messages(), None);
        let done = DONE[1].load(Ordering::Relaxed) - before;
        eprintln!("{done} of 24 copies were done on the card, {} submissions waited", DONE[3].load(Ordering::Relaxed));
        assert!(done >= 12, "only {done} copies were done on the card");
    }
}
