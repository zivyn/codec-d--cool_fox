//! The whole-process sampling profiler: `VICTORY_PROFILE=<hz>` (diagnostics build).
//!
//! One thread (`victory-profile`) wakes `<hz>` times a second. For every thread of the process (ours,
//! the title's, the driver's and the system's pools) whose processor cycles moved since it was last
//! seen (`QueryThreadCycleTime`: a thread that waited costs one cheap call), it suspends the thread,
//! reads its context, unwinds the stack with the x64 unwind data of the module each frame is in
//! (`RtlVirtualUnwind`, fed by a table of our own, so no lock of the system's is taken while a thread
//! is stopped), and resumes it. Between the suspend and the resume nothing allocates, logs or takes
//! a lock: the frames go to a buffer made beforehand and are counted after the resume.
//!
//! What is kept is a calling-context tree in memory of a size fixed at the start (`VICTORY_PROFILE_NODES`
//! nodes of 24 bytes and a table of four bytes for two slots of each): a stack is a path from a
//! thread's root, the outermost frame first, and each sample adds one to the node where its path
//! ends (and the processor cycles the thread used since its last sample, so that the weights add up to
//! the thread's real time). When the nodes are all used, a sample counts for the deepest node it
//! reached and `lost` says how many did.
//!
//! The frames are function entries (`module+rva` of the function's first byte) but the innermost, which is
//! the exact instruction, so a symbol of the PDB names the function and a line table the line. A frame
//! with no unwind data ends the walk (code of no module: the walk is its address alone). Markers stand
//! in the path where a sample says more: `[fault]` (the stack passes `KiUserExceptionDispatcher`: a
//! vectored handler of ours or SEH is running), `[syscall]` (the thread is in a system call:
//! it waits, or the kernel works for it), `[spin]` (it stands at a `pause`), `[truncated]` (the stack
//! was deeper than `VICTORY_PROFILE_DEPTH`), `[nostack]`.
//!
//! The files, every `VICTORY_PROFILE_SECS` seconds and at the end (written to a temporary file that
//! then replaces the old one): `<file>` folded stacks (`group;frame;...;frame count`, count is samples;
//! the title's threads also carry their name or start as the second part), `<file>.cycles` the same
//! stacks counted in thousands of cycles, `<file>.txt` the report: the numbers of the run, the modules
//! (with the GUID of their PDB), the threads, the groups and the top self and inclusive functions of each
//! group. `prof-read.py` (in the work folder) reads them against the PDB.

#![cfg_attr(not(feature = "diagnostics"), allow(dead_code))]

use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Wdk::System::Threading::{
    NtQueryInformationThread, ThreadBasicInformation, ThreadQuerySetWin32StartAddress,
};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HMODULE, LocalFree};
use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, GetThreadContext, IMAGE_RUNTIME_FUNCTION_ENTRY, RtlVirtualUnwind, UNW_FLAG_NHANDLER,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleA, GetProcAddress,
};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::ProcessStatus::K32EnumProcessModules;
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentProcess,
    GetCurrentThread, GetCurrentThreadId, GetThreadId, GetThreadTimes, ResumeThread,
    SetThreadPriority, SetWaitableTimer, SuspendThread, THREAD_GET_CONTEXT,
    THREAD_PRIORITY_HIGHEST, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME, TIMER_ALL_ACCESS,
    WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

/// Most frames of a stack the profiler ever keeps.
const MAX_DEPTH: usize = 64;
/// What stands in a path where a sample says more than its frames (no frame has an address this small).
const M_TRUNC: u64 = 1;
const M_FAULT: u64 = 2;
const M_SYSCALL: u64 = 3;
const M_SPIN: u64 = 4;
const M_NOSTACK: u64 = 5;
/// The mark of the node that is a thread's root: this bit and the thread id.
const THREAD_KEY: u64 = 1 << 63;
/// `CONTEXT_AMD64 | CONTEXT_CONTROL | CONTEXT_INTEGER`: what the unwind needs.
const CONTEXT_UNWIND: u32 = 0x0010_0000 | 0x1 | 0x2;
/// `STATUS_NO_MORE_ENTRIES`: the end of the walk of the threads.
const STATUS_NO_MORE_ENTRIES: i32 = 0x8000_001Au32 as i32;
/// The threads and modules are looked up again this often.
const REFRESH: Duration = Duration::from_millis(250);

/// What the environment asked for.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Config {
    /// Rounds a second.
    pub hz: f64,
    /// Seconds between the writes of the files.
    pub every: Duration,
    /// Frames kept of a stack.
    pub depth: usize,
    /// Nodes of the tree.
    pub nodes: usize,
    /// Stacks in the folded files (the heaviest).
    pub lines: usize,
    /// The folded file; the others are this name and `.cycles` and `.txt`.
    pub file: PathBuf,
    /// Seconds sampled and seconds not, alternating, from the start (the profiler's own cost shows
    /// in the census lines of the two).
    pub duty: Option<(Duration, Duration)>,
}

