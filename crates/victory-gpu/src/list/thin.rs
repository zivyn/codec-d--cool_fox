//! List calls that only write down their arguments (`VICTORY_THIN=1`); what
//! they mean is worked out by threads of ours, beside the title's.
//!
//! Why: the title makes 45,000 to 60,000 list calls a frame. Each was
//! translated inside the call, 3.5 us of it, 160 to 190 ms of processor time a
//! frame on the title's five recording threads; its own code between the calls
//! is a tenth of that. Nothing else of the title's frame could go on
//! meanwhile: recording took 55 of 65 ms, and behind it the card stood idle
//! for half the frame (timeline waits-3). On the console such a call writes a
//! few words into the list's memory and the card reads them when the list
//! runs. So it is here now: a call appends its arguments (and copies of what
//! they point at) to a stream of words; threads of ours run the stream, call
//! by call and in order, through the list methods there were before, on a list
//! of ours ([`Recording::back`]) that the title never sees. They begin while
//! the title still records (every [`CHUNK`] words are handed over), several
//! lists at a time. A submission goes into the line of submissions
//! (`crate::queue::behind`) as it was made, by the recordings' names; the
//! thread that serves the line goes on with it when they are translated.
//!
//! What may be looked at later than before, and why that is all right: all
//! that the card of the console looks at when the list runs (descriptor heaps,
//! constants, upload memory, indirect arguments): it must stay as it is until
//! the list has run, and no fence behind the list is set before. What D3D12
//! reads at the call is copied at the call: the arrays and structures passed,
//! and the render target and depth views, whose slots may be written again
//! right after. Objects named (pipelines, resources) must live until the list
//! has run; a recording the title resets or releases without submitting it is
//! not run any further.

use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use super::replay;
use crate::descriptor::RtvRecord;
use crate::draw::{IndexView, VertexView};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT;

/// Words after which a stream is handed to the translating threads.
const CHUNK: usize = 1024;

/// Whether list calls are only written down (`VICTORY_THIN=1`; needs the line
/// of submissions, which it turns on).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        wanted() && crate::host::host().is_some() && crate::queue::line_on() && {
            start();
            true
        }
    })
}

/// Whether it is asked for.
pub(crate) fn wanted() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::defaults::var_os("VICTORY_THIN").is_some_and(|v| v == "1"))
}

/// What a title list keeps: the recording being written and the words of it
/// not handed over yet.
#[derive(Default)]
pub(super) struct Front {
    recording: Option<Arc<Recording>>,
    words: Vec<u64>,
    closed: bool,
    submitted: bool,
    /// What the list is, for pricing its reuse (`VICTORY_REPLAY_COUNT`).
    priced: replay::Hashes,
}

impl Drop for Front {
    fn drop(&mut self) {
        self.leave();
    }
}

impl Front {
    /// Lets go of the recording there is: one never submitted is not run any
    /// further.
    fn leave(&mut self) {
        if let Some(recording) = self.recording.take()
            && !self.submitted
        {
            recording.abandoned.store(true, Release);
        }
        self.words.clear();
    }

    /// Begins a recording, with `pipeline` bound.
    pub(super) fn begin(&mut self, kind: u32, pipeline: This) {
        self.leave();
        self.recording = Some(Arc::new(Recording::new(kind)));
        self.closed = false;
        self.submitted = false;
        self.priced.clear();
        self.put(RESET, &[pipeline as u64]);
    }

    #[inline]
    fn put(&mut self, op: u64, args: &[u64]) {
        if self.closed || self.recording.is_none() {
            return;
        }
        if replay::on() {
            self.priced.add(op, args, is_address(op), op == UPLOAD, is_draw(op));
        }
        self.words.push(op | (args.len() as u64) << 16);
        self.words.extend_from_slice(args);
        if self.words.len() >= CHUNK {
            self.hand_over();
        }
    }

    /// [`Self::put`] of some words and then the bytes of some things.
    fn put_with(&mut self, op: u64, args: &[u64], blobs: &[&[u8]]) {
        if self.closed || self.recording.is_none() {
            return;
        }
        let at = self.words.len();
        self.words.push(0);
        self.words.extend_from_slice(args);
        for blob in blobs {
            let from = self.words.len();
            self.words.resize(from + blob.len().div_ceil(8), 0);
            // SAFETY: the words just made hold the bytes; plain data.
            unsafe {
                std::ptr::copy_nonoverlapping(blob.as_ptr(), self.words.as_mut_ptr().add(from).cast(), blob.len())
            };
        }
        let len = (self.words.len() - at - 1) as u64;
        self.words[at] = op | len << 16;
        if replay::on() {
            self.priced.add(op, &self.words[at + 1..], false, false, false);
        }
        if self.words.len() >= CHUNK {
            self.hand_over();
        }
    }

    fn hand_over(&mut self) {
        let Some(recording) = &self.recording else { return };
        if self.words.is_empty() {
            return;
        }
        let fresh = held(&WORDS).pop().unwrap_or_else(|| Vec::with_capacity(CHUNK + 256));
        let words = std::mem::replace(&mut self.words, fresh);
        CHUNKS.fetch_add(1, Relaxed);
        let schedule = {
            let mut inner = held(&recording.inner);
            inner.chunks.push_back(words);
            !std::mem::replace(&mut inner.scheduled, true)
        };
        if schedule {
            held(&READY).push_back(recording.clone());
            WORK.notify_one();
        }
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.put(CLOSE, &[]);
        self.closed = true;
        if replay::on() {
            replay::done(&mut self.priced);
        }
        self.hand_over();
    }
}

/// Whether a call names a place that is another one in each list (a root view's address).
fn is_address(op: u64) -> bool {
    op == Plain::GRAPHICS_VIEW as u64 || op == Plain::COMPUTE_VIEW as u64
}

/// Whether a call draws or dispatches.
fn is_draw(op: u64) -> bool {
    op == Plain::DRAW as u64
        || op == Plain::DRAW_INDEXED as u64
        || op == Plain::DISPATCH as u64
        || op == Plain::DISPATCH_X as u64
        || op == Plain::DISPATCH_MESH as u64
        || op == Plain::DISPATCH_MESH_X as u64
        || op == Plain::EXECUTE_INDIRECT as u64
}

/// Streams' memory to use again.
static WORDS: Mutex<Vec<Vec<u64>>> = Mutex::new(Vec::new());
/// Recordings with words to translate.
static READY: Mutex<VecDeque<Arc<Recording>>> = Mutex::new(VecDeque::new());
static WORK: Condvar = Condvar::new();
/// Lists of ours nobody uses, closed.
static BACKS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Since the last line of the log: streams handed over; calls translated and
/// the microseconds that took (all threads), recordings translated, and
/// recordings let go of in the middle.
static CHUNKS: AtomicU64 = AtomicU64::new(0);
static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

fn held<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One recording of a title list: what was written of it and not translated
/// yet, and the list of ours it goes to.
pub(crate) struct Recording {
    kind: u32,
    inner: Mutex<Inner>,
    /// Translated to its end.
    done: AtomicBool,
    moved: Condvar,
    abandoned: AtomicBool,
}

#[derive(Default)]
struct Inner {
    chunks: VecDeque<Vec<u64>>,
    /// Streams translated (commands may name places in them until the end).
    kept: Vec<Vec<u64>>,
    /// It is among [`READY`] or being translated.
    scheduled: bool,
    /// The list of ours (0: none yet).
    back: usize,
    /// That list was closed by the stream.
    closed: bool,
}

impl Recording {
    fn new(kind: u32) -> Self {
        Recording {
            kind,
            inner: Mutex::default(),
            done: AtomicBool::new(false),
            moved: Condvar::new(),
            abandoned: AtomicBool::new(false),
        }
    }

    pub(crate) fn done(&self) -> bool {
        self.done.load(Acquire)
    }

