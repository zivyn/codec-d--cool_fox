//! Title memory by address, for translated shaders that read and write
//! through raw 40-bit addresses (`victory_shader::emit::SLOT_MEMORY`: the
//! console's ray-tracing structure builder and the programs that walk its
//! structures; the structures themselves lie in plain title memory, in no
//! buffer object).
//!
//! A page in a title buffer that holds all of it is in that buffer's copy
//! ([`crate::shadow`]), which every other access uses. A page outside
//! buffers is in a window of this module's own, on the card
//! ([`crate::shadow::by_address`]): 4 MiB around what a dispatch names (the
//! title's pool of dynamic structures), 256 KiB around a page a shader came
//! to by a pointer (the streamed structures lie anywhere between other
//! data; `VICTORY_MEMORY_WINDOW_KB`).
//!
//! This module keeps what a shader needs to find the copies:
//!
//! - the **directory**: per 64 KiB page of title address space the number
//!   of the copy that holds the page (0: none) and the page's byte offset
//!   in it; one for all lists, entries only ever go from empty to filled.
//!   The CPU writes it in memory of its own and shaders read a copy of that
//!   on the card, brought up to date as title memory is (over the bus, every
//!   step of every ray waited for it);
//! - per view heap a run of slots with a raw unordered access view of each
//!   numbered copy (slot 0 of the run: a null view, for pages without a
//!   copy);
//! - the **faults**: the page numbers shaders found empty, read back here
//!   once a frame, so the copies exist from the next frame on. A pointer a
//!   shader takes out of memory cannot be known before it runs; the
//!   addresses a dispatch is given as root views are made ready before it.
//!
//! **A page that is not there when a shader touches it is a lost access**:
//! the read gives 0, the store is dropped, and the fault note only helps
//! the next frame. For the structure builder that is the same as a dropped
//! stage, so before a dispatch the host makes ready everything it can see
//! the dispatch name: the window around each root view, around each
//! pair of root constants that is a title address and around each root
//! constant that is one in 256-byte units (the builder passes structures
//! all three ways), and the one after each (a structure may cross a
//! window's end).
//! shortcut: what a stage reaches only through pointers it reads out of
//! memory (the structures a top-level build's instances name) is there from
//! the second frame on; the fault count in the log says how often that
//! happens. The top level is built anew every frame.
//!
//! **What comes back to title memory**: what the builder stores in title
//! buffers of at most [`BACK`] bytes it names (the sizes the title reads to
//! make a structure smaller). Structures and scratch memory stay on the card.
//! Other programs that store by address declare the copies they name.
//!
//! shortcut: a copy that is numbered is kept for the rest of the run, and
//! there are at most [`COPIES`] of them.
//! shortcut: the windows a list does not name are brought up to date once a
//! frame, by the first list that reads by address: bytes the title writes
//! there later in the frame are seen a frame late.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use victory_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES};
use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::host::HostHeap;
use crate::host::{HostList, host};
use crate::shadow::Shadow;

/// Copies that can be numbered (the run of heap slots is one longer). A
/// frame of the dorm names 1,100 pages outside the named windows.
const COPIES: u32 = (1 << 14) - 1;
/// A window around memory a dispatch names.
const CELL: u64 = 4 << 20;
/// A directory page.
const PAGE: u64 = 1 << 16;
/// The biggest title buffer whose builder results come back.
const BACK: u64 = 256 << 10;

/// What a draw or dispatch does with title memory by address.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Use {
    Reads,
    Writes,
    /// A stage of the structure builder; `back`: its results come back.
    Builder { back: bool },
}