impl Config {
    /// `VICTORY_PROFILE`'s `value` if it is a rate (a plain number); the legacy `from,to,ms,file` list
    /// is not. `var` reads the other `VICTORY_PROFILE_*`, `dir` is where the file goes when the
    /// run has no capture folder (`VICTORY_CAPTURE`) to put it beside the log.
    pub(crate) fn from_env(
        value: &str,
        var: &dyn Fn(&str) -> Option<String>,
        dir: &Path,
    ) -> Option<Config> {
        let hz = value
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|h| h.is_finite() && *h > 0.0)?;
        let number = |name: &str| var(name).and_then(|v| v.trim().parse::<f64>().ok());
        let secs = |s: f64| Duration::try_from_secs_f64(s.max(0.0)).ok();
        let file = match var("VICTORY_PROFILE_FILE").filter(|f| !f.trim().is_empty()) {
            Some(f) => PathBuf::from(f.trim()),
            None => var("VICTORY_CAPTURE")
                .filter(|c| !c.trim().is_empty())
                .and_then(|c| Path::new(c.trim()).parent().map(Path::to_path_buf))
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| dir.to_path_buf())
                .join("profile.folded"),
        };
        let duty = var("VICTORY_PROFILE_DUTY").and_then(|d| {
            let (on, off) = d.split_once(',')?;
            let (on, off) = (
                secs(on.trim().parse().ok()?)?,
                secs(off.trim().parse().ok()?)?,
            );
            (!on.is_zero() && !off.is_zero()).then_some((on, off))
        });
        Some(Config {
            hz: hz.clamp(1.0, 5000.0),
            every: secs(number("VICTORY_PROFILE_SECS").unwrap_or(15.0))?
                .max(Duration::from_secs(1)),
            depth: (number("VICTORY_PROFILE_DEPTH").unwrap_or(32.0) as usize).clamp(2, MAX_DEPTH),
            nodes: (number("VICTORY_PROFILE_NODES").unwrap_or(1_048_576.0) as usize)
                .clamp(4096, 16 << 20),
            lines: (number("VICTORY_PROFILE_LINES").unwrap_or(200_000.0) as usize).max(1000),
            file,
            duty,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The modules and their unwind data.

/// A `RUNTIME_FUNCTION`: the entry of the unwind table (`.pdata`).
#[repr(C)]
#[derive(Clone, Copy)]
struct Rf {
    begin: u32,
    end: u32,
    unwind: u32,
}

struct Module {
    name: String,
    base: usize,
    size: usize,
    /// The address of the unwind table in the image, and its length.
    pdata: usize,
    count: usize,
    stamp: u32,
    guid: String,
    age: u32,
    pdb: String,
}

impl Module {
    fn end(&self) -> usize {
        self.base + self.size
    }

    /// The unwind entry of the function that holds `rva`, in the image (an entry that points to another
    /// is followed once).
    fn entry(&self, rva: u32) -> Option<*const Rf> {
        if self.count == 0 {
            return None;
        }
        // SAFETY: the table of a loaded image, read only while the image stays (the table of modules is
        // looked up again every `REFRESH`, and a module that runs code stays loaded).
        let table = unsafe { std::slice::from_raw_parts(self.pdata as *const Rf, self.count) };
        let at = table.partition_point(|r| r.begin <= rva);
        let found = table.get(at.checked_sub(1)?)?;
        if rva >= found.end {
            return None;
        }
        if found.unwind & 1 == 0 {
            return Some(found as *const Rf);
        }
        let target = (found.unwind & !3) as usize;
        (target + size_of::<Rf>() <= self.size).then(|| (self.base + target) as *const Rf)
    }
}

/// The modules of the process, as they were at the last [`Modules::refresh`], and every one seen before.
#[derive(Default)]
struct Modules {
    all: Vec<Module>,
    ids: HashMap<(usize, usize, u32), usize>,
    /// (start, end, index in `all`) of those loaded now, by start.
    live: Vec<(usize, usize, usize)>,
}

fn read<T: Copy>(address: usize) -> T {
    // SAFETY: callers read inside a mapped image.
    unsafe { (address as *const T).read_unaligned() }
}

/// The fields of a loaded image's headers the profiler needs.
struct Header {
    size: usize,
    stamp: u32,
    pdata: usize,
    count: usize,
    guid: String,
    age: u32,
    pdb: String,
}

fn parse_header(base: usize) -> Option<Header> {
    if read::<u16>(base) != 0x5A4D {
        return None;
    }
    let nt = base + read::<u32>(base + 0x3C) as usize;
    if read::<u32>(nt) != 0x0000_4550 {
        return None;
    }
    let stamp = read::<u32>(nt + 8);
    let optional = nt + 24;
    if read::<u16>(optional) != 0x20B {
        return None;
    }
    let size = read::<u32>(optional + 56) as usize;
    let directory = |i: usize| {
        (
            read::<u32>(optional + 112 + 8 * i),
            read::<u32>(optional + 116 + 8 * i),
        )
    };
    let (exception, exception_len) = directory(3);
    let (count, pdata) = if exception != 0 && (exception as usize) < size {
        (
            (exception_len as usize / size_of::<Rf>())
                .min((size - exception as usize) / size_of::<Rf>()),
            base + exception as usize,
        )
    } else {
        (0, 0)
    };
    let (mut guid, mut age, mut pdb) = (String::new(), 0, String::new());
    let (debug, debug_len) = directory(6);
    if debug != 0 && (debug as usize) < size {
        for i in 0..(debug_len as usize / 28).min(16) {
            let entry = base + debug as usize + 28 * i;
            let (kind, raw) = (read::<u32>(entry + 12), read::<u32>(entry + 20) as usize);
            if kind == 2 && raw != 0 && raw + 24 <= size && read::<u32>(base + raw) == 0x5344_5352 {
                let g = base + raw + 4;
                let d4: Vec<String> = (0..8)
                    .map(|k| format!("{:02X}", read::<u8>(g + 8 + k)))
                    .collect();
                guid = format!(
                    "{:08X}{:04X}{:04X}{}",
                    read::<u32>(g),
                    read::<u16>(g + 4),
                    read::<u16>(g + 6),
                    d4.concat()
                );
                age = read::<u32>(g + 16);
                let path: Vec<u8> = (0..260.min(size - raw - 24))
                    .map(|k| read::<u8>(base + raw + 24 + k))
                    .take_while(|&b| b != 0)
                    .collect();
                pdb = String::from_utf8_lossy(&path)
                    .rsplit(['\\', '/'])
                    .next()
                    .unwrap_or("")
                    .to_string();
                break;
            }
        }
    }
    Some(Header {
        size,
        stamp,
        pdata,
        count,
        guid,
        age,
        pdb,
    })
}

fn module_file_name(module: HMODULE) -> String {
    let mut path = [0u16; 520];
    // SAFETY: a module of this process and a buffer of the length given.
    let len = unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32) } as usize;
    let path = String::from_utf16_lossy(&path[..len.min(path.len())]);
    path.rsplit(['\\', '/']).next().unwrap_or("").to_string()
}