    /// Waits until it is translated.
    pub(crate) fn wait(&self) {
        let mut inner = held(&self.inner);
        while !self.done() {
            inner = self.moved.wait(inner).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// The list of ours it went to (null: it was never begun).
    pub(crate) fn back(&self) -> This {
        held(&self.inner).back as This
    }

    /// Translates what there is.
    fn run(&self) {
        loop {
            let (chunk, back) = {
                let mut inner = held(&self.inner);
                let Some(chunk) = inner.chunks.pop_front() else {
                    inner.scheduled = false;
                    return;
                };
                if inner.back == 0 {
                    inner.back = back_list(self.kind) as usize;
                }
                (chunk, inner.back as This)
            };
            let began = std::time::Instant::now();
            let (calls, closed) = translate(back, &chunk, || self.abandoned.load(Acquire));
            if closed {
                // (Before it is said to be done: from then on the submitting thread may take its host side.)
                super::summarize(back);
            }
            STATS[0].fetch_add(calls, Relaxed);
            STATS[1].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
            let mut inner = held(&self.inner);
            inner.kept.push(chunk);
            if closed {
                inner.closed = true;
                let kept = std::mem::take(&mut inner.kept);
                self.done.store(true, Release);
                self.moved.notify_all();
                drop(inner);
                give_back(kept);
                STATS[2].fetch_add(1, Relaxed);
                crate::queue::line_moved();
            }
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        give_back(inner.chunks.drain(..).chain(std::mem::take(&mut inner.kept)).collect());
        if inner.back != 0 {
            if !inner.closed {
                // Left in the middle.
                STATS[3].fetch_add(1, Relaxed);
                super::close(inner.back as This);
            }
            held(&BACKS).push(inner.back);
        }
    }
}

fn give_back(streams: Vec<Vec<u64>>) {
    let mut words = held(&WORDS);
    for mut s in streams {
        if words.len() < 1024 && s.capacity() <= 16 * CHUNK {
            s.clear();
            words.push(s);
        }
    }
}

/// A list of ours, closed.
fn back_list(kind: u32) -> This {
    if let Some(back) = held(&BACKS).pop() {
        // SAFETY: a list of ours nobody uses.
        unsafe { (*(back as *mut List)).kind = kind };
        return back as This;
    }
    let list: This = Box::into_raw(super::boxed(kind, std::ptr::null(), false)).cast_const().cast();
    super::close(list);
    list
}

fn start() {
    let threads = std::env::var("VICTORY_THIN_THREADS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(8usize).clamp(1, 32);
    for i in 0..threads {
        let worker = move || {
            crate::cores::keep_many_on_fast_cores("victory-list");
            crate::cores::prioritize("victory-list", crate::cores::Role::Translator);
            crate::queue::no_drain_here();
            loop {
                let recording = {
                    let mut ready = held(&READY);
                    loop {
                        match ready.pop_front() {
                            Some(r) => break r,
                            None => ready = WORK.wait(ready).unwrap_or_else(|e| e.into_inner()),
                        }
                    }
                };
                crate::sched::list_sync();
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| recording.run())).is_err() {
                    crate::host::fail(format_args!("a thread that translates lists panicked"));
                }
            }
        };
        if let Err(e) = std::thread::Builder::new().name(format!("victory-list-{i}")).spawn(worker) {
            tracing::warn!(error = %e, "a thread less to translate lists");
        }
    }
    tracing::info!(threads, "list calls are written down and translated by threads of ours (VICTORY_THIN=1)");
}

/// The recordings of submitted lists, in order (one that was not begun is left
/// out).
pub(crate) fn submitted(lists: &[This]) -> Arc<[Arc<Recording>]> {
    lists
        .iter()
        .filter(|l| !l.is_null())
        .filter_map(|&l| {
            // SAFETY: the title passes its lists, which nobody records into now.
            let front = unsafe { &mut (*l.cast_mut().cast::<List>()).thin };
            front.close();
            front.submitted = true;
            front.recording.clone()
        })
        .collect()
}

/// For the frame-rate line.
pub(crate) fn line(frames: f64) {
    if !wanted() || !on() {
        return;
    }
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Relaxed) as f64 / frames).collect();
    tracing::info!(
        "list calls translated by threads of ours per frame (VICTORY_THIN): {:.0} in {:.1} ms (all threads), {:.1} lists, {:.1} pieces; left in the middle {:.2}",
        s[0],
        s[1] / 1000.0,
        s[2],
        CHUNKS.swap(0, Relaxed) as f64 / frames,
        s[3]
    );
    crate::descriptor::host::view_cache_line(frames);
    super::replay::line(frames);
    crate::draw::state_filter_line(frames);
    crate::shadow::world_line(frames);
}

#[inline]
fn front<'a>(this: This) -> &'a mut Front {
    // SAFETY: the title passes one of our lists; a list is recorded by one
    // thread at a time.
    unsafe { &mut (*this.cast_mut().cast::<List>()).thin }
}

/// An argument as a word.
trait Word: Copy {
    fn word(self) -> u64;
    fn unword(w: u64) -> Self;
}
macro_rules! words {
    ($($t:ty),*) => { $(impl Word for $t {
        #[inline]
        fn word(self) -> u64 { self as u64 }
        #[inline]
        fn unword(w: u64) -> Self { w as $t }
    })* };
}
words!(u8, u32, i32, u64, This);
impl Word for f32 {
    #[inline]
    fn word(self) -> u64 {
        u64::from(self.to_bits())
    }
    #[inline]
    fn unword(w: u64) -> Self {
        f32::from_bits(w as u32)
    }
}

/// The methods whose arguments are all plain values: the one the title calls,
/// and the call made of it later.
macro_rules! plain {
    ($($op:ident $name:ident = $was:path, ($($a:ident: $t:ty),*);)*) => {
        #[allow(clippy::upper_case_acronyms, non_camel_case_types)]
        #[repr(u64)]
        enum Plain { $($op),* , END }
        $(extern "system" fn $name(this: This $(, $a: $t)*) {
            front(this).put(Plain::$op as u64, &[$(Word::word($a)),*]);
        })*
        /// Whether `op` was one of them; done on `list`.
        #[inline]
        fn run_plain(op: u64, list: This, w: &[u64]) -> bool {
            match op {
                $(n if n == Plain::$op as u64 => {
                    let mut words = w.iter();
                    $(let $a: $t = Word::unword(words.next().copied().unwrap_or(0));)*
                    let _ = words;
                    $was(list $(, $a)*);
                })*
                _ => return false,
            }
            true
        }
    };
}

plain! {
    TOPOLOGY topology = state::topology, (v: u32);
    STENCIL_REF stencil_ref = state::stencil_ref, (v: u32);
    DEPTH_BOUNDS depth_bounds = state::depth_bounds, (min: f32, max: f32);
    COMPUTE_TABLE compute_table = state::compute_table, (slot: u32, handle: u64);
    GRAPHICS_TABLE graphics_table = state::graphics_table, (slot: u32, handle: u64);
    COMPUTE_CONSTANT compute_constant = state::compute_constant, (slot: u32, value: u32, offset: u32);
    GRAPHICS_CONSTANT graphics_constant = state::graphics_constant, (slot: u32, value: u32, offset: u32);
    COMPUTE_VIEW compute_view = state::compute_view, (slot: u32, address: u64);
    GRAPHICS_VIEW graphics_view = state::graphics_view, (slot: u32, address: u64);
    SET_PIPELINE set_pipeline = super::set_pipeline, (pipeline: This);
    DRAW draw = state::draw, (vertices: u32, instances: u32, first: u32, first_instance: u32);
    DRAW_INDEXED draw_indexed = state::draw_indexed, (indices: u32, instances: u32, first: u32, base: i32, first_instance: u32);
    DISPATCH dispatch = state::dispatch, (x: u32, y: u32, z: u32);
    DISPATCH_X dispatch_x = super::dispatch_x, (x: u32, y: u32, z: u32, flags: u32);
    DISPATCH_MESH dispatch_mesh = state::dispatch_mesh, (x: u32, y: u32, z: u32);
    DISPATCH_MESH_X dispatch_mesh_x = state::dispatch_mesh_x, (x: u32, flags: u64, indirect: u64, payload: u64);
    COPY_BUFFER copy_buffer_region = super::copy_buffer_region, (dst: This, dst_offset: u64, src: This, src_offset: u64, len: u64);
    COPY_RESOURCE copy_resource = super::copy_resource, (dst: This, src: This);
    COPY_RESOURCE_X copy_resource_x = super::copy_resource_x, (dst: This, src: This, flags: u32);
    COPY_MEMORY copy_memory = super::copy_memory, (dst: u64, src: u64, len: u64, flags: u32);
    RESOLVE resolve = super::resolve, (dst: This, dst_sub: u32, src: This, src_sub: u32, format: u32);
    RESOLVE_QUERIES resolve_queries = super::resolve_queries, (heap: This, kind: u32, first: u32, count: u32, buffer: This, offset: u64);
    EXECUTE_INDIRECT execute_indirect = super::execute_indirect, (signature: This, max: u32, arguments: This, offset: u64, count: This, count_offset: u64);
    WRITE32 write32 = super::write32, (address: u64, value: u32, flush: u32, flags: u32);
    WRITE64 write64 = super::write64, (address: u64, value: u64, flush: u32, flags: u32);
    WAIT_VALUE wait_value = super::wait_value, (address: u64, comparison: u32, reference: u64);
    WAIT_VALUE64 wait_value64 = super::wait_value64, (address: u64, comparison: u32, reference: u64);
    SYNC sync = super::sync, (flags: u32, control: u32, address: u64, size: u64);
    BEGIN_QUERY begin_query = super::query_begin, (heap: This, kind: u32, index: u32);
    END_QUERY end_query = super::query_end, (heap: This, kind: u32, index: u32);
}