struct Memory {
    /// The CPU's entries (number, offset), and the copy shaders read.
    directory: (*mut u32, Arc<Shadow>),
    faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`; whether it is a window of ours.
    copies: Vec<(Arc<Shadow>, bool)>,
    /// Places of title buffers' copies among `copies`.
    numbers: HashMap<usize, usize>,
    /// Per host view heap (by address): the first slot of its run, and how
    /// many copies have their view there.
    heaps: HashMap<usize, (u32, u32)>,
    /// The frame the faults were last read in.
    frame: u64,
    /// The frame our windows were last brought up to date in.
    swept: u64,
    /// Pages asked for this frame that are not the title's memory.
    refused: std::collections::HashSet<u64>,
    stats: Stats,
}

/// Since the last report (diagnostics).
#[derive(Default)]
struct Stats {
    from: Option<(std::time::Instant, u64)>,
    uses: [u64; 3],
    faults: u64,
    back: u64,
}

// SAFETY: the mappings are of memory the struct owns; all access is under
// the mutex.
unsafe impl Send for Memory {}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// What a draw binds.
pub(crate) struct Ready {
    pub(crate) directory: u64,
    pub(crate) faults: u64,
    /// The view heap slot of the null view; copy `n`'s view is `n` further.
    pub(crate) base: u32,
}

/// A window around a page a shader came to by a pointer.
fn far_window() -> u64 {
    static BYTES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| {
        let kb = std::env::var("VICTORY_MEMORY_WINDOW_KB").ok().and_then(|v| v.trim().parse::<u64>().ok());
        kb.map(|kb| kb << 10).filter(|b| b.is_power_of_two() && (PAGE..=CELL).contains(b)).unwrap_or(256 << 10)
    })
}

impl Memory {
    fn new() -> Option<Memory> {
        let host = host()?;
        let bytes = u64::from(MEMORY_PAGES) * 8;
        // Zeroed, and watched for writes as the title's memory is.
        let entries = crate::upload::reserve(bytes as usize)?;
        let directory = crate::shadow::by_address(entries as u64, bytes)?;
        crate::host::name(&directory.buffer, || "title memory by address: directory".into());
        let (faults, data) = host.shared_buffer(u64::from(MEMORY_FAULTS) * 4)?;
        crate::host::name(&faults, || "title memory by address: faults".into());
        // SAFETY: a fresh mapping of at least that many bytes.
        unsafe { std::ptr::write_bytes(data, 0, MEMORY_FAULTS as usize * 4) };
        // SAFETY: a live buffer.
        let gpu = unsafe { faults.GetGPUVirtualAddress() };
        tracing::info!("title memory by address: a shader reads or writes through raw addresses");
        Some(Memory {
            directory: (entries as *mut u32, directory),
            faults: (faults, data.cast::<u32>(), gpu),
            copies: Vec::new(),
            numbers: HashMap::new(),
            heaps: HashMap::new(),
            frame: u64::MAX,
            swept: u64::MAX,
            refused: Default::default(),
            stats: Stats::default(),
        })
    }

    /// The directory's entry of `page`.
    fn entry(&self, page: u64) -> *mut u32 {
        debug_assert!(page < u64::from(MEMORY_PAGES));
        // SAFETY: inside the directory.
        unsafe { self.directory.0.add(page as usize * 2) }
    }

    /// Gives the page of `address` its copy, if it has none and the memory
    /// is the title's (a window of `window` bytes outside buffers); the place
    /// of its copy in `copies`.
    fn map(&mut self, address: u64, window: u64) -> Option<usize> {
        let page = address / PAGE;
        // The last entry stays empty: addresses past the directory read it.
        if page >= u64::from(MEMORY_PAGES) - 1 {
            return None;
        }
        // SAFETY: an entry of the directory.
        let mut number = unsafe { self.entry(page).read() };
        if number == 0 {
            self.make(page, window);
            // SAFETY: as above.
            number = unsafe { self.entry(page).read() };
        }
        (number as usize).checked_sub(1)
    }

    fn make(&mut self, page: u64, window: u64) {
        let start = page * PAGE;
        // A shader's pointer, or a constant taken for an address, may be
        // anything: only memory the title has.
        if self.refused.contains(&page) {
            return;
        }
        if !crate::texture::committed(start as *const u8, PAGE as usize) {
            self.refused.insert(page);
            return;
        }
        // The copy of a title buffer that holds all of the page, the
        // innermost first; else a window of our own.
        let whole = |s: &Arc<Shadow>| s.offset_in(start).is_some() && s.left(start) >= PAGE;
        let buffer = crate::shadow::in_buffer(start, PAGE).and_then(|_| crate::shadow::find(start, PAGE)).filter(whole);
        let own = buffer.is_none();
        let shadow = match buffer {
            Some(s) => s,
            None => {
                if self.copies.len() >= COPIES as usize {
                    static ONCE: std::sync::Once = std::sync::Once::new();
                    ONCE.call_once(|| {
                        tracing::warn!(COPIES, "title memory by address: no number left for another copy; its pages read zeros")
                    });
                    return;
                }
                let Some(s) = crate::shadow::by_address(start & !(window - 1), window) else {
                    return;
                };
                s
            }
        };
        // SAFETY: a live buffer.
        let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
        if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            return;
        }
        let key = Arc::as_ptr(&shadow) as usize;
        let n = match self.numbers.get(&key) {
            Some(&n) => n,
            None if self.copies.len() < COPIES as usize => {
                self.copies.push((shadow.clone(), own));
                let n = self.copies.len() - 1;
                self.numbers.insert(key, n);
                // Every page the copy holds whole, not only the one asked
                // for: a structure is more than 64 KiB. (Not those of title
                // buffers inside a window: they have copies of their own.)
                let (first, len) = shadow.span();
                let mut at = first.next_multiple_of(PAGE);
                while at + PAGE <= first + len && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
                    let entry = self.entry(at / PAGE);
                    // SAFETY: an entry of the directory.
                    if unsafe { entry.read() } == 0 && (!own || crate::shadow::in_buffer(at, PAGE).is_none()) {
                        fill(entry, n, at - first);
                    }
                    at += PAGE;
                }
                n
            }
            None => return,
        };
        fill(self.entry(page), n, shadow.offset_in(start).unwrap_or(0));
    }

    /// The pages shaders found empty since the last frame.
    fn take_faults(&mut self) {
        let mut pages = Vec::new();
        for i in 1..MEMORY_FAULTS as usize {
            // SAFETY: inside the faults' mapping; the GPU may be writing,
            // a number missed now is written again next frame.
            let at = unsafe { self.faults.1.add(i) };
            // SAFETY: as above.
            let page = unsafe { at.read_volatile() };
            if page != 0 {
                pages.push(page);
                // SAFETY: as above.
                unsafe { at.write_volatile(0) };
            }
        }
        if pages.is_empty() {
            return;
        }
        let mut made = 0;
        for &page in &pages {
            made += usize::from(self.map(u64::from(page) * PAGE, far_window()).is_some());
        }
        self.stats.faults += pages.len() as u64;
        // Each is an access that read 0 or was dropped.
        static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 100 {
            pages.sort_unstable();
            let first: Vec<String> = pages.iter().take(8).map(|&p| format!("{:#x}", u64::from(p) * PAGE)).collect();
            tracing::warn!(
                frame = self.frame,
                pages = pages.len(),
                made,
                copies = self.copies.len(),
                ?first,
                "title memory by address: shaders touched pages that had no copy (those accesses read 0 or were dropped); the copies are made now, of those that are the title's"
            );
        }
    }

    /// What a dispatch names: the copy around `address` and the one after it;
    /// their places in `copies`.
    fn name(&mut self, address: u64, named: &mut Vec<usize>) {
        for at in [address, (address & !(CELL - 1)) + CELL] {
            if let Some(n) = self.map(at, CELL)
                && !named.contains(&n)
            {
                named.push(n);
            }
        }
    }

    /// Every ten seconds or so, what was used (with the frame's line).
    fn report(&mut self) {
        let now = std::time::Instant::now();
        let (since, frame) = *self.stats.from.get_or_insert((now, self.frame));
        let seconds = now.duration_since(since).as_secs_f64();
        if seconds < 10.0 {
            return;
        }
        let frames = self.frame.wrapping_sub(frame).max(1) as f64;
        let (mut ours, mut bytes) = (0, 0);
        for (s, own) in &self.copies {
            if *own {
                ours += 1;
                bytes += s.span().1;
            }
        }
        let s = std::mem::take(&mut self.stats);
        tracing::info!(
            "title memory by address: {ours} windows ({} MB) and {} title buffers; per frame {:.0} dispatches that read, {:.0} that write, \
             {:.0} of the structure builder, {:.1} pages that had no copy, {:.1} results brought back",
            bytes >> 20,
            self.copies.len() - ours,
            s.uses[0] as f64 / frames,
            s.uses[1] as f64 / frames,
            s.uses[2] as f64 / frames,
            s.faults as f64 / frames,
            s.back as f64 / frames
        );
        self.stats.from = Some((now, self.frame));
    }
}

/// The offset first: a shader that sees the number sees it too.
fn fill(entry: *mut u32, n: usize, offset: u64) {
    // SAFETY: an entry of the directory, under the mutex.
    unsafe {
        entry.add(1).write_volatile(offset as u32);
        entry.write_volatile(n as u32 + 1);
    }
}

/// Before a draw whose shader reads title memory by address: what its
/// root arguments name gets its copies (`addresses`: its root views;
/// `constants`: its root constants, see the module), every numbered copy
/// has its view in `heap`'s run and is brought up to date when `list` is
/// submitted (what `use_` writes is copied back after, see the module).
/// None if the host cannot hold it; the draw is then dropped.
pub(crate) fn ready(list: &HostList, heap: &HostHeap, addresses: &[u64], constants: &[u32], use_: Use) -> Option<Ready> {
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let m = guard.as_mut()?;
    let frame = crate::frame::current();
    if m.frame != frame {
        m.frame = frame;
        m.refused.clear();
        m.take_faults();
        m.report();
    }
    m.stats.uses[match use_ {
        Use::Reads => 0,
        Use::Writes => 1,
        Use::Builder { .. } => 2,
    }] += 1;
    let numbered = m.copies.len();
    let mut named = Vec::new();
    for &address in addresses {
        m.name(address, &mut named);
    }
    // A title address in two constants (low dword, then bits 32 to 39), or
    // in one in 256-byte units. A constant that only looks like one names
    // memory nothing reads; one that is not the title's memory is refused.
    for (i, &c) in constants.iter().enumerate() {
        if let Some(&high) = constants.get(i + 1)
            && (1..=0xFF).contains(&high)
            && c >= 0x1_0000
        {
            m.name(u64::from(c) | u64::from(high) << 32, &mut named);
        }
        if c >= 0x0100_0000 {
            m.name(u64::from(c) << 8, &mut named);
        }
    }
    let key = std::ptr::from_ref(heap) as usize;
    let (base, made) = match m.heaps.get(&key) {
        Some(&run) => run,
        None => {
            // Held for the heap's life: not given back with a list's slots.
            let base = heap.dynamic(COPIES + 1)?;
            heap.null_raw_uav_at(base);
            (base, 0)
        }
    };
    for (n, (shadow, _)) in m.copies.iter().enumerate().skip(made as usize) {
        let (_, len) = shadow.span();
        heap.raw_uav_at(base + 1 + n as u32, &shadow.buffer, 0, len);
    }
    m.heaps.insert(key, (base, m.copies.len() as u32));
    // Once per list, and for copies numbered since: the builder alone
    // dispatches hundreds of times a frame.
    // shortcut: a copy another list numbers meanwhile is brought up to date
    // by that list's submission, which may run after this one's.
    const READ: usize = usize::MAX - 0x6d;
    const NAMED: usize = usize::MAX - 0x1_0000;
    const WRITE: usize = usize::MAX - 0x2_0000;
    const RESULT: usize = usize::MAX / 2;
    let first_read = list.first_use(READ);
    if first_read {
        let (start, len) = m.directory.1.span();
        m.directory.1.refresh(list, start, len);
        let sweep = m.swept != frame;
        m.swept = frame;
        for (shadow, own) in &m.copies {
            if sweep || !own {
                let (start, len) = shadow.span();
                shadow.refresh(list, start, len);
            }
        }
        list.keep(&m.faults.0);
    } else if m.copies.len() > numbered {
        let (start, len) = m.directory.1.span();
        m.directory.1.refresh(list, start, len);
    }
    for (shadow, _) in &m.copies[numbered..] {
        let (start, len) = shadow.span();
        shadow.refresh(list, start, len);
    }
    for &n in &named {
        let (shadow, own) = &m.copies[n];
        let (start, len) = shadow.span();
        if *own && list.first_use(NAMED - n) {
            shadow.refresh(list, start, len);
        }
        if use_ == Use::Writes && list.first_use(WRITE - n) {
            #[cfg(feature = "diagnostics")]
            crate::host::wide_write(start, len, 1);
            shadow.gpu_writes(list, start, len);
        }
    }
    if use_ == (Use::Builder { back: true }) {
        for &address in addresses {
            let Some((start, len)) = crate::shadow::in_buffer(address, 1).filter(|b| b.1 <= BACK) else {
                continue;
            };
            if !list.first_use(RESULT + start as usize) {
                continue;
            }
            // The copy or copies its pages are in (it need not fill one).
            let mut at = start;
            while at < start + len {
                let end = (at / PAGE + 1) * PAGE;
                let n = end.min(start + len) - at;
                if let Some(k) = m.map(at, CELL) {
                    let shadow = &m.copies[k].0;
                    shadow.refresh(list, at, n);
                    shadow.gpu_writes(list, at, n);
                    m.stats.back += 1;
                }
                at = end;
            }
        }
    }
    let directory = &m.directory.1;
    Some(Ready {
        directory: directory.gpu(directory.span().0),
        faults: m.faults.2,
        base,
    })
}