fn enum_modules() -> Vec<HMODULE> {
    let mut modules: Vec<HMODULE> = vec![null_mut(); 512];
    loop {
        let mut needed = 0u32;
        // SAFETY: the buffer and its size in bytes are given; this process's pseudo handle.
        let ok = unsafe {
            K32EnumProcessModules(
                GetCurrentProcess(),
                modules.as_mut_ptr(),
                (modules.len() * size_of::<HMODULE>()) as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            return Vec::new();
        }
        let n = needed as usize / size_of::<HMODULE>();
        if n <= modules.len() {
            modules.truncate(n);
            return modules;
        }
        modules.resize(n + 64, null_mut());
    }
}

impl Modules {
    fn refresh(&mut self) {
        self.live.clear();
        for module in enum_modules() {
            let base = module as usize;
            let Some(h) = parse_header(base) else {
                continue;
            };
            let key = (base, h.size, h.stamp);
            let index = match self.ids.get(&key) {
                Some(&i) => i,
                None => {
                    self.all.push(Module {
                        name: module_file_name(module),
                        base,
                        size: h.size,
                        pdata: h.pdata,
                        count: h.count,
                        stamp: h.stamp,
                        guid: h.guid,
                        age: h.age,
                        pdb: h.pdb,
                    });
                    self.ids.insert(key, self.all.len() - 1);
                    self.all.len() - 1
                }
            };
            self.live.push((base, base + h.size, index));
        }
        self.live.sort_unstable();
    }

    /// The module loaded now that holds `address`.
    fn find(&self, address: usize) -> Option<&Module> {
        let at = self.live.partition_point(|l| l.0 <= address);
        let live = self.live.get(at.checked_sub(1)?)?;
        (address < live.1).then(|| &self.all[live.2])
    }

    /// The module and unwind entry of the function that holds `address`.
    fn entry(&self, address: usize) -> Option<(&Module, *const Rf)> {
        let module = self.find(address)?;
        Some((module, module.entry((address - module.base) as u32)?))
    }

    /// The address of the first byte of the function that holds `address`, if the unwind data knows it.
    fn begin_of(&self, address: usize) -> Option<usize> {
        let (module, entry) = self.entry(address)?;
        // SAFETY: an entry of the image's table.
        Some(module.base + unsafe { (*entry).begin } as usize)
    }

    /// `module+0xrva` of an address in any module seen, or the bare address.
    fn label(&self, address: usize) -> String {
        match self
            .all
            .iter()
            .rev()
            .find(|m| address >= m.base && address < m.end())
        {
            Some(m) => format!("{}+{:#x}", m.name, address - m.base),
            None => format!("{address:#x}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The walk of a stopped thread's stack.

/// What the walk of one stack found.
#[derive(Default, Debug)]
struct Walked {
    /// Frames put in the buffer, the innermost first.
    n: usize,
    /// The stack passes `KiUserExceptionDispatcher`.
    fault: bool,
    /// The stack was cut at the depth asked.
    truncated: bool,
    /// The instruction before the one the thread is at is `syscall`.
    syscall: bool,
    /// The thread is at a `pause` or just after one.
    spin: bool,
}

fn register_of(context: &CONTEXT, i: u8) -> u64 {
    // SAFETY: `Rax` to `R15` are 16 consecutive u64 fields of the context, in the order of the registers'
    // numbers in the unwind data.
    unsafe { (&raw const context.Rax).add(usize::from(i)).read() }
}

/// Walks the stack of the thread whose context is `context` (`low..high` being its stack), putting
/// at most `depth` frames in `out`: the instruction it is at first, then the function entry of each caller.
/// Nothing in it allocates or takes a lock.
///
/// # Safety
/// `context` must be a context of a thread that cannot run meanwhile (suspended, or the caller's own),
/// `low..high` the bounds of its stack, `modules` a table that is not changed meanwhile.
unsafe fn walk(
    modules: &Modules,
    context: &mut CONTEXT,
    (low, high): (usize, usize),
    depth: usize,
    kiuser: usize,
    out: &mut [u64; MAX_DEPTH],
) -> Walked {
    let mut walked = Walked::default();
    let depth = depth.min(MAX_DEPTH);
    let rip = context.Rip as usize;
    // The bytes around the instruction the thread is at: a `syscall` just before it (the thread is in
    // the system, its return address is this), a `pause` at it or just before it (the wait loops of the
    // title are `pause` and a nop where the console had `mwaitx`), or a wait loop the console-only
    // instructions noted.
    if modules
        .find(rip)
        .is_some_and(|m| rip >= m.base + 3 && rip + 2 <= m.end())
    {
        let (before, at) = (read::<u32>(rip - 3).to_le_bytes(), read::<u16>(rip));
        walked.syscall = before[1] == 0x0F && before[2] == 0x05;
        walked.spin = (before[0] == 0xF3 && before[1] == 0x90)
            || (before[1] == 0xF3 && before[2] == 0x90)
            || at == 0x90F3
            || crate::console_insn::wait_site_near(rip).is_some();
    }
    loop {
        let pc = context.Rip as usize;
        let leaf = walked.n == 0;
        let found = modules.entry(if leaf { pc } else { pc.wrapping_sub(1) });
        out[walked.n] = match found {
            Some((m, e)) if !leaf => (m.base + read::<u32>(e as usize) as usize) as u64,
            _ => pc as u64,
        };
        walked.n += 1;
        if let Some((m, e)) = found {
            if kiuser != 0 && m.base + read::<u32>(e as usize) as usize == kiuser {
                walked.fault = true;
            }
        }
        if walked.n >= depth {
            walked.truncated = true;
            break;
        }
        let rsp = context.Rsp as usize;
        if rsp < low || rsp + 8 > high {
            break;
        }
        match found {
            Some((m, e)) => {
                // A frame register that is not on the stack (the thread stands in a prologue, or the data is
                // not of this frame) would make the unwind read where nothing is mapped.
                let info = m.base + (read::<u32>(e as usize + 8) & !3) as usize;
                if info + 4 > m.end() {
                    break;
                }
                let frame_register = read::<u8>(info + 3) & 0x0F;
                if frame_register != 0 {
                    let value = register_of(context, frame_register) as usize;
                    if value < low || value > high {
                        break;
                    }
                }
                let (mut data, mut frame) = (null_mut(), 0u64);
                // SAFETY: the entry is of the image `m.base`; the context is the caller's to change; the
                // stack it reads is checked above to be the thread's.
                unsafe {
                    RtlVirtualUnwind(
                        UNW_FLAG_NHANDLER,
                        m.base as u64,
                        pc as u64,
                        e.cast::<IMAGE_RUNTIME_FUNCTION_ENTRY>(),
                        context,
                        &mut data,
                        &mut frame,
                        null_mut(),
                    )
                };
            }
            // A function with no unwind data that the thread is in is a leaf: the return address is on top.
            None if leaf && modules.find(pc).is_some() => {
                context.Rip = read::<u64>(rsp);
                context.Rsp += 8;
            }
            None => break,
        }
        if context.Rip == 0 || (context.Rsp as usize) <= rsp {
            break;
        }
    }
    walked
}

// ---------------------------------------------------------------------------------------------
// The tree of stacks.

#[repr(C)]
#[derive(Clone, Copy)]
struct Node {
    addr: u64,
    parent: u32,
    samples: u32,
    cycles: u64,
}

struct Tree {
    nodes: Vec<Node>,
    /// Open addressing by (parent, address): the index of the node plus none for empty, zero is empty.
    slots: Vec<u32>,
    mask: usize,
    cap: usize,
    /// Samples that ended at a node short of their end, for want of room.
    lost: u64,
}

impl Tree {
    fn new(cap: usize) -> Tree {
        let slots = (cap * 2).next_power_of_two();
        let mut nodes = Vec::with_capacity(cap);
        nodes.push(Node {
            addr: 0,
            parent: 0,
            samples: 0,
            cycles: 0,
        });
        Tree {
            nodes,
            slots: vec![0; slots],
            mask: slots - 1,
            cap,
            lost: 0,
        }
    }

    /// The child of `parent` that stands for `addr`, made if there is room.
    fn child(&mut self, parent: u32, addr: u64) -> Option<u32> {
        let mut h = u64::from(parent).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ addr.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        h ^= h >> 32;
        let mut i = h as usize & self.mask;
        loop {
            let s = self.slots[i];
            if s == 0 {
                break;
            }
            let n = &self.nodes[s as usize];
            if n.parent == parent && n.addr == addr {
                return Some(s);
            }
            i = (i + 1) & self.mask;
        }
        if self.nodes.len() >= self.cap {
            return None;
        }
        let id = self.nodes.len() as u32;
        self.nodes.push(Node {
            addr,
            parent,
            samples: 0,
            cycles: 0,
        });
        self.slots[i] = id;
        Some(id)
    }

    /// Counts a sample: `frames` are the stack as the walk made it (innermost first); the path from
    /// `root` is the markers, then the frames outermost first, then the mark of the instruction.
    fn record(&mut self, root: u32, walked: &Walked, frames: &[u64], cycles: u64) {
        let mut path = [0u64; MAX_DEPTH + 4];
        let mut n = 0;
        let mut push = |a: u64| {
            path[n] = a;
            n += 1;
        };
        if walked.n == 0 {
            push(M_NOSTACK);
        }
        if walked.truncated {
            push(M_TRUNC);
        }
        if walked.fault {
            push(M_FAULT);
        }
        for &f in frames[..walked.n].iter().rev() {
            push(f);
        }
        if walked.syscall {
            push(M_SYSCALL);
        } else if walked.spin {
            push(M_SPIN);
        }
        let mut at = root;
        for &a in &path[..n] {
            match self.child(at, a) {
                Some(c) => at = c,
                None => {
                    self.lost += 1;
                    break;
                }
            }
        }
        let node = &mut self.nodes[at as usize];
        node.samples = node.samples.saturating_add(1);
        node.cycles = node.cycles.saturating_add(cycles);
    }
}

// ---------------------------------------------------------------------------------------------
// The threads.

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Kind {
    Title,
    Ours,
    Other,
}

impl Kind {
    fn text(self) -> &'static str {
        match self {
            Kind::Title => "title",
            Kind::Ours => "ours",
            Kind::Other => "other",
        }
    }
}

/// The group of a thread, as the census names them (`threadcpu::census`): the title's threads together;
/// ours and any named thread by the name without its number; the rest by the module they start in.
fn group_of(name: &str, module: &str, exe: &str) -> (String, Kind) {
    let module = module.to_ascii_lowercase();
    let stem = module
        .strip_suffix(".dll")
        .or_else(|| module.strip_suffix(".exe"))
        .unwrap_or(&module);
    if stem.starts_with("victory-image") {
        return ("title".into(), Kind::Title);
    }
    let kind = if !exe.is_empty() && module == exe.to_ascii_lowercase() {
        Kind::Ours
    } else {
        Kind::Other
    };
    let base = name
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end_matches(['-', ' ', '_']);
    if !base.is_empty() {
        return (base.to_string(), kind);
    }
    if !name.is_empty() {
        return (name.to_string(), kind);
    }
    match (kind, stem.is_empty()) {
        (Kind::Ours, _) => ("ours (no name)".into(), kind),
        (_, true) => ("?".into(), kind),
        _ => (stem.to_string(), kind),
    }
}

struct Thr {
    tid: u32,
    handle: HANDLE,
    teb: usize,
    name: String,
    start: String,
    group: String,
    kind: Kind,
    root: u32,
    /// The cycles at the last sample (or at the start of the window), and what the samples carried.
    cycles_last: u64,
    cycles_sampled: u64,
    idle: u64,
    samples: u64,
    phases: [u64; 4],
    /// Processor time, 100 ns, in the windows sampled; and the counters at the last look.
    user: u64,
    kernel: u64,
    user_last: u64,
    kernel_last: u64,
    alive: bool,
}

fn ticks(f: &FILETIME) -> u64 {
    u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime)
}

/// (creation, user, kernel) of a thread, 100 ns units; creation is since 1601.
fn times_of(thread: HANDLE) -> (u64, u64, u64) {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: plain out structures and a thread handle.
    unsafe { GetThreadTimes(thread, &mut created, &mut exited, &mut kernel, &mut user) };
    (ticks(&created), ticks(&user), ticks(&kernel))
}

fn cycles_of(thread: HANDLE) -> Option<u64> {
    let mut cycles = 0u64;
    // SAFETY: a thread handle with query access, one out value.
    (unsafe { QueryThreadCycleTime(thread, &mut cycles) } != 0).then_some(cycles)
}

fn name_of(thread: HANDLE) -> String {
    let mut text = null_mut();
    // SAFETY: the system allocates the string; it is freed below.
    unsafe {
        if windows_sys::Win32::System::Threading::GetThreadDescription(thread, &mut text) < 0
            || text.is_null()
        {
            return String::new();
        }
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        let name = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast::<c_void>());
        name
    }
}

/// `THREAD_BASIC_INFORMATION`.
#[repr(C)]
struct BasicInfo {
    exit_status: i32,
    teb: usize,
    client_id: [usize; 2],
    affinity: usize,
    priority: i32,
    base_priority: i32,
}

fn start_of(thread: HANDLE) -> usize {
    let mut start = 0usize;
    // SAFETY: writes one pointer-sized start address.
    unsafe {
        NtQueryInformationThread(
            thread,
            ThreadQuerySetWin32StartAddress,
            (&raw mut start).cast(),
            size_of::<usize>() as u32,
            null_mut(),
        )
    };
    start
}

fn teb_of(thread: HANDLE) -> usize {
    let mut info = BasicInfo {
        exit_status: 0,
        teb: 0,
        client_id: [0; 2],
        affinity: 0,
        priority: 0,
        base_priority: 0,
    };
    // SAFETY: writes `info`, whose layout and size are the system's.
    let status = unsafe {
        NtQueryInformationThread(
            thread,
            ThreadBasicInformation,
            (&raw mut info).cast(),
            size_of::<BasicInfo>() as u32,
            null_mut(),
        )
    };
    if status == 0 { info.teb } else { 0 }
}

/// `NtGetNextThread(process, thread, access, attributes, flags, &next)`.
type GetNextThread = unsafe extern "system" fn(HANDLE, HANDLE, u32, u32, u32, *mut HANDLE) -> i32;

fn get_next_thread() -> Option<GetNextThread> {
    // SAFETY: plain lookups in a module every process has; the function has the signature above.
    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        if ntdll.is_null() {
            return None;
        }
        GetProcAddress(ntdll, c"NtGetNextThread".as_ptr().cast())
            .map(|f| std::mem::transmute::<unsafe extern "system" fn() -> isize, GetNextThread>(f))
    }
}

fn counter() -> i64 {
    let mut now = 0i64;
    // SAFETY: one out value.
    unsafe { QueryPerformanceCounter(&mut now) };
    now
}

fn frequency() -> i64 {
    static FREQ: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 1i64;
        // SAFETY: one out value.
        unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    })
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// A `CONTEXT` at the alignment `GetThreadContext` needs.
#[repr(C, align(16))]
struct Aligned(CONTEXT);