const fn after(n: u64) -> u64 {
    Plain::END as u64 + n
}
const RESET: u64 = after(0);
const CLOSE: u64 = after(1);
const UPLOAD: u64 = after(2);
const BARRIER: u64 = after(3);
const BARRIER_GROUPS: u64 = after(4);
const VIEWPORTS: u64 = after(5);
const SCISSORS: u64 = after(6);
const BLEND_FACTOR: u64 = after(7);
const HEAPS: u64 = after(8);
const COMPUTE_CONSTANTS: u64 = after(9);
const GRAPHICS_CONSTANTS: u64 = after(10);
const INDEX_BUFFER: u64 = after(11);
const VERTEX_BUFFERS: u64 = after(12);
const TARGETS: u64 = after(13);
const CLEAR_RTV: u64 = after(14);
const CLEAR_DSV: u64 = after(15);
const COPY_TEXTURE: u64 = after(16);

/// The bytes of `count` things at `items` (none if null).
fn bytes_of<'a, T>(items: *const T, count: usize) -> &'a [u8] {
    if items.is_null() || count == 0 {
        return &[];
    }
    // SAFETY: the title passes `count` items.
    unsafe { std::slice::from_raw_parts(items.cast(), count * size_of::<T>()) }
}

/// Where the things are in a stream (null for none).
fn at<T>(w: &[u64], from: usize, count: usize) -> *const T {
    if count == 0 || from >= w.len() { std::ptr::null() } else { w[from..].as_ptr().cast() }
}

/// [`at`] where an empty array is not null.
fn at_or<T>(w: &[u64], from: usize) -> *const T {
    // SAFETY: at most one past the end.
    unsafe { w.as_ptr().add(from.min(w.len())).cast() }
}

/// A view's record as it is now (zeros for no view), so that its slot may be
/// written again: the copy stands for the handle.
fn record(handle: u64) -> [u64; RECORD] {
    let mut words = [0u64; RECORD];
    if handle != 0 {
        // SAFETY: a descriptor slot the title passes (they are larger).
        unsafe {
            std::ptr::copy_nonoverlapping(handle as *const u8, words.as_mut_ptr().cast(), size_of::<RtvRecord>())
        };
    }
    words
}
const RECORD: usize = size_of::<RtvRecord>().div_ceil(8);

extern "system" fn list_close(this: This) -> HRESULT {
    front(this).close();
    S_OK
}
extern "system" fn list_reset(this: This, allocator: This, pipeline: This) -> HRESULT {
    if allocator.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes one of our lists.
    let kind = unsafe { (*this.cast::<List>()).kind };
    front(this).begin(kind, pipeline);
    S_OK
}
extern "system" fn list_reset_x(this: This, allocator: This, pipeline: This, _flags: u32) -> HRESULT {
    list_reset(this, allocator, pipeline)
}
extern "system" fn allocate_upload(this: This, size: u64, alignment: u64) -> *mut u8 {
    let align = usize::try_from(alignment).unwrap_or(0).max(16);
    let Some(len) = usize::try_from(size).ok().and_then(|s| s.checked_add(align)) else {
        return std::ptr::null_mut();
    };
    if this.is_null() || !align.is_power_of_two() {
        return std::ptr::null_mut();
    }
    let front = front(this);
    if front.closed || front.recording.is_none() {
        return std::ptr::null_mut();
    }
    let Some(mut block) = Upload::new(len) else {
        return std::ptr::null_mut();
    };
    let start = block.start();
    // SAFETY: the offset is below `align`, so `size` bytes fit after it.
    let place = unsafe { start.add(start.align_offset(align)) };
    front.put(UPLOAD, &[Box::into_raw(Box::new(block)) as u64]);
    place
}
extern "system" fn barrier(this: This, count: u32, barriers: *const Barrier) -> u32 {
    if !barriers.is_null() {
        front(this).put_with(BARRIER, &[u64::from(count)], &[bytes_of(barriers, count as usize)]);
    }
    0
}
extern "system" fn barrier_groups(this: This, count: u32, _groups: *const u8) {
    front(this).put(BARRIER_GROUPS, &[u64::from(count)]);
}
extern "system" fn viewports(this: This, count: u32, items: *const D3D12_VIEWPORT) {
    let count = if items.is_null() { 0 } else { count };
    front(this).put_with(VIEWPORTS, &[u64::from(count)], &[bytes_of(items, count as usize)]);
}
extern "system" fn scissors(this: This, count: u32, items: *const RECT) {
    let count = if items.is_null() { 0 } else { count };
    front(this).put_with(SCISSORS, &[u64::from(count)], &[bytes_of(items, count as usize)]);
}
extern "system" fn blend_factor(this: This, factor: *const [f32; 4]) {
    let given = !factor.is_null();
    front(this).put_with(BLEND_FACTOR, &[u64::from(given)], &[bytes_of(factor, usize::from(given))]);
}
extern "system" fn heaps(this: This, count: u32, items: *const This) {
    let count = if items.is_null() { 0 } else { count };
    front(this).put_with(HEAPS, &[u64::from(count)], &[bytes_of(items, count as usize)]);
}
fn constants(this: This, op: u64, slot: u32, count: u32, values: *const u32, offset: u32) {
    let count = if values.is_null() { 0 } else { count };
    front(this).put_with(
        op,
        &[u64::from(slot) | u64::from(offset) << 32, u64::from(count)],
        &[bytes_of(values, count as usize)],
    );
}
extern "system" fn compute_constants(this: This, slot: u32, count: u32, values: *const u32, offset: u32) {
    constants(this, COMPUTE_CONSTANTS, slot, count, values, offset);
}
extern "system" fn graphics_constants(this: This, slot: u32, count: u32, values: *const u32, offset: u32) {
    constants(this, GRAPHICS_CONSTANTS, slot, count, values, offset);
}
extern "system" fn index_buffer(this: This, view: *const IndexView) {
    let given = !view.is_null();
    front(this).put_with(INDEX_BUFFER, &[u64::from(given)], &[bytes_of(view, usize::from(given))]);
}
extern "system" fn vertex_buffers(this: This, start: u32, count: u32, views: *const VertexView) {
    let given = !views.is_null();
    front(this).put_with(
        VERTEX_BUFFERS,
        &[u64::from(start) | u64::from(count) << 32, u64::from(given)],
        &[bytes_of(views, count as usize)],
    );
}
extern "system" fn targets(this: This, count: u32, handles: *const u64, one_range: i32, depth: *const u64) {
    let count = if handles.is_null() { 0 } else { (count as usize).min(8) };
    // SAFETY: the title passes a handle or null.
    let depth = unsafe { depth.as_ref() }.copied().unwrap_or(0);
    let mut records = [0u64; RECORD * 9];
    let mut present = 0u64;
    for i in 0..count {
        // SAFETY: the title passes `count` handles, or the first of a range
        // of views of 128 bytes.
        let handle = unsafe { if one_range != 0 { *handles + 128 * i as u64 } else { *handles.add(i) } };
        present |= u64::from(handle != 0) << i;
        records[i * RECORD..(i + 1) * RECORD].copy_from_slice(&record(handle));
    }
    records[count * RECORD..(count + 1) * RECORD].copy_from_slice(&record(depth));
    let records = &records[..(count + 1) * RECORD];
    front(this).put_with(
        TARGETS,
        &[count as u64, present, u64::from(depth != 0)],
        &[bytes_of(records.as_ptr(), records.len())],
    );
}
extern "system" fn clear_rtv(this: This, handle: u64, color: *const [f32; 4], count: u32, rects: *const Rect) {
    if handle == 0 || color.is_null() {
        return;
    }
    let count = if rects.is_null() { 0 } else { count };
    let view = record(handle);
    front(this).put_with(
        CLEAR_RTV,
        &[u64::from(count)],
        &[bytes_of(view.as_ptr(), RECORD), bytes_of(color, 1), bytes_of(rects, count as usize)],
    );
}
extern "system" fn clear_dsv(
    this: This,
    handle: u64,
    flags: u32,
    depth: f32,
    stencil: u8,
    count: u32,
    rects: *const Rect,
) {
    if handle == 0 {
        return;
    }
    let count = if rects.is_null() { 0 } else { count };
    let view = record(handle);
    front(this).put_with(
        CLEAR_DSV,
        &[u64::from(count), u64::from(flags) | u64::from(stencil) << 32, Word::word(depth)],
        &[bytes_of(view.as_ptr(), RECORD), bytes_of(rects, count as usize)],
    );
}
extern "system" fn copy_texture_region(
    this: This,
    dst: *const CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const CopyLocation,
    source_box: *const [u32; 6],
) {
    if dst.is_null() || src.is_null() {
        return;
    }
    let boxed = !source_box.is_null();
    front(this).put_with(
        COPY_TEXTURE,
        &[u64::from(x) | u64::from(y) << 32, u64::from(z) | u64::from(boxed) << 32],
        &[bytes_of(dst, 1), bytes_of(src, 1), bytes_of(source_box, usize::from(boxed))],
    );
}
#[allow(clippy::too_many_arguments)] // the method's own arguments
extern "system" fn copy_texture_region_x(
    this: This,
    dst: *const CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const CopyLocation,
    source_box: *const [u32; 6],
    _flags: u32,
) {
    copy_texture_region(this, dst, x, y, z, src, source_box);
}

/// Whether runs of executions the title makes one after the other, with
/// nothing between them, one record each, the next record after the last, are
/// one execution of all of them (`VICTORY_INDIRECT_RUNS=1`; off by default).
/// Nothing is between them, so the state they run with is the same, and so
/// are the draws and their order.
fn runs_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_INDIRECT_RUNS").is_some_and(|v| v == "1"))
}

/// Whether what the title does between two executions that would be one is
/// noted (`VICTORY_INDIRECT_LOOK=1`).
fn look_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| runs_on() || std::env::var_os("VICTORY_INDIRECT_LOOK").is_some_and(|v| v == "1"))
}

/// Whether runs of executions that differ in the root arguments of their
/// stages are made one execution (`VICTORY_INDIRECT_FOLD=1`; off by
/// default; `crate::draw::fold`). Read once per stream: a stream plans and
/// makes its runs alone, and leaves the host list as the executions one by
/// one would, so streams translated with the switch on and off can follow
/// each other on one list.
fn fold_on() -> bool {
    crate::ab::live_flag!("VICTORY_INDIRECT_FOLD")
}

/// The most executions of a run made one.
const FOLD_MOST: u32 = 64;

/// The executions that may be one run, from the one with `signature` that was
/// just taken (`at` is the next op): how many there are (that one included),
/// and where the stream goes on after the last. What may lie between them is
/// only what sets roots the title sets for each object (tables, constants,
/// views), uploads, and the pipeline the list already has (`pipeline`, asked
/// if one is set): nothing that the host list is told of on its own. The
/// executions are the ones of one record without a count of one signature.
fn fold_run(stream: &[u64], mut at: usize, signature: u64, pipeline: &dyn Fn() -> u64) -> (u32, usize) {
    let (mut n, mut end) = (1u32, at);
    while at < stream.len() && n < FOLD_MOST {
        let (op, len) = (stream[at] & 0xffff, (stream[at] >> 16) as usize);
        let Some(w) = stream.get(at + 1..at + 1 + len) else {
            break;
        };
        at += 1 + len;
        if op == Plain::EXECUTE_INDIRECT as u64 {
            if single(w).is_some_and(|(s, ..)| s == signature) {
                n += 1;
                end = at;
                continue;
            }
            break;
        }
        let roots = [
            Plain::COMPUTE_TABLE as u64,
            Plain::GRAPHICS_TABLE as u64,
            Plain::COMPUTE_CONSTANT as u64,
            Plain::GRAPHICS_CONSTANT as u64,
            Plain::COMPUTE_VIEW as u64,
            Plain::GRAPHICS_VIEW as u64,
        ];
        let between = op == UPLOAD
            || op == COMPUTE_CONSTANTS
            || op == GRAPHICS_CONSTANTS
            || roots.contains(&op)
            || (op == Plain::SET_PIPELINE as u64 && w.first().is_some_and(|&p| p == pipeline()));
        if !between {
            break;
        }
    }
    (n, end)
}

/// Since the last line: executions with one record and no count; each that
/// is the next record of the one before (same signature and arguments); of
/// those, with nothing between; runs made one and the executions that saved.
static RUNS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// What lay between executions that were the next record of the one before
/// (the first op codes), and how often.
static BETWEEN: Mutex<Option<std::collections::HashMap<Vec<u8>, u64>>> = Mutex::new(None);

/// The names of the plain ops and the others, by code.
const NAMES: [&str; 47] = [
    "topology", "stencil_ref", "depth_bounds", "compute_table", "graphics_table", "compute_constant",
    "graphics_constant", "compute_view", "graphics_view", "set_pipeline", "draw", "draw_indexed", "dispatch",
    "dispatch_x", "dispatch_mesh", "dispatch_mesh_x", "copy_buffer", "copy_resource", "copy_resource_x",
    "copy_memory", "resolve", "resolve_queries", "execute_indirect", "write32", "write64", "wait_value",
    "wait_value64", "sync", "begin_query", "end_query", "reset", "close", "upload", "barrier",
    "barrier_groups", "viewports", "scissors",
    "blend_factor", "heaps", "compute_constants", "graphics_constants", "index_buffer", "vertex_buffers",
    "targets", "clear_rtv", "clear_dsv", "copy_texture",
];

/// The line of what the executions come to, per frame.
pub(crate) fn runs_line(frames: f64) {
    #[cfg(windows)]
    crate::draw::fold::line(frames);
    let r: Vec<u64> = RUNS.iter().map(|r| r.swap(0, Relaxed)).collect();
    let between = held(&BETWEEN).take();
    let d: Vec<u64> = DROPS.iter().map(|d| d.swap(0, Relaxed)).collect();
    if d.iter().sum::<u64>() > 0 && frames > 0.0 {
        tracing::info!(
            "calls that only feed dispatches the host drops, left out per frame (VICTORY_DROP_EARLY): {:.0} root parameters set, {:.0} dispatches; {:.0} streams planned",
            d[0] as f64 / frames,
            d[1] as f64 / frames,
            d[2] as f64 / frames,
        );
    }
    if r[0] == 0 || frames <= 0.0 {
        return;
    }
    let per = |v: u64| v as f64 / frames;
    let mut top = String::new();
    if let Some(between) = between {
        let mut all: Vec<_> = between.into_iter().collect();
        all.sort_by_key(|(k, n)| (std::cmp::Reverse(*n), k.clone()));
        for (ops, n) in all.into_iter().take(8) {
            let names: Vec<&str> = ops.iter().map(|&o| NAMES.get(o as usize).copied().unwrap_or("?")).collect();
            top.push_str(&format!("; {:.1}/frame after [{}]", per(n), names.join(" ")));
        }
    }
    tracing::info!(
        "executions with one record per frame (VICTORY_INDIRECT_RUNS={}): {:.0}, of them {:.0} the next record of the one before, {:.0} of those with nothing between; {:.0} runs made one, saving {:.0} executions{top}",
        u8::from(runs_on()),
        per(r[0]),
        per(r[1]),
        per(r[2]),
        per(r[3]),
        per(r[4]),
    );
}