/// The numbers of the run.
#[derive(Default)]
struct Stats {
    rounds: u64,
    late: u64,
    samples: u64,
    idle: u64,
    suspends: u64,
    suspend_ticks: i64,
    suspend_max_ticks: i64,
    writes: u64,
    write_ms: f64,
    refreshes: u64,
    high_resolution: bool,
}

pub(crate) struct State {
    cfg: Config,
    phase: fn() -> u32,
    me: u32,
    exe: String,
    next_thread: GetNextThread,
    modules: Modules,
    tree: Tree,
    threads: Vec<Thr>,
    by_tid: HashMap<u32, usize>,
    context: Box<Aligned>,
    frames: [u64; MAX_DEPTH],
    kiuser: usize,
    /// When the profile began, as FILETIME ticks and Unix ms.
    start_file_time: u64,
    start_ms: u128,
    started: Instant,
    stats: Stats,
    /// Whether the window of sampling is open, and the windows done: (from, to) Unix ms.
    on: bool,
    window_from: u128,
    windows: Vec<(u128, u128)>,
}

impl State {
    pub(crate) fn new(cfg: Config, phase: fn() -> u32) -> Result<State, String> {
        let next_thread = get_next_thread().ok_or("this system has no NtGetNextThread")?;
        let unix = unix_ms();
        // SAFETY: a plain query of the calling thread; a zeroed context is valid.
        let (me, context) = unsafe { (GetCurrentThreadId(), std::mem::zeroed()) };
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let mut state = State {
            tree: Tree::new(cfg.nodes),
            cfg,
            phase,
            me,
            exe,
            next_thread,
            modules: Modules::default(),
            threads: Vec::new(),
            by_tid: HashMap::new(),
            context: Box::new(Aligned(context)),
            frames: [0; MAX_DEPTH],
            kiuser: 0,
            // 100 ns ticks from 1601 to 1970, and the time now.
            start_file_time: (unix as u64) * 10_000 + 116_444_736_000_000_000,
            start_ms: unix,
            started: Instant::now(),
            stats: Stats::default(),
            on: false,
            window_from: unix,
            windows: Vec::new(),
        };
        state.modules.refresh();
        // SAFETY: plain lookups in a module every process has.
        let address = unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            GetProcAddress(ntdll, c"KiUserExceptionDispatcher".as_ptr().cast())
                .map_or(0, |f| f as usize)
        };
        state.kiuser = state.modules.begin_of(address).unwrap_or(address);
        state.refresh_threads();
        state.begin_window();
        Ok(state)
    }

    /// The modules and the threads, as they are now.
    pub(crate) fn refresh(&mut self) {
        self.modules.refresh();
        self.refresh_threads();
        self.stats.refreshes += 1;
    }

    fn refresh_threads(&mut self) {
        // SAFETY: the pseudo handle of this process.
        let process = unsafe { GetCurrentProcess() };
        let access = THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION;
        let mut seen: Vec<u32> = Vec::with_capacity(self.threads.len() + 8);
        let mut last: HANDLE = null_mut();
        let mut kept = false;
        // Whether the walk ran to the end: only then is a thread not seen one that ended.
        let mut complete = false;
        loop {
            let mut next: HANDLE = null_mut();
            // SAFETY: `last` is null or a thread handle of ours from the call before; `next` is written.
            let status = unsafe { (self.next_thread)(process, last, access, 0, 0, &mut next) };
            if !last.is_null() && !kept {
                // SAFETY: opened by the call before and not kept.
                unsafe { CloseHandle(last) };
            }
            if status < 0 || next.is_null() {
                complete = status == STATUS_NO_MORE_ENTRIES;
                break;
            }
            // SAFETY: a thread handle of ours.
            let tid = unsafe { GetThreadId(next) };
            seen.push(tid);
            // (A thread id that comes back after its thread ended is a new thread.)
            let known = tid == self.me
                || self
                    .by_tid
                    .get(&tid)
                    .is_some_and(|&i| self.threads[i].alive);
            kept = !known;
            if kept {
                self.register(next, tid);
            }
            last = next;
        }
        for t in self.threads.iter_mut().filter(|t| t.alive) {
            if seen.contains(&t.tid) || !complete {
                // A thread's name can come after it started.
                let name = name_of(t.handle);
                if name != t.name {
                    t.name = name;
                    let (group, kind) =
                        group_of(&t.name, &module_name_of(&self.modules, &t.start), &self.exe);
                    (t.group, t.kind) = (group, kind);
                }
            } else {
                // Ended: what it ran to the end is read from the handle that is still open.
                let (_, user, kernel) = times_of(t.handle);
                if self.on {
                    t.user += user.saturating_sub(t.user_last);
                    t.kernel += kernel.saturating_sub(t.kernel_last);
                }
                t.alive = false;
                // SAFETY: opened by the walk and kept.
                unsafe { CloseHandle(t.handle) };
                t.handle = null_mut();
            }
        }
    }

    /// Adds a thread seen for the first time; its handle is kept.
    fn register(&mut self, handle: HANDLE, tid: u32) {
        let name = name_of(handle);
        let start_address = start_of(handle);
        let start = self.modules.label(start_address);
        let (group, kind) = group_of(&name, &module_name_of(&self.modules, &start), &self.exe);
        let (created, user, kernel) = times_of(handle);
        // A thread that began after the profile did has all it has run counted, one that was running does
        // from now on.
        let after = created >= self.start_file_time;
        let cycles = cycles_of(handle).unwrap_or(0);
        let root = self.tree.child(0, THREAD_KEY | u64::from(tid)).unwrap_or(0);
        self.by_tid.insert(tid, self.threads.len());
        self.threads.push(Thr {
            tid,
            handle,
            teb: teb_of(handle),
            name,
            start,
            group,
            kind,
            root,
            cycles_last: if after && self.on { 0 } else { cycles },
            cycles_sampled: 0,
            idle: 0,
            samples: 0,
            phases: [0; 4],
            user: 0,
            kernel: 0,
            user_last: if after { 0 } else { user },
            kernel_last: if after { 0 } else { kernel },
            alive: true,
        });
    }

    /// Adds the processor time the threads ran since the last look to the windows, if one is open.
    fn accumulate(&mut self) {
        for t in self.threads.iter_mut().filter(|t| t.alive) {
            let (_, user, kernel) = times_of(t.handle);
            if self.on {
                t.user += user.saturating_sub(t.user_last);
                t.kernel += kernel.saturating_sub(t.kernel_last);
            }
            t.user_last = user;
            t.kernel_last = kernel;
        }
    }

    /// Opens a window of sampling: what the threads ran before it is not counted.
    fn begin_window(&mut self) {
        if self.on {
            return;
        }
        self.accumulate();
        for t in self.threads.iter_mut().filter(|t| t.alive) {
            if let Some(c) = cycles_of(t.handle) {
                t.cycles_last = c;
            }
        }
        self.on = true;
        self.window_from = unix_ms();
    }

    fn end_window(&mut self) {
        if !self.on {
            return;
        }
        self.accumulate();
        self.on = false;
        self.windows.push((self.window_from, unix_ms()));
    }

    /// One round: every thread that ran since it was last seen, sampled. `force` samples the others too.
    pub(crate) fn sample_round(&mut self, force: bool) {
        let phase = ((self.phase)() as usize).min(3);
        for i in 0..self.threads.len() {
            self.sample(i, force, phase);
        }
        self.stats.rounds += 1;
    }

    fn sample(&mut self, i: usize, force: bool, phase: usize) {
        let (handle, teb, root) = {
            let t = &self.threads[i];
            if !t.alive || t.tid == self.me || t.handle.is_null() {
                return;
            }
            (t.handle, t.teb, t.root)
        };
        let Some(cycles) = cycles_of(handle) else {
            return;
        };
        let last = self.threads[i].cycles_last;
        if cycles == last && !force {
            self.threads[i].idle += 1;
            self.stats.idle += 1;
            return;
        }
        let started = counter();
        // SAFETY: a thread of this process, which this one stops, reads and lets go; between the stop and the
        // start nothing here allocates or takes a lock: the context, the buffer of frames and the table of
        // modules were made before, and the walk reads only the image and the stack of the thread.
        let walked = unsafe {
            if SuspendThread(handle) == u32::MAX {
                return;
            }
            self.context.0.ContextFlags = CONTEXT_UNWIND;
            let walked = if GetThreadContext(handle, &mut self.context.0) != 0 {
                let (high, low) = if teb != 0 {
                    (read::<usize>(teb + 8), read::<usize>(teb + 0x10))
                } else {
                    (0, 0)
                };
                walk(
                    &self.modules,
                    &mut self.context.0,
                    (low, high),
                    self.cfg.depth,
                    self.kiuser,
                    &mut self.frames,
                )
            } else {
                Walked::default()
            };
            ResumeThread(handle);
            walked
        };
        let took = counter() - started;
        self.stats.suspends += 1;
        self.stats.suspend_ticks += took;
        self.stats.suspend_max_ticks = self.stats.suspend_max_ticks.max(took);
        let weight = cycles.saturating_sub(last);
        self.tree.record(root, &walked, &self.frames, weight);
        let t = &mut self.threads[i];
        t.cycles_last = cycles;
        t.cycles_sampled += weight;
        t.samples += 1;
        t.phases[phase] += 1;
        self.stats.samples += 1;
    }

    // -----------------------------------------------------------------------------------------
    // The files.

    /// Writes the three files (to a temporary name that then replaces them).
    pub(crate) fn write(&mut self) -> std::io::Result<()> {
        let began = Instant::now();
        self.accumulate();
        let (folded, cycles, text) = self.render();
        if let Some(dir) = self.cfg.file.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let names = [
            self.cfg.file.clone(),
            with_suffix(&self.cfg.file, ".cycles"),
            with_suffix(&self.cfg.file, ".txt"),
        ];
        for (name, body) in names.iter().zip([&folded, &cycles, &text]) {
            let temp = with_suffix(name, ".tmp");
            std::fs::write(&temp, body)?;
            std::fs::rename(&temp, name)?;
        }
        self.stats.writes += 1;
        self.stats.write_ms = began.elapsed().as_secs_f64() * 1e3;
        Ok(())
    }

    /// The thread whose root heads the path of node `i`, and the path (outermost first, root left out).
    fn path_of(&self, mut i: u32, path: &mut Vec<u64>) -> Option<usize> {
        path.clear();
        for _ in 0..4 * MAX_DEPTH {
            let n = self.tree.nodes.get(i as usize)?;
            if n.addr & THREAD_KEY != 0 {
                path.reverse();
                return self.by_tid.get(&((n.addr & 0xFFFF_FFFF) as u32)).copied();
            }
            path.push(n.addr);
            i = n.parent;
        }
        None
    }

    fn label(&self, cache: &mut HashMap<u64, String>, addr: u64) -> String {
        cache
            .entry(addr)
            .or_insert_with(|| match addr {
                M_TRUNC => "[truncated]".into(),
                M_FAULT => "[fault]".into(),
                M_SYSCALL => "[syscall]".into(),
                M_SPIN => "[spin]".into(),
                M_NOSTACK => "[nostack]".into(),
                a => self.modules.label(a as usize),
            })
            .clone()
    }

    /// The folded stacks, the same counted in thousands of cycles, and the report.
    fn render(&self) -> (String, String, String) {
        let mut ids: Vec<u32> = (1..self.tree.nodes.len() as u32)
            .filter(|&i| self.tree.nodes[i as usize].samples > 0)
            .collect();
        ids.sort_unstable_by_key(|&i| std::cmp::Reverse(self.tree.nodes[i as usize].cycles));
        let mut labels: HashMap<u64, String> = HashMap::new();
        let mut functions: HashMap<u64, u64> = HashMap::new();
        let mut groups: Vec<GroupAgg> = Vec::new();
        let mut group_at: HashMap<(String, Kind), usize> = HashMap::new();
        let (mut folded, mut cycles) = (String::new(), String::new());
        let mut path = Vec::with_capacity(MAX_DEPTH + 4);
        let mut dropped = (0u64, 0u64);
        for (rank, &i) in ids.iter().enumerate() {
            let node = self.tree.nodes[i as usize];
            let Some(t) = self.path_of(i, &mut path).map(|t| &self.threads[t]) else {
                continue;
            };
            let gi = *group_at
                .entry((t.group.clone(), t.kind))
                .or_insert_with(|| {
                    groups.push(GroupAgg::new(&t.group, t.kind));
                    groups.len() - 1
                });
            let group = &mut groups[gi];
            group.samples += u64::from(node.samples);
            group.cycles += node.cycles;
            // The function that is the frame of the sample (the last that is not a marker), and the keys
            // of all the functions on the path, each once.
            let mut keys: Vec<u64> = Vec::with_capacity(path.len());
            let mut inner = None;
            for &a in &path {
                match a {
                    M_FAULT => group.fault += u64::from(node.samples),
                    M_SYSCALL => group.syscall += u64::from(node.samples),
                    M_SPIN => group.spin += u64::from(node.samples),
                    M_TRUNC | M_NOSTACK => {}
                    a => {
                        let key = *functions.entry(a).or_insert_with(|| {
                            self.modules.begin_of(a as usize).map_or(a, |b| b as u64)
                        });
                        if !keys.contains(&key) {
                            keys.push(key);
                        }
                        inner = Some(key);
                    }
                }
            }
            let flag = path
                .iter()
                .rev()
                .find(|a| matches!(**a, M_SYSCALL | M_SPIN))
                .copied()
                .unwrap_or(0);
            if let Some(key) = inner {
                group.add_self((key, flag), node.samples, node.cycles);
            }
            for key in keys {
                group.add_inclusive(key, node.samples, node.cycles);
            }
            if rank < self.cfg.lines {
                folded.push_str(&t.group.replace(';', ","));
                cycles.push_str(&t.group.replace(';', ","));
                if t.kind == Kind::Title {
                    let who = if t.name.is_empty() { &t.start } else { &t.name };
                    let part = format!(";{}", who.replace(';', ","));
                    folded.push_str(&part);
                    cycles.push_str(&part);
                }
                for &a in &path {
                    let part = format!(";{}", self.label(&mut labels, a));
                    folded.push_str(&part);
                    cycles.push_str(&part);
                }
                let _ = writeln!(folded, " {}", node.samples);
                let _ = writeln!(cycles, " {}", node.cycles / 1000);
            } else {
                dropped.0 += 1;
                dropped.1 += u64::from(node.samples);
            }
        }
        let report = self.report(&groups, &mut labels, dropped, ids.len());
        (folded, cycles, report)
    }

    fn report(
        &self,
        groups: &[GroupAgg],
        labels: &mut HashMap<u64, String>,
        dropped: (u64, u64),
        stacks: usize,
    ) -> String {
        let mut out = String::new();
        let s = &self.stats;
        let elapsed = self.started.elapsed();
        let end_ms = unix_ms();
        let (own_user, own_kernel) = {
            // SAFETY: the pseudo handle of this thread.
            let (_, u, k) = times_of(unsafe { GetCurrentThread() });
            (u / 10_000, k / 10_000)
        };
        let freq = frequency() as f64;
        let _ = writeln!(out, "victory-profile 1");
        let _ = writeln!(
            out,
            "start_unix_ms\t{}\nend_unix_ms\t{end_ms}\nelapsed_ms\t{}",
            self.start_ms,
            elapsed.as_millis()
        );
        let _ = writeln!(
            out,
            "hz_asked\t{}\nrounds\t{}\nlate_rounds\t{}\nsamples\t{}\nidle_skips\t{}\nlost_samples\t{}\nstacks\t{stacks}",
            self.cfg.hz, s.rounds, s.late, s.samples, s.idle, self.tree.lost
        );
        let _ = writeln!(
            out,
            "suspends\t{}\nsuspend_us_total\t{:.0}\nsuspend_us_max\t{:.0}",
            s.suspends,
            s.suspend_ticks as f64 * 1e6 / freq,
            s.suspend_max_ticks as f64 * 1e6 / freq
        );
        let _ = writeln!(out, "profiler_cpu_ms\tuser {own_user}\tkernel {own_kernel}");
        let _ = writeln!(
            out,
            "tree_nodes\t{}\t{}\nfolded_lines_dropped\t{}\t{}\nwrites\t{}\tlast_write_ms {:.0}\ntimer\t{}\ndepth\t{}",
            self.tree.nodes.len(),
            self.tree.cap,
            dropped.0,
            dropped.1,
            s.writes,
            s.write_ms,
            if s.high_resolution { "high" } else { "low" },
            self.cfg.depth
        );
        let mut windows = self.windows.clone();
        if self.on {
            windows.push((self.window_from, end_ms));
        }
        for (from, to) in windows {
            let _ = writeln!(out, "window\t{from}\t{to}");
        }
        for m in &self.modules.all {
            let _ = writeln!(
                out,
                "module\t{}\t{:#x}\t{:#x}\t{:08x}\t{}\t{}\t{}",
                m.name, m.base, m.size, m.stamp, m.guid, m.age, m.pdb
            );
        }
        for t in &self.threads {
            let _ = writeln!(
                out,
                "thread\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                t.tid,
                t.group,
                t.kind.text(),
                if t.name.is_empty() { "-" } else { &t.name },
                t.start,
                t.user / 10_000,
                t.kernel / 10_000,
                t.cycles_sampled,
                t.samples,
                t.idle,
                t.phases[0],
                t.phases[1],
                t.phases[2],
                t.phases[3]
            );
        }
        for g in groups.iter() {
            let mut user = 0;
            let mut kernel = 0;
            let mut count = 0;
            for t in self
                .threads
                .iter()
                .filter(|t| t.group == g.name && t.kind == g.kind)
            {
                user += t.user / 10_000;
                kernel += t.kernel / 10_000;
                count += 1;
            }
            let _ = writeln!(
                out,
                "group\t{}\t{}\t{count}\t{user}\t{kernel}\t{}\t{}\t{}\t{}\t{}",
                g.name,
                g.kind.text(),
                g.samples,
                g.cycles,
                g.fault,
                g.syscall,
                g.spin
            );
        }
        // The tables, heaviest groups first.
        let mut order: Vec<usize> = (0..groups.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(groups[i].cycles));
        for &gi in order.iter().take(40) {
            let g = &groups[gi];
            if g.samples == 0 {
                continue;
            }
            let ms: f64 = self
                .threads
                .iter()
                .filter(|t| t.group == g.name && t.kind == g.kind)
                .map(|t| (t.user + t.kernel) as f64 / 1e4)
                .sum();
            let _ = writeln!(
                out,
                "# group {} ({}): {} samples, {:.0} ms of thread time",
                g.name,
                g.kind.text(),
                g.samples,
                ms
            );
            for (title, rows) in [("self", g.top_self()), ("inclusive", g.top_inclusive())] {
                let _ = writeln!(
                    out,
                    "# {title}: samples, share of the group, ms (of the group's thread time, by cycles), function"
                );
                for (name, samples, cyc) in rows {
                    let function = self.label(labels, name.0);
                    let share = 100.0 * samples as f64 / g.samples.max(1) as f64;
                    let est = if g.cycles > 0 {
                        ms * cyc as f64 / g.cycles as f64
                    } else {
                        0.0
                    };
                    let tag = match name.1 {
                        M_SYSCALL => " [syscall]",
                        M_SPIN => " [spin]",
                        _ => "",
                    };
                    let _ = writeln!(
                        out,
                        "{title}\t{}\t{samples}\t{share:.2}\t{est:.1}\t{function}{tag}",
                        g.name
                    );
                }
            }
        }
        out
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// The module name of a `module+0xrva` label.
fn module_name_of(_modules: &Modules, label: &str) -> String {
    label
        .rsplit_once('+')
        .map_or(String::new(), |(m, _)| m.to_string())
}

/// What the tables of a group add up.
struct GroupAgg {
    name: String,
    kind: Kind,
    samples: u64,
    cycles: u64,
    fault: u64,
    syscall: u64,
    spin: u64,
    own: HashMap<(u64, u64), (u64, u64)>,
    inclusive: HashMap<u64, (u64, u64)>,
}

impl GroupAgg {
    fn new(name: &str, kind: Kind) -> GroupAgg {
        GroupAgg {
            name: name.to_string(),
            kind,
            samples: 0,
            cycles: 0,
            fault: 0,
            syscall: 0,
            spin: 0,
            own: HashMap::new(),
            inclusive: HashMap::new(),
        }
    }

    fn add_self(&mut self, key: (u64, u64), samples: u32, cycles: u64) {
        let e = self.own.entry(key).or_default();
        e.0 += u64::from(samples);
        e.1 += cycles;
    }

    fn add_inclusive(&mut self, key: u64, samples: u32, cycles: u64) {
        let e = self.inclusive.entry(key).or_default();
        e.0 += u64::from(samples);
        e.1 += cycles;
    }

    fn top_self(&self) -> Vec<((u64, u64), u64, u64)> {
        let mut rows: Vec<_> = self.own.iter().map(|(k, v)| (*k, v.0, v.1)).collect();
        rows.sort_by_key(|r| std::cmp::Reverse((r.2, r.1)));
        rows.truncate(50);
        rows
    }

    fn top_inclusive(&self) -> Vec<((u64, u64), u64, u64)> {
        let mut rows: Vec<_> = self
            .inclusive
            .iter()
            .map(|(k, v)| ((*k, 0), v.0, v.1))
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse((r.2, r.1)));
        rows.truncate(50);
        rows
    }
}

// ---------------------------------------------------------------------------------------------
// The thread that samples.

static RUNNING: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

/// Starts the profiler thread.
pub(crate) fn start(cfg: Config, phase: fn() -> u32) {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("victory-profile".into())
        .spawn(move || {
            struct Done;
            impl Drop for Done {
                fn drop(&mut self) {
                    DONE.store(true, Ordering::SeqCst);
                }
            }
            let _done = Done;
            match State::new(cfg, phase) {
                Ok(state) => run(state),
                Err(e) => eprintln!("VIctory: the profile does not start: {e}"),
            }
        });
    if spawned.is_err() {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// Stops the profiler and has it write the files one last time (the process is about to end), waiting for it
/// no more than a few seconds.
pub(crate) fn finish() {
    if !RUNNING.load(Ordering::SeqCst) {
        return;
    }
    STOP.store(true, Ordering::SeqCst);
    let began = Instant::now();
    while !DONE.load(Ordering::SeqCst) && began.elapsed() < Duration::from_secs(4) {
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn timer() -> (HANDLE, bool) {
    // SAFETY: plain calls that make a timer for this thread to wait on.
    unsafe {
        let high = CreateWaitableTimerExW(
            null(),
            null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS,
        );
        if !high.is_null() {
            return (high, true);
        }
        (
            CreateWaitableTimerExW(null(), null(), 0, TIMER_ALL_ACCESS),
            false,
        )
    }
}

/// Waits `ticks` of the performance counter.
fn wait(timer: HANDLE, ticks: i64) {
    if timer.is_null() {
        std::thread::sleep(Duration::from_secs_f64(
            ticks.max(0) as f64 / frequency() as f64,
        ));
        return;
    }
    // Relative, in units of 100 ns, as a negative number.
    let due = -((ticks.max(1) as i128 * 10_000_000 / i128::from(frequency())) as i64).max(1);
    // SAFETY: a timer of ours, set once and waited for.
    unsafe {
        SetWaitableTimer(timer, &due, 0, None, null(), 0);
        WaitForSingleObject(timer, 1000);
    }
}

fn run(mut state: State) {
    // SAFETY: raises this thread's priority, so that it is not left behind by the threads it samples.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
    let (timer, high) = timer();
    state.stats.high_resolution = high;
    let period = (frequency() as f64 / state.cfg.hz).max(1.0) as i64;
    let mut next = counter() + period;
    let mut next_refresh = Instant::now() + REFRESH;
    let mut next_write = Instant::now() + state.cfg.every;
    eprintln!(
        "VIctory: the profile begins ({} Hz, to {}, timer {})",
        state.cfg.hz,
        state.cfg.file.display(),
        if high {
            "high resolution"
        } else {
            "low resolution"
        }
    );
    while !STOP.load(Ordering::SeqCst) {
        let sampling = match state.cfg.duty {
            None => true,
            Some((on, off)) => {
                let phase = state.started.elapsed().as_secs_f64() % (on + off).as_secs_f64();
                phase < on.as_secs_f64()
            }
        };
        if sampling {
            state.begin_window();
            state.sample_round(false);
        } else {
            state.end_window();
        }
        let now = Instant::now();
        if now >= next_refresh {
            state.refresh();
            next_refresh = Instant::now() + REFRESH;
        }
        if now >= next_write {
            if let Err(e) = state.write() {
                eprintln!("VIctory: the profile was not written: {e}");
            }
            next_write = Instant::now() + state.cfg.every;
        }
        let left = next - counter();
        if sampling && left > 0 {
            wait(timer, left);
            next += period;
        } else if sampling {
            state.stats.late += 1;
            next = counter() + period;
        } else {
            wait(timer, frequency() / 50);
            next = counter() + period;
        }
    }
    state.refresh();
    state.end_window();
    match state.write() {
        Ok(()) => eprintln!("VIctory: the profile is in {}", state.cfg.file.display()),
        Err(e) => eprintln!("VIctory: the profile was not written: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureContext;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string())
    }

    fn no_phase() -> u32 {
        0
    }

    #[test]
    fn a_rate_is_read_and_the_old_list_is_left_to_the_old_profiler() {
        let dir = Path::new("D:\\cache");
        let c = Config::from_env("1000", &env(&[]), dir).unwrap();
        assert_eq!(c.hz, 1000.0);
        assert_eq!((c.depth, c.nodes, c.lines), (32, 1_048_576, 200_000));
        assert_eq!(c.every, Duration::from_secs(15));
        assert_eq!(c.file, Path::new("D:\\cache").join("profile.folded"));
        assert_eq!(c.duty, None);
        assert_eq!(Config::from_env("150,160,5,x.txt", &env(&[]), dir), None);
        assert_eq!(Config::from_env("0", &env(&[]), dir), None);
        assert_eq!(Config::from_env("fast", &env(&[]), dir), None);
        assert_eq!(Config::from_env("1e9", &env(&[]), dir).unwrap().hz, 5000.0);
        let c = Config::from_env(
            "250",
            &env(&[
                ("VICTORY_PROFILE_FILE", " E:\\x\\p.folded "),
                ("VICTORY_PROFILE_SECS", "0.1"),
                ("VICTORY_PROFILE_DEPTH", "500"),
                ("VICTORY_PROFILE_NODES", "10"),
                ("VICTORY_PROFILE_DUTY", "10, 5"),
            ]),
            dir,
        )
        .unwrap();
        assert_eq!(c.file, Path::new("E:\\x\\p.folded"));
        assert_eq!(
            (c.every, c.depth, c.nodes),
            (Duration::from_secs(1), MAX_DEPTH, 4096)
        );
        assert_eq!(
            c.duty,
            Some((Duration::from_secs(10), Duration::from_secs(5)))
        );
        // With no file named, the file goes beside the log of a run that has a capture folder.
        let c = Config::from_env(
            "100",
            &env(&[("VICTORY_CAPTURE", "G:\\runs\\a\\frames")]),
            dir,
        )
        .unwrap();
        assert_eq!(c.file, Path::new("G:\\runs\\a").join("profile.folded"));
        // A duty that is not two times is none.
        let c = Config::from_env("100", &env(&[("VICTORY_PROFILE_DUTY", "10")]), dir).unwrap();
        assert_eq!(c.duty, None);
    }

    #[test]
    fn threads_are_grouped_as_the_census_groups_them() {
        let exe = "victory.exe";
        assert_eq!(
            group_of("", "victory-image-b99f.dll", exe),
            ("title".into(), Kind::Title)
        );
        assert_eq!(
            group_of("Render", "victory-image-b99f.dll", exe),
            ("title".into(), Kind::Title)
        );
        assert_eq!(
            group_of("victory-wide-1", "victory.exe", exe),
            ("victory-wide".into(), Kind::Ours)
        );
        assert_eq!(
            group_of("", "victory.exe", exe),
            ("ours (no name)".into(), Kind::Ours)
        );
        assert_eq!(
            group_of("", "nvwgf2umx.dll", exe),
            ("nvwgf2umx".into(), Kind::Other)
        );
        assert_eq!(group_of("", "", exe), ("?".into(), Kind::Other));
        assert_eq!(
            group_of("dstorage 7", "dstorage.dll", exe),
            ("dstorage".into(), Kind::Other)
        );
    }

    #[test]
    fn the_tree_counts_a_path_once_and_runs_out_of_room_without_losing_a_sample() {
        // Room for four nodes: the sentinel, a thread's root and the two frames of the path.
        let mut tree = Tree::new(4);
        let root = tree.child(0, THREAD_KEY | 7).unwrap();
        assert_eq!(tree.child(0, THREAD_KEY | 7), Some(root));
        let frames = [0x40, 0x30, 0x20, 0x10];
        let two = Walked {
            n: 2,
            ..Walked::default()
        };
        tree.record(root, &two, &frames, 100);
        tree.record(root, &two, &frames, 50);
        // The outermost frame (the last of the walk) comes first in the path: 0x30, then 0x40.
        let outer = tree.child(root, 0x30).unwrap();
        let inner = tree.child(outer, 0x40).unwrap();
        assert_eq!(
            (
                tree.nodes[outer as usize].samples,
                tree.nodes[outer as usize].cycles
            ),
            (0, 0)
        );
        assert_eq!(
            (
                tree.nodes[inner as usize].samples,
                tree.nodes[inner as usize].cycles
            ),
            (2, 150)
        );
        assert_eq!(tree.lost, 0);
        // A path that needs a node there is no room for ends at the deepest node it reached.
        let more = Walked {
            n: 4,
            fault: true,
            ..Walked::default()
        };
        tree.record(root, &more, &frames, 7);
        assert_eq!(tree.lost, 1);
        assert_eq!(
            (
                tree.nodes[root as usize].samples,
                tree.nodes[root as usize].cycles
            ),
            (1, 7)
        );
        assert_eq!(tree.nodes.len(), 4);
    }

    /// The addresses of the three functions of the known chain, taken from inside each.
    static INSIDE: [AtomicUsize; 3] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    /// The address inside `spin_here`.
    static INSIDE_SPIN: AtomicUsize = AtomicUsize::new(0);

    #[inline(always)]
    fn here() -> usize {
        let at: usize;
        // SAFETY: reads the instruction pointer.
        unsafe {
            std::arch::asm!("lea {}, [rip]", out(reg) at, options(nomem, nostack, preserves_flags))
        };
        at
    }

    /// The frames, as our walk and the crash report's walk make them, of this call.
    #[inline(never)]
    fn chain_c(modules: &Modules) -> (Vec<u64>, Vec<u64>, Walked) {
        INSIDE[2].store(here(), Ordering::SeqCst);
        let mut context = Aligned(unsafe { std::mem::zeroed() });
        // SAFETY: captures this thread's context, 16-byte aligned; the bounds are this thread's.
        let (ours, walked, theirs) = unsafe {
            RtlCaptureContext(&mut context.0);
            let (mut low, mut high) = (0usize, 0usize);
            windows_sys::Win32::System::Threading::GetCurrentThreadStackLimits(&mut low, &mut high);
            let mut theirs = Vec::with_capacity(crate::crash::MAX_FRAMES);
            crate::crash::unwind_into(context.0, low, high, &mut theirs);
            let mut out = [0u64; MAX_DEPTH];
            let walked = walk(modules, &mut context.0, (low, high), 32, 0, &mut out);
            (out[..walked.n].to_vec(), walked, theirs)
        };
        std::hint::black_box(&ours);
        (ours, theirs, walked)
    }

    #[inline(never)]
    fn chain_b(modules: &Modules) -> (Vec<u64>, Vec<u64>, Walked) {
        INSIDE[1].store(here(), Ordering::SeqCst);
        let r = chain_c(modules);
        std::hint::black_box(&r);
        r
    }

    #[inline(never)]
    fn chain_a(modules: &Modules) -> (Vec<u64>, Vec<u64>, Walked) {
        INSIDE[0].store(here(), Ordering::SeqCst);
        let r = chain_b(modules);
        std::hint::black_box(&r);
        r
    }

    /// A known call chain on this thread: the walk lists the functions in order, as function entries,
    /// and agrees with the crash report's walk on every frame.
    #[test]
    fn the_walk_of_our_own_stack_names_the_known_callers_in_order() {
        let mut modules = Modules::default();
        modules.refresh();
        let (ours, theirs, walked) = chain_a(&modules);
        let begin = |a: usize| modules.begin_of(a).expect("unwind data");
        let (a, b, c) = (
            begin(INSIDE[0].load(Ordering::SeqCst)),
            begin(INSIDE[1].load(Ordering::SeqCst)),
            begin(INSIDE[2].load(Ordering::SeqCst)),
        );
        assert!(a != b && b != c && a != c);
        // Frame 0 is where the context was captured, in `chain_c`; then its callers.
        assert_eq!(begin(ours[0] as usize), c, "{ours:x?}");
        assert_eq!(&ours[1..3], &[b as u64, a as u64], "{ours:x?}");
        assert!(
            walked.n >= 5 && !walked.fault && !walked.truncated,
            "{walked:?}"
        );
        // The crash report's walk (return addresses) gives the same functions.
        let theirs: Vec<u64> = theirs
            .iter()
            .take(6)
            .map(|&r| begin(r as usize - 1) as u64)
            .collect();
        let n = ours.len().min(theirs.len()).min(6);
        assert_eq!(&ours[1..n], &theirs[1..n]);
        // And the depth asked is kept, the stack cut there.
        let mut context = Aligned(unsafe { std::mem::zeroed() });
        let mut out = [0u64; MAX_DEPTH];
        // SAFETY: as above.
        let cut = unsafe {
            RtlCaptureContext(&mut context.0);
            let (mut low, mut high) = (0usize, 0usize);
            windows_sys::Win32::System::Threading::GetCurrentThreadStackLimits(&mut low, &mut high);
            walk(&modules, &mut context.0, (low, high), 3, 0, &mut out)
        };
        assert!(cut.n == 3 && cut.truncated, "{cut:?}");
    }

    #[inline(never)]
    fn spin_here(stop: &AtomicBool) {
        INSIDE_SPIN.store(here(), Ordering::SeqCst);
        while !stop.load(Ordering::Relaxed) {
            std::hint::spin_loop();
        }
    }

    /// The profiler samples another thread: the spinner is found under its group with `spin_here` in the
    /// stack and its instruction marked as a pause; a thread that waits in a system call is marked so; the
    /// files carry the same samples as the tree, and the weights add up to the cycles the thread used.
    #[test]
    fn another_threads_samples_are_counted_marked_and_written() {
        static SPIN_STOP: AtomicBool = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let spinner = std::thread::Builder::new()
            .name("victory-prof-spin-3".into())
            .spawn(|| spin_here(&SPIN_STOP))
            .unwrap();
        let waiter = std::thread::Builder::new()
            .name("victory-prof-wait-5".into())
            .spawn(move || {
                let _ = rx.recv();
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("p.folded");
        let cfg = Config::from_env("1000", &env(&[]), dir.path())
            .map(|c| Config {
                file: file.clone(),
                ..c
            })
            .unwrap();
        let mut state = State::new(cfg, no_phase).unwrap();
        for round in 0..300 {
            // The waiter does not run: it is sampled by force, now and then.
            state.sample_round(round % 20 == 0);
            std::thread::sleep(Duration::from_millis(1));
        }
        state.write().unwrap();
        SPIN_STOP.store(true, Ordering::SeqCst);
        tx.send(()).unwrap();
        spinner.join().unwrap();
        waiter.join().unwrap();

        let spin_begin = state
            .modules
            .begin_of(INSIDE_SPIN.load(Ordering::SeqCst))
            .unwrap() as u64;
        let folded = std::fs::read_to_string(&file).unwrap();
        let cycles = std::fs::read_to_string(with_suffix(&file, ".cycles")).unwrap();
        let report = std::fs::read_to_string(with_suffix(&file, ".txt")).unwrap();
        let count = |text: &str, group: &str| -> u64 {
            text.lines()
                .filter(|l| l.starts_with(&format!("{group};")))
                .map(|l| l.rsplit_once(' ').unwrap().1.parse::<u64>().unwrap())
                .sum()
        };
        let spin_samples = count(&folded, "victory-prof-spin");
        assert!(
            spin_samples > 100,
            "{spin_samples} samples of the spinner\n{folded}"
        );
        let spin_lines: Vec<&str> = folded
            .lines()
            .filter(|l| l.starts_with("victory-prof-spin;"))
            .collect();
        let named = state.modules.label(spin_begin as usize);
        assert!(
            spin_lines
                .iter()
                .any(|l| l.contains(&format!(";{named};")) || l.contains(&format!(";{named} "))),
            "{spin_lines:?}"
        );
        assert!(
            spin_lines.iter().any(|l| l.contains(";[spin] ")),
            "no sample at a pause: {spin_lines:?}"
        );
        assert!(
            folded
                .lines()
                .any(|l| l.starts_with("victory-prof-wait;") && l.contains(";[syscall] ")),
            "{folded}"
        );
        assert!(count(&cycles, "victory-prof-spin") > 0);
        // The tree and the file agree: every sample is in a line (the file keeps all of them here).
        let in_tree: u64 = state.tree.nodes.iter().map(|n| u64::from(n.samples)).sum();
        let in_file: u64 = folded
            .lines()
            .map(|l| l.rsplit_once(' ').unwrap().1.parse::<u64>().unwrap())
            .sum();
        assert_eq!(in_tree, in_file);
        assert_eq!(in_tree, state.stats.samples);
        // The weights are the cycles the thread used between its first sample's baseline and its last.
        let t = state
            .threads
            .iter()
            .find(|t| t.name.starts_with("victory-prof-spin"))
            .unwrap();
        assert!(t.cycles_sampled > 0 && t.samples > 100);
        // The report names the thread, the group and the modules, and says the profile's own cost.
        for needle in [
            "victory-profile 1",
            "\nthread\t",
            "victory-prof-spin\tours",
            "\nmodule\t",
            "\nsuspend_us_total\t",
            "\nprofiler_cpu_ms\t",
            "# group victory-prof-spin",
        ] {
            assert!(report.contains(needle), "no {needle:?} in\n{report}");
        }
        assert!(
            report
                .lines()
                .any(|l| l.starts_with("self\tvictory-prof-spin\t"))
        );
        assert!(
            report
                .lines()
                .any(|l| l.starts_with("inclusive\tvictory-prof-spin\t"))
        );
    }
}