/// The call `execute_indirect(signature, max, arguments, offset, count,
/// count offset)` as the words of a stream hold it, if it is one execution
/// of one record without a count: the signature, the arguments, the offset.
fn single(w: &[u64]) -> Option<(u64, u64, u64)> {
    (w.len() >= 6 && w[0] != 0 && w[1] as u32 == 1 && w[2] != 0 && w[4] == 0).then(|| (w[0], w[2], w[3]))
}

/// How far the run of executions that the one with `first` begins goes in
/// `stream` from `at` (the next op): how many records it is, and where the
/// stream goes on. Only uploads (which take no part in what the card does)
/// may lie between; the ones in the run are `done` as they pass.
fn run_of(
    stream: &[u64],
    mut at: usize,
    first: (u64, u64, u64),
    stride: u64,
    mut done: impl FnMut(&[u64]),
) -> (u64, usize) {
    let (signature, arguments, offset) = first;
    let mut records = 1u64;
    let mut uploads: Vec<usize> = Vec::new();
    let mut end = at;
    while at < stream.len() {
        let (op, len) = (stream[at] & 0xffff, (stream[at] >> 16) as usize);
        let Some(w) = stream.get(at + 1..at + 1 + len) else {
            break;
        };
        if op == UPLOAD {
            uploads.push(at);
            at += 1 + len;
            continue;
        }
        let next = single(w)
            .filter(|&(s, a, o)| op == Plain::EXECUTE_INDIRECT as u64 && s == signature && a == arguments && o == offset + records * stride);
        if next.is_none() {
            break;
        }
        records += 1;
        at += 1 + len;
        end = at;
    }
    for u in uploads.into_iter().filter(|&u| u < end) {
        done(&stream[u + 1..u + 1 + (stream[u] >> 16) as usize]);
    }
    (records, end)
}

/// Whether calls that only feed dispatches the host drops are left out
/// (`VICTORY_DROP_EARLY=1`; off by default; with the structure builder off,
/// which is what drops them). Read for each stream, so it may change while the
/// title runs: what a stream leaves out is a stream's own, and the state it
/// leaves is the same whichever way the switch stands (`dead_ops`).
fn drop_early_on() -> bool {
    if !crate::pipeline::builder_disabled() {
        return false;
    }
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_DROP_EARLY") {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DROP_EARLY").is_some_and(|v| v == "1"))
}

/// Since the last line: root parameters set and dispatches left out, and
/// streams planned.
static DROPS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Which calls of `stream` (by their order, uploads counted) need not be made,
/// given the pipeline the list has at its start (`start`) and which pipelines
/// drop their dispatches (`dropped`).
///
/// - A dispatch while a dropped pipeline is set does nothing: left out.
/// - A compute root parameter set (table, view, or constants) is read only by
///   a dispatch that runs, or an execution that is not dropped. Roots persist
///   from one dispatch to the next, so one that is replaced whole (a table or
///   a view; constants only replace the dwords they give) before any such
///   consumer is read by nobody: left out. At the end of the stream every
///   parameter counts as read (the next stream goes on from here), and a
///   consumer reads them all.
/// - The pipelines the title sets are all kept (they say which dispatch is
///   dropped), and everything else.
fn dead_ops(stream: &[u64], start: u64, dropped: &dyn Fn(u64) -> bool) -> Vec<bool> {
    const SLOTS: u32 = 64;
    let mut ops: Vec<(u64, &[u64])> = Vec::new();
    let mut at = 0;
    while at < stream.len() {
        let (op, len) = (stream[at] & 0xffff, (stream[at] >> 16) as usize);
        let Some(w) = stream.get(at + 1..at + 1 + len) else {
            break;
        };
        ops.push((op, w));
        at += 1 + len;
    }
    let (dispatch, dispatch_x, indirect) = (Plain::DISPATCH as u64, Plain::DISPATCH_X as u64, Plain::EXECUTE_INDIRECT as u64);
    // Forward: each dispatch is dropped or a consumer.
    let mut cur = start;
    let mut consumer = vec![false; ops.len()];
    let mut dead = vec![false; ops.len()];
    for (i, &(op, w)) in ops.iter().enumerate() {
        if (op == RESET || op == Plain::SET_PIPELINE as u64) && !w.is_empty() {
            cur = w[0];
        } else if op == dispatch || op == dispatch_x {
            if dropped(cur) {
                dead[i] = true;
            } else {
                consumer[i] = true;
            }
        } else if op == indirect {
            consumer[i] = !dropped(cur);
        }
    }
    // Backward: the parameters something later reads.
    let mut live = u64::MAX;
    for (i, &(op, w)) in ops.iter().enumerate().rev() {
        if consumer[i] {
            live = u64::MAX;
            continue;
        }
        let slot = match op {
            o if o == Plain::COMPUTE_TABLE as u64 || o == Plain::COMPUTE_VIEW as u64 || o == Plain::COMPUTE_CONSTANT as u64 => {
                w.first().map(|&s| s as u32)
            }
            o if o == COMPUTE_CONSTANTS => w.first().map(|&s| s as u32),
            _ => None,
        };
        let Some(slot) = slot else { continue };
        if slot >= SLOTS {
            continue;
        }
        let whole = op == Plain::COMPUTE_TABLE as u64 || op == Plain::COMPUTE_VIEW as u64;
        if live >> slot & 1 == 0 {
            dead[i] = true;
        } else if whole {
            live &= !(1 << slot);
        }
    }
    dead
}

/// Makes the calls of a stream on `list`: how many, and whether the last one
/// closed it. Once `left` says so only what must not be lost is done.
fn translate(list: This, stream: &[u64], left: impl Fn() -> bool) -> (u64, bool) {
    let (mut i, mut calls, mut closed) = (0, 0, false);
    let mut skip = left();
    // The calls that need not be made (`VICTORY_DROP_EARLY`), by their order.
    let dead = if drop_early_on() && !skip {
        // SAFETY: a list of ours that no thread but this one records into.
        let start = held(unsafe { &(*list.cast::<List>()).state }).pipeline_handle();
        DROPS[2].fetch_add(1, Relaxed);
        dead_ops(stream, start, &|p| crate::pipeline::is_builder(p as This))
    } else {
        Vec::new()
    };
    let mut ordinal = 0usize;
    // The execution before: its signature, arguments and where its next
    // record would be; what came since (op codes, the first few, and how many).
    let mut last: Option<(u64, u64, u64)> = None;
    let mut between: Vec<u8> = Vec::new();
    let mut since = 0u32;
    // Where the run of executions made one ends (`crate::draw::fold`), 0: none.
    let mut fold_until = 0usize;
    let name_upload = |w: &[u64]| {
        // SAFETY: made by `allocate_upload`, named once.
        let block = *unsafe { Box::from_raw(w[0] as *mut Upload) };
        // SAFETY: a list of ours.
        let uploads = unsafe { &(*list.cast::<List>()).uploads };
        held(uploads).push(block);
    };
    while i < stream.len() {
        if fold_until != 0 && i >= fold_until {
            super::fold_end(list);
            fold_until = 0;
        }
        let (op, len) = (stream[i] & 0xffff, (stream[i] >> 16) as usize);
        let w = &stream[i + 1..i + 1 + len];
        i += 1 + len;
        calls += 1;
        let this_op = ordinal;
        ordinal += 1;
        if calls % 64 == 0 {
            skip = left();
        }
        if op == UPLOAD {
            name_upload(w);
            continue;
        }
        if skip && op != CLOSE && op != RESET {
            continue;
        }
        if dead.get(this_op).copied().unwrap_or(false) {
            DROPS[usize::from(op == Plain::DISPATCH as u64 || op == Plain::DISPATCH_X as u64)].fetch_add(1, Relaxed);
            continue;
        }
        if op == Plain::EXECUTE_INDIRECT as u64 {
            RUNS[0].fetch_add(1, Relaxed);
            let first = single(w);
            if let Some((signature, arguments, offset)) = first {
                if last == Some((signature, arguments, offset)) {
                    RUNS[1].fetch_add(1, Relaxed);
                    if since == 0 {
                        RUNS[2].fetch_add(1, Relaxed);
                    } else if look_on() {
                        let mut seen = held(&BETWEEN);
                        let seen = seen.get_or_insert_with(Default::default);
                        if seen.len() < 256 {
                            *seen.entry(between.clone()).or_insert(0) += 1;
                        }
                    }
                }
                let stride = crate::signature::layout(signature as This).map_or(0, |(stride, _)| u64::from(stride));
                last = (stride > 0).then_some((signature, arguments, offset + stride));
                if runs_on() && stride > 0 {
                    let (records, end) = run_of(stream, i, (signature, arguments, offset), stride, &name_upload);
                    if records > 1 {
                        // One execution of all the records; the calls it
                        // stands for are counted as draws.
                        calls += records - 1;
                        RUNS[3].fetch_add(1, Relaxed);
                        RUNS[4].fetch_add(records - 1, Relaxed);
                        super::count_draws(records - 1);
                        super::execute_indirect(
                            list,
                            signature as This,
                            records as u32,
                            arguments as This,
                            offset,
                            std::ptr::null(),
                            0,
                        );
                        i = end;
                        last = Some((signature, arguments, offset + records * stride));
                        between.clear();
                        since = 0;
                        continue;
                    }
                }
            } else {
                last = None;
            }
            between.clear();
            since = 0;
            // The executions after this one that may be one with it.
            if fold_until == 0
                && !skip
                && dead.is_empty()
                && !runs_on()
                && let Some((signature, ..)) = first
                && fold_on()
            {
                // SAFETY: a list of ours that no thread but this one records into.
                let pipeline = || held(unsafe { &(*list.cast::<List>()).state }).pipeline_handle();
                let (n, end) = fold_run(stream, i, signature, &pipeline);
                if n >= 2 {
                    super::fold_begin(list, n);
                    fold_until = end;
                }
            }
        } else {
            since += 1;
            if look_on() && between.len() < 8 {
                between.push(op.min(255) as u8);
            }
        }
        if run_plain(op, list, w) {
            continue;
        }
        let n = w.first().map_or(0, |&n| n as usize);
        match op {
            RESET => {
                // (Only whether there is an allocator is looked at.)
                let allocator: This = std::ptr::NonNull::<u64>::dangling().as_ptr().cast_const().cast();
                super::reset(list, allocator, w[0] as This);
            }
            CLOSE => {
                super::close(list);
                closed = true;
            }
            BARRIER => {
                super::barrier(list, n as u32, at_or(w, 1));
            }
            BARRIER_GROUPS => super::barrier_groups(list, n as u32, std::ptr::null()),
            VIEWPORTS => state::viewports(list, n as u32, at(w, 1, n)),
            SCISSORS => state::scissors(list, n as u32, at(w, 1, n)),
            BLEND_FACTOR => state::blend_factor(list, at(w, 1, n)),
            HEAPS => state::heaps(list, n as u32, at(w, 1, n)),
            COMPUTE_CONSTANTS => {
                state::compute_constants(list, w[0] as u32, w[1] as u32, at(w, 2, w[1] as usize), (w[0] >> 32) as u32)
            }
            GRAPHICS_CONSTANTS => {
                state::graphics_constants(list, w[0] as u32, w[1] as u32, at(w, 2, w[1] as usize), (w[0] >> 32) as u32)
            }
            INDEX_BUFFER => state::index_buffer(list, at(w, 1, n)),
            VERTEX_BUFFERS => {
                let views = if w[1] != 0 { at_or(w, 2) } else { std::ptr::null() };
                state::vertex_buffers(list, w[0] as u32, (w[0] >> 32) as u32, views);
            }
            TARGETS => {
                let mut handles = [0u64; 8];
                let place = |k: usize| w[3 + k * RECORD..].as_ptr() as u64;
                for (k, h) in handles.iter_mut().enumerate().take(n) {
                    if w[1] >> k & 1 != 0 {
                        *h = place(k);
                    }
                }
                let depth = if w[2] != 0 { place(n) } else { 0 };
                state::targets(list, n as u32, handles.as_ptr(), 0, &depth);
            }
            CLEAR_RTV => {
                super::clear_rtv(
                    list,
                    w[1..].as_ptr() as u64,
                    at_or(w, 1 + RECORD),
                    n as u32,
                    at(w, 1 + RECORD + 2, n),
                );
            }
            CLEAR_DSV => {
                super::clear_dsv(
                    list,
                    w[3..].as_ptr() as u64,
                    w[1] as u32,
                    Word::unword(w[2]),
                    (w[1] >> 32) as u8,
                    n as u32,
                    at(w, 3 + RECORD, n),
                );
            }
            COPY_TEXTURE => {
                let step = size_of::<CopyLocation>().div_ceil(8);
                let boxed = (w[1] >> 32) as usize;
                super::copy_texture_region(
                    list,
                    at_or(w, 2),
                    w[0] as u32,
                    (w[0] >> 32) as u32,
                    w[1] as u32,
                    at_or(w, 2 + step),
                    at(w, 2 + 2 * step, boxed),
                );
            }
            _ => {}
        }
    }
    if fold_until != 0 {
        super::fold_end(list);
    }
    (calls, closed)
}

/// The table of a list whose calls are written down: that of the others with
/// the methods that record replaced (slots as in [`super::METHODS`]).
pub(super) const fn table(newer: bool) -> [*const (); 160] {
    let mut f = super::list_table(newer);
    let mut i = 0;
    while i < MINE.len() {
        let (slot, method) = MINE[i];
        f[if newer { newer_slot(slot) } else { slot }] = method;
        i += 1;
    }
    if newer {
        f[146] = barrier_groups as *const ();
        f[148] = allocate_upload as *const ();
    }
    f
}

const MINE: [(usize, *const ()); 52] = [
    (9, list_close as *const ()),
    (10, list_reset as *const ()),
    (12, draw as *const ()),
    (13, draw_indexed as *const ()),
    (14, dispatch as *const ()),
    (15, copy_buffer_region as *const ()),
    (16, copy_texture_region as *const ()),
    (17, copy_resource as *const ()),
    (19, resolve as *const ()),
    (20, topology as *const ()),
    (21, viewports as *const ()),
    (22, scissors as *const ()),
    (23, blend_factor as *const ()),
    (24, stencil_ref as *const ()),
    (25, set_pipeline as *const ()),
    (26, barrier as *const ()),
    (28, begin_query as *const ()),
    (29, end_query as *const ()),
    (30, resolve_queries as *const ()),
    (32, heaps as *const ()),
    (35, compute_table as *const ()),
    (36, graphics_table as *const ()),
    (37, compute_constant as *const ()),
    (38, graphics_constant as *const ()),
    (39, compute_constants as *const ()),
    (40, graphics_constants as *const ()),
    (41, compute_view as *const ()),
    (42, graphics_view as *const ()),
    (43, compute_view as *const ()),
    (44, graphics_view as *const ()),
    (45, compute_view as *const ()),
    (46, graphics_view as *const ()),
    (47, index_buffer as *const ()),
    (48, vertex_buffers as *const ()),
    (50, targets as *const ()),
    (51, clear_dsv as *const ()),
    (52, clear_rtv as *const ()),
    (59, execute_indirect as *const ()),
    (60, copy_memory as *const ()),
    (68, depth_bounds as *const ()),
    (69, write32 as *const ()),
    (70, write64 as *const ()),
    (71, wait_value as *const ()),
    (72, wait_value64 as *const ()),
    (78, dispatch_x as *const ()),
    (83, copy_texture_region_x as *const ()),
    (84, copy_resource_x as *const ()),
    (98, dispatch_mesh_x as *const ()),
    (127, dispatch_mesh as *const ()),
    (129, sync as *const ()),
    (130, sync as *const ()),
    (137, list_reset_x as *const ()),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every method of the tables that records is written down too: what is
    /// left in both tables alike records nothing.
    #[test]
    fn every_recording_method_is_written_down() {
        const NOTHING: [usize; 14] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 33, 34, 73, 99, 124];
        let mine: Vec<usize> = MINE.iter().map(|m| m.0).collect();
        for (slot, _) in METHODS.iter().chain(MORE_METHODS.iter()) {
            assert!(mine.contains(slot) != NOTHING.contains(slot), "slot {slot}");
        }
        for &slot in &mine {
            assert!(
                METHODS.iter().chain(MORE_METHODS.iter()).any(|m| m.0 == slot),
                "slot {slot} is written down and not there"
            );
        }
    }

    fn put(stream: &mut Vec<u64>, op: u64, args: &[u64]) {
        stream.push(op | (args.len() as u64) << 16);
        stream.extend_from_slice(args);
    }

    const EXECUTE: u64 = Plain::EXECUTE_INDIRECT as u64;

    /// The names go by the codes.
    #[test]
    fn the_names_of_the_ops_are_by_their_codes() {
        assert_eq!(NAMES.len(), COPY_TEXTURE as usize + 1);
        for (op, name) in [
            (Plain::EXECUTE_INDIRECT as u64, "execute_indirect"),
            (Plain::DRAW as u64, "draw"),
            (Plain::SYNC as u64, "sync"),
            (RESET, "reset"),
            (UPLOAD, "upload"),
            (BARRIER_GROUPS, "barrier_groups"),
            (COPY_TEXTURE, "copy_texture"),
        ] {
            assert_eq!(NAMES[op as usize], name);
        }
    }

    /// A run is the executions of one record, one after the other, each the
    /// next record of the one before; uploads may lie between; anything else
    /// ends it, as do another signature, other arguments, a gap, a count or
    /// more records.
    #[test]
    fn a_run_is_the_executions_that_follow_one_another_record_by_record() {
        let call = |signature: u64, max: u64, arguments: u64, offset: u64, count: u64| [signature, max, arguments, offset, count, 0];
        let mut stream = Vec::new();
        // The first of the run was taken by the caller; its offset is 0x100, stride 16.
        put(&mut stream, EXECUTE, &call(7, 1, 9, 0x110, 0));
        put(&mut stream, UPLOAD, &[0xdead]);
        put(&mut stream, EXECUTE, &call(7, 1, 9, 0x120, 0));
        let upto_third = stream.len();
        put(&mut stream, UPLOAD, &[0xbeef]);
        put(&mut stream, Plain::DRAW as u64, &[3, 1, 0, 0]);
        put(&mut stream, EXECUTE, &call(7, 1, 9, 0x130, 0));
        let mut named = Vec::new();
        let (records, end) = run_of(&stream, 0, (7, 9, 0x100), 16, |w| named.push(w[0]));
        assert_eq!((records, end), (3, upto_third));
        // The upload between the executions was named; the one behind the run was not.
        assert_eq!(named, [0xdead]);
        // The same without the draw: the second upload is also between.
        let mut again = Vec::new();
        again.extend_from_slice(&stream[..upto_third]);
        put(&mut again, UPLOAD, &[0xbeef]);
        put(&mut again, EXECUTE, &call(7, 1, 9, 0x130, 0));
        let mut named = Vec::new();
        assert_eq!(run_of(&again, 0, (7, 9, 0x100), 16, |w| named.push(w[0])), (4, again.len()));
        assert_eq!(named, [0xdead, 0xbeef]);
        // Each thing that ends a run.
        for (what, next) in [
            ("another signature", call(8, 1, 9, 0x110, 0)),
            ("other arguments", call(7, 1, 10, 0x110, 0)),
            ("a gap", call(7, 1, 9, 0x120, 0)),
            ("the same record again", call(7, 1, 9, 0x100, 0)),
            ("a count", call(7, 1, 9, 0x110, 5)),
            ("more records", call(7, 2, 9, 0x110, 0)),
        ] {
            let mut stream = Vec::new();
            put(&mut stream, EXECUTE, &next);
            assert_eq!(run_of(&stream, 0, (7, 9, 0x100), 16, |_| ()), (1, 0), "{what}");
        }
        // Nothing after it, and an op cut short.
        assert_eq!(run_of(&[], 0, (7, 9, 0x100), 16, |_| ()), (1, 0));
        assert_eq!(run_of(&[EXECUTE | 6 << 16, 7], 0, (7, 9, 0x100), 16, |_| ()), (1, 0));
    }

    /// A fold is the executions of one signature with only what the title sets for each object between them (roots, uploads, the
    /// pipeline the list has): anything else, another signature, a count or more records ends it, and it is never longer than the
    /// most a run holds.
    #[test]
    fn a_fold_is_the_executions_with_only_roots_between() {
        let call = |signature: u64, max: u64, count: u64| [signature, max, 9, 0x40, count, 0];
        let pipeline = || 5u64;
        let mut stream = Vec::new();
        put(&mut stream, Plain::GRAPHICS_CONSTANT as u64, &[3, 7, 0]);
        put(&mut stream, GRAPHICS_CONSTANTS, &[3, 2, 1]);
        put(&mut stream, UPLOAD, &[1]);
        put(&mut stream, EXECUTE, &call(7, 1, 0));
        put(&mut stream, Plain::GRAPHICS_TABLE as u64, &[1, 0x100]);
        put(&mut stream, Plain::COMPUTE_VIEW as u64, &[2, 0x200]);
        put(&mut stream, Plain::SET_PIPELINE as u64, &[5]);
        put(&mut stream, EXECUTE, &call(7, 1, 0));
        let upto = stream.len();
        put(&mut stream, Plain::GRAPHICS_VIEW as u64, &[2, 0x200]);
        put(&mut stream, Plain::COMPUTE_CONSTANT as u64, &[1, 2, 0]);
        // (What follows the last execution is not part of the run.)
        assert_eq!(fold_run(&stream, 0, 7, &pipeline), (3, upto));
        // Each thing that ends it, right after the execution that was taken.
        for (what, op, words) in [
            ("a draw", Plain::DRAW as u64, vec![3, 1, 0, 0]),
            ("another pipeline", Plain::SET_PIPELINE as u64, vec![6]),
            ("a barrier", BARRIER, vec![1, 0]),
            ("targets", TARGETS, vec![1, 1, 0, 0]),
            ("another signature", EXECUTE, call(8, 1, 0).to_vec()),
            ("a count", EXECUTE, call(7, 1, 3).to_vec()),
            ("more records", EXECUTE, call(7, 2, 0).to_vec()),
        ] {
            let mut stream = Vec::new();
            put(&mut stream, op, &words);
            put(&mut stream, EXECUTE, &call(7, 1, 0));
            assert_eq!(fold_run(&stream, 0, 7, &pipeline), (1, 0), "{what}");
        }
        // Nothing after it, an op cut short, and the most.
        assert_eq!(fold_run(&[], 0, 7, &pipeline), (1, 0));
        assert_eq!(fold_run(&[EXECUTE | 6 << 16, 7], 0, 7, &pipeline), (1, 0));
        let mut long = Vec::new();
        for _ in 0..FOLD_MOST + 10 {
            put(&mut long, EXECUTE, &call(7, 1, 0));
        }
        let (n, end) = fold_run(&long, 0, 7, &pipeline);
        assert_eq!(n, FOLD_MOST);
        assert_eq!(end, (FOLD_MOST as usize - 1) * 7);
        #[cfg(windows)]
        assert_eq!(FOLD_MOST, crate::draw::fold::MOST);
    }

    /// Which calls are executions of one record without a count.
    #[test]
    fn one_record_without_a_count_is_a_single() {
        assert_eq!(single(&[7, 1, 9, 0x40, 0, 0]), Some((7, 9, 0x40)));
        for w in [[0u64, 1, 9, 0, 0, 0], [7, 2, 9, 0, 0, 0], [7, 1, 0, 0, 0, 0], [7, 1, 9, 0, 3, 0]] {
            assert_eq!(single(&w), None);
        }
        assert_eq!(single(&[7, 1, 9]), None);
    }

    /// What the compute roots hold, as the state keeps them.
    #[derive(Clone, Debug, PartialEq)]
    enum Root {
        Table(u64),
        View(u64),
        Constants(Vec<u32>),
    }

    type Roots = std::collections::BTreeMap<u32, Root>;

    /// The state a stream leaves, and what each dispatch that runs sees (the
    /// pipeline and every root), run without the calls in `skip`. Pipelines of
    /// an even number are dropped.
    fn model(stream: &[u64], start: u64, skip: &[bool]) -> (Vec<(u64, Roots)>, (u64, Roots)) {
        let mut roots: Roots = Default::default();
        let (mut cur, mut seen) = (start, Vec::new());
        let (mut at, mut k) = (0, 0);
        while at < stream.len() {
            let (op, len) = (stream[at] & 0xffff, (stream[at] >> 16) as usize);
            let w = &stream[at + 1..at + 1 + len];
            at += 1 + len;
            let this = k;
            k += 1;
            if skip.get(this).copied().unwrap_or(false) {
                continue;
            }
            let slot = w.first().map_or(0, |&s| s as u32);
            match op {
                o if o == Plain::SET_PIPELINE as u64 || o == RESET => cur = w[0],
                o if o == Plain::COMPUTE_TABLE as u64 => {
                    roots.insert(slot, Root::Table(w[1]));
                }
                o if o == Plain::COMPUTE_VIEW as u64 => {
                    roots.insert(slot, Root::View(w[1]));
                }
                o if o == Plain::COMPUTE_CONSTANT as u64 || o == COMPUTE_CONSTANTS => {
                    let (offset, values): (usize, Vec<u32>) = if o == COMPUTE_CONSTANTS {
                        let count = w[1] as usize;
                        ((w[0] >> 32) as usize, (0..count).map(|i| (w[2 + i / 2] >> (32 * (i % 2))) as u32).collect())
                    } else {
                        (w[2] as usize, vec![w[1] as u32])
                    };
                    if !matches!(roots.get(&slot), Some(Root::Constants(_))) {
                        roots.insert(slot, Root::Constants(Vec::new()));
                    }
                    if let Some(Root::Constants(c)) = roots.get_mut(&slot) {
                        if c.len() < offset + values.len() {
                            c.resize(offset + values.len(), 0);
                        }
                        c[offset..offset + values.len()].copy_from_slice(&values);
                    }
                }
                o if (o == Plain::DISPATCH as u64 || o == Plain::DISPATCH_X as u64) && cur % 2 == 1 => {
                    seen.push((cur, roots.clone()));
                }
                o if o == Plain::EXECUTE_INDIRECT as u64 && cur % 2 == 1 => seen.push((cur, roots.clone())),
                _ => {}
            }
        }
        (seen, (cur, roots))
    }

    /// Calls left out change nothing a dispatch that runs, or the next stream,
    /// can see: the pipeline and every compute root are as without leaving any
    /// out, at each dispatch and at the end, over many streams of sets and
    /// dispatches of running and dropped pipelines.
    #[test]
    fn the_next_dispatch_that_runs_sees_what_it_saw_with_none_left_out() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |m: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % m
        };
        let (mut left_out, mut dispatches_out) = (0, 0);
        for round in 0..400 {
            let mut stream = Vec::new();
            for _ in 0..(10 + next(70)) {
                let slot = next(5);
                match next(11) {
                    0 | 1 => put(&mut stream, Plain::SET_PIPELINE as u64, &[1 + next(4)]),
                    2 | 3 => put(&mut stream, Plain::COMPUTE_TABLE as u64, &[slot, 100 + next(50)]),
                    4 => put(&mut stream, Plain::COMPUTE_VIEW as u64, &[slot, 200 + next(50)]),
                    5 => put(&mut stream, Plain::COMPUTE_CONSTANT as u64, &[slot, next(1000), next(3)]),
                    6 => {
                        let count = 1 + next(3);
                        let words: Vec<u64> = (0..count.div_ceil(2)).map(|_| next(1 << 40)).collect();
                        let mut all = vec![slot | next(3) << 32, count];
                        all.extend(words);
                        put(&mut stream, COMPUTE_CONSTANTS, &all);
                    }
                    7 | 8 => put(&mut stream, Plain::DISPATCH_X as u64, &[1, 1, 1, 0]),
                    9 => put(&mut stream, Plain::DISPATCH as u64, &[1, 1, 1]),
                    _ => match next(3) {
                        0 => put(&mut stream, UPLOAD, &[1]),
                        1 => put(&mut stream, Plain::EXECUTE_INDIRECT as u64, &[5, 1, 9, 0, 0, 0]),
                        _ => put(&mut stream, Plain::DRAW as u64, &[3, 1, 0, 0]),
                    },
                }
            }
            let start = 1 + next(4);
            let dead = dead_ops(&stream, start, &|p| p % 2 == 0);
            assert_eq!(model(&stream, start, &[]), model(&stream, start, &dead), "round {round}");
            left_out += dead.iter().filter(|&&d| d).count();
            let (mut at, mut k) = (0, 0);
            while at < stream.len() {
                let (op, len) = (stream[at] & 0xffff, (stream[at] >> 16) as usize);
                dispatches_out += usize::from(dead[k] && (op == Plain::DISPATCH as u64 || op == Plain::DISPATCH_X as u64));
                at += 1 + len;
                k += 1;
            }
        }
        // It is not nothing that is left out: parameters, and dispatches.
        assert!(left_out > 1000 && dispatches_out > 100, "{left_out} {dispatches_out}");
    }

    /// The case that matters: stages of the structure builder that are dropped
    /// set the same parameters one after another, and the dispatch that runs
    /// after them sets some of its own: only the last set of what it does not
    /// set is made.
    #[test]
    fn stages_that_are_dropped_leave_the_last_set_of_each_parameter() {
        let mut stream = Vec::new();
        // The pipeline the list has is a real one (1); 2 is a stage that is dropped.
        for stage in 0..3u64 {
            put(&mut stream, Plain::SET_PIPELINE as u64, &[2]);
            put(&mut stream, Plain::COMPUTE_TABLE as u64, &[0, 10 + stage]);
            put(&mut stream, Plain::COMPUTE_VIEW as u64, &[1, 20 + stage]);
            put(&mut stream, Plain::DISPATCH_X as u64, &[8, 1, 1, 0]);
        }
        put(&mut stream, Plain::SET_PIPELINE as u64, &[1]);
        put(&mut stream, Plain::COMPUTE_TABLE as u64, &[0, 99]);
        put(&mut stream, Plain::DISPATCH as u64, &[1, 1, 1]);
        let dead = dead_ops(&stream, 1, &|p| p == 2);
        // Per stage (pipeline, table, view, dispatch): the dispatch always, the
        // table of slot 0 always (the dispatch that runs sets it), the view of
        // slot 1 for the stages before the last (that dispatch reads the last).
        assert_eq!(
            dead,
            [false, true, true, true, false, true, true, true, false, true, false, true, false, false, false]
        );
        assert_eq!(model(&stream, 1, &[]), model(&stream, 1, &dead));
    }

    /// Arguments come out of a stream as they went in.
    #[test]
    fn a_stream_gives_back_what_was_written() {
        let mut f = Front::default();
        f.recording = Some(Arc::new(Recording::new(0)));
        f.put(Plain::DRAW_INDEXED as u64, &[3, 1, 0, Word::word(-7i32), 5]);
        let viewport = [1.0f32, 2.0, 3.0, 4.0, 0.0, 1.0];
        f.put_with(VIEWPORTS, &[1], &[bytes_of(viewport.as_ptr(), 6)]);
        f.put_with(COMPUTE_CONSTANTS, &[4 | 2 << 32, 3], &[bytes_of([7u32, 8, 9].as_ptr(), 3)]);
        let s = std::mem::take(&mut f.words);
        assert_eq!((s[0] & 0xffff, s[0] >> 16), (Plain::DRAW_INDEXED as u64, 5));
        assert_eq!(<i32 as Word>::unword(s[4]), -7);
        let w = &s[6..];
        assert_eq!((w[0] & 0xffff, w[0] >> 16, w[1]), (VIEWPORTS, 4, 1));
        // SAFETY: what was written just now.
        assert_eq!(unsafe { *at::<[f32; 6]>(&w[1..], 1, 1) }, viewport);
        let w = &w[5..];
        assert_eq!((w[0] & 0xffff, w[0] >> 16), (COMPUTE_CONSTANTS, 4));
        // SAFETY: as above.
        assert_eq!(unsafe { *at::<[u32; 3]>(&w[1..], 2, 3) }, [7, 8, 9]);
        assert!(at::<u32>(&w[1..], 2, 0).is_null());
    }

    /// A view's record is the one at the call, also if its slot is written
    /// again right after.
    #[test]
    fn a_target_is_the_one_at_the_call() {
        let mut slot = [0x5256_5476u64, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let kept = record(slot.as_ptr() as u64);
        slot[1] = 99;
        assert_eq!(kept[1], 2);
        assert_eq!(record(0), [0; RECORD]);
    }
}
