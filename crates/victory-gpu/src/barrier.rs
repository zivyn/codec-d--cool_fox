//! What the title's enhanced barriers (`barrier_groups`) say, as far as the PC's
//! layout of them reads (GPU3).
//!
//! Every group the title sends was a wait for every earlier shader write
//! (a UAV barrier on every resource): 690 a frame at the window spot, between
//! 7,700 commands that average 3.9 microseconds. A barrier whose "access
//! before" holds no unordered-access write (a render target made readable, a
//! copy's destination made a source, a texture only read before) orders
//! nothing that a UAV barrier orders: the host's own transitions of the host
//! copies (`crate::list::ensure_state`) wait for those writes where a texture
//! is used next. `VICTORY_GROUP_SKIP=1` leaves the wait out for groups that
//! are read fully and are all of that kind, and keeps it for anything it
//! cannot read or does not know. `VICTORY_BARRIER_LOOK=1` only counts and
//! says which shapes the groups have.
//!
//! The layout is `D3D12_BARRIER_GROUP` (type, count, pointer) of global (16
//! bytes: sync before and after, access before and after), buffer (40) and
//! texture (64) barriers. It is read with the reads that survive an
//! unmapped page, and every value must be a known flag, else the group is
//! "unknown" and the wait stays.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// What a barrier of a group says before and after: its kind (0 global, 1
/// texture, 2 buffer), sync before and after, access before and after.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Look {
    pub(crate) kind: u32,
    pub(crate) sync: [u32; 2],
    pub(crate) access: [u32; 2],
}

const UNORDERED_ACCESS: u32 = 0x10;
const STREAM_OUTPUT: u32 = 0x100;
const ACCELERATION_STRUCTURE_WRITE: u32 = 0x8000;
const NO_ACCESS: u32 = 0x8000_0000;
/// Every access flag of the PC's `D3D12_BARRIER_ACCESS` (and no access).
const ACCESS_KNOWN: u32 = 0x7f_ffff | NO_ACCESS;
/// Every sync flag of `D3D12_BARRIER_SYNC`.
const SYNC_KNOWN: u32 = 0xffff_ffff;
/// The size of a barrier of each kind.
const SIZES: [usize; 3] = [16, 64, 40];

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// The barriers of `count` groups at `groups`, read through `read` (which
/// fills the buffer from an address and says whether it could). `None` if
/// anything does not read as the PC's layout.
pub(crate) fn decode(groups: usize, count: u32, read: &dyn Fn(usize, &mut [u8]) -> bool) -> Option<Vec<Look>> {
    if count == 0 || count > 16 {
        return None;
    }
    let mut head = vec![0u8; count as usize * 16];
    if groups % 4 != 0 || !read(groups, &mut head) {
        return None;
    }
    let mut out = Vec::new();
    for g in 0..count as usize {
        let at = g * 16;
        let kind = word(&head, at);
        let n = word(&head, at + 4) as usize;
        let pointer = u64::from_le_bytes(head[at + 8..at + 16].try_into().ok()?) as usize;
        if kind > 2 || n == 0 || n > 64 || pointer == 0 || pointer % 4 != 0 {
            return None;
        }
        let size = SIZES[kind as usize];
        let mut body = vec![0u8; n * size];
        if !read(pointer, &mut body) {
            return None;
        }
        for b in 0..n {
            let at = b * size;
            let look = Look {
                kind,
                sync: [word(&body, at), word(&body, at + 4)],
                access: [word(&body, at + 8), word(&body, at + 12)],
            };
            if look.sync.iter().any(|s| s & !SYNC_KNOWN != 0) || look.access.iter().any(|a| a & !ACCESS_KNOWN != 0) {
                return None;
            }
            out.push(look);
        }
    }
    Some(out)
}

/// Whether the groups need the wait for every earlier shader write: one of
/// their barriers has an unordered-access, stream-output or
/// acceleration-structure write before it, or says nothing (common access:
/// anything).
pub(crate) fn needs_wait(looks: &[Look]) -> bool {
    looks.iter().any(|l| {
        let before = l.access[0];
        before == 0 || before & (UNORDERED_ACCESS | STREAM_OUTPUT | ACCELERATION_STRUCTURE_WRITE) != 0
    })
}

/// Whether groups are looked at (`VICTORY_BARRIER_LOOK=1` or the skip).
pub(crate) fn looking() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BARRIER_LOOK").is_some_and(|v| v == "1") || skipping())
}

/// Whether the wait is left out for groups that do not need it
/// (`VICTORY_GROUP_SKIP=1`; off by default).
pub(crate) fn skipping() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GROUP_SKIP").is_some_and(|v| v == "1"))
}

/// Since the last line: calls, groups read, groups not read, calls that need
/// the wait, calls left without it.
pub(crate) static CALLS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
/// What the host emitted for the title's other barriers, since the last line:
/// barrier calls, the barriers in them, the waits added for buffers, pipeline
/// syncs, transitions of host copies for our own uses.
pub(crate) static EMITTED: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

static SHAPES: Mutex<Option<HashMap<Look, u64>>> = Mutex::new(None);

/// The groups of a call, once decided: counted, and their shapes noted.
pub(crate) fn note(looks: Option<&[Look]>, wait: bool) {
    CALLS[0].fetch_add(1, Ordering::Relaxed);
    match looks {
        Some(looks) => {
            CALLS[1].fetch_add(looks.len() as u64, Ordering::Relaxed);
            let mut shapes = SHAPES.lock().unwrap_or_else(|e| e.into_inner());
            let shapes = shapes.get_or_insert_with(HashMap::new);
            if shapes.len() < 4096 {
                for l in looks {
                    *shapes.entry(*l).or_insert(0) += 1;
                }
            }
        }
        None => {
            CALLS[2].fetch_add(1, Ordering::Relaxed);
        }
    }
    CALLS[if wait { 3 } else { 4 }].fetch_add(1, Ordering::Relaxed);
}

/// The line, once every five seconds: what the title's barriers came to and
/// what the host emitted for them, per `frames`.
pub(crate) fn line(frames: f64) {
    let c: Vec<u64> = CALLS.iter().map(|c| c.swap(0, Ordering::Relaxed)).collect();
    let e: Vec<u64> = EMITTED.iter().map(|c| c.swap(0, Ordering::Relaxed)).collect();
    let shapes = SHAPES.lock().unwrap_or_else(|e| e.into_inner()).take();
    if c[0] + e.iter().sum::<u64>() == 0 || frames <= 0.0 {
        return;
    }
    let per = |v: u64| v as f64 / frames;
    let mut top = String::new();
    if let Some(shapes) = shapes {
        let mut all: Vec<_> = shapes.into_iter().collect();
        all.sort_by_key(|&(look, n)| (std::cmp::Reverse(n), look.kind, look.access, look.sync));
        for (l, n) in all.into_iter().take(8) {
            top.push_str(&format!(
                "; {:.1}/frame kind {} sync {:#x}->{:#x} access {:#x}->{:#x}",
                per(n),
                l.kind,
                l.sync[0],
                l.sync[1],
                l.access[0],
                l.access[1]
            ));
        }
    }
    tracing::info!(
        "barriers per frame (VICTORY_BARRIER_LOOK, VICTORY_GROUP_SKIP={}): the title's groups {:.0} calls with {:.0} barriers read, {:.0} calls not read, {:.0} calls with a wait for every earlier shader write, {:.0} without it; \
         its other barrier calls {:.0} with {:.0} barriers, {:.0} waits added for its buffers, {:.0} pipeline syncs; {:.0} transitions of host copies for our own uses{top}",
        u8::from(skipping()),
        per(c[0]),
        per(c[1]),
        per(c[2]),
        per(c[3]),
        per(c[4]),
        per(e[0]),
        per(e[1]),
        per(e[2]),
        per(e[3]),
        per(e[4]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A buffer of bytes as title memory at `base`.
    struct Memory {
        base: usize,
        bytes: Vec<u8>,
    }

    impl Memory {
        fn read(&self, at: usize, out: &mut [u8]) -> bool {
            let Some(from) = at.checked_sub(self.base) else {
                return false;
            };
            match self.bytes.get(from..from + out.len()) {
                Some(src) => {
                    out.copy_from_slice(src);
                    true
                }
                None => false,
            }
        }
    }

    fn put(bytes: &mut Vec<u8>, at: usize, words: &[u32]) {
        if bytes.len() < at + words.len() * 4 {
            bytes.resize(at + words.len() * 4, 0);
        }
        for (i, w) in words.iter().enumerate() {
            bytes[at + i * 4..at + i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
    }

    /// Two groups: a global barrier of 16 bytes and two buffer barriers of 40.
    fn memory() -> Memory {
        let base = 0x1000;
        let mut bytes = Vec::new();
        // Group headers at +0: (kind, n, pointer).
        put(&mut bytes, 0, &[0, 1, base as u32 + 0x100, 0]);
        put(&mut bytes, 16, &[2, 2, base as u32 + 0x200, 0]);
        // The global barrier: all shading, all shading, UAV -> SRV.
        put(&mut bytes, 0x100, &[0x1000, 0x1000, UNORDERED_ACCESS, 0x80]);
        // Buffer barriers: compute, compute, copy dest -> SRV; render target -> SRV.
        put(&mut bytes, 0x200, &[0x80, 0x80, 0x400, 0x80, 0, 0, 0, 0, 0, 0]);
        put(&mut bytes, 0x228, &[0x40, 0x80, 0x8, 0x80, 0, 0, 0, 0, 0, 0]);
        Memory { base, bytes }
    }

    #[test]
    fn groups_read_as_the_pcs_layout() {
        let m = memory();
        let looks = decode(m.base, 2, &|at, out| m.read(at, out)).unwrap();
        assert_eq!(looks.len(), 3);
        assert_eq!(looks[0], Look { kind: 0, sync: [0x1000, 0x1000], access: [UNORDERED_ACCESS, 0x80] });
        assert_eq!(looks[1].kind, 2);
        assert_eq!(looks[2].access, [0x8, 0x80]);
        // The first group alone holds a write: the wait stays.
        assert!(needs_wait(&looks[..1]));
        // The buffer barriers after a copy and a render target do not.
        assert!(!needs_wait(&looks[1..]));
        // Together they do.
        assert!(needs_wait(&looks));
    }

    #[test]
    fn what_does_not_read_as_a_group_keeps_the_wait() {
        let m = memory();
        let read = |at: usize, out: &mut [u8]| m.read(at, out);
        // Past the memory, a misaligned address, no groups, too many.
        assert!(decode(m.base + 0x1000, 2, &read).is_none());
        assert!(decode(m.base + 2, 2, &read).is_none());
        assert!(decode(m.base, 0, &read).is_none());
        assert!(decode(m.base, 17, &read).is_none());
        // A kind that is none, a count that is none, a pointer that is none.
        for (words, why) in [([3u32, 1, 0x1100, 0], "kind"), ([0, 0, 0x1100, 0], "count"), ([0, 1, 0, 0], "pointer"), ([0, 65, 0x1100, 0], "many")] {
            let mut bad = memory();
            put(&mut bad.bytes, 0, &words);
            assert!(decode(bad.base, 1, &|at, out| bad.read(at, out)).is_none(), "{why}");
        }
        // A flag nobody knows (access bit 23) is not read.
        let mut odd = memory();
        put(&mut odd.bytes, 0x100, &[0x1000, 0x1000, 0x80_0000, 0x80]);
        assert!(decode(odd.base, 1, &|at, out| odd.read(at, out)).is_none());
    }

    #[test]
    fn a_barrier_that_says_nothing_before_it_is_a_wait() {
        let look = |before| Look { kind: 0, sync: [1, 1], access: [before, 0x80] };
        // Common access: anything may be written.
        assert!(needs_wait(&[look(0)]));
        for access in [UNORDERED_ACCESS, STREAM_OUTPUT, ACCELERATION_STRUCTURE_WRITE, UNORDERED_ACCESS | 0x80] {
            assert!(needs_wait(&[look(access)]), "{access:#x}");
        }
        for access in [0x8, 0x20, 0x40, 0x80, 0x400, 0x800, 0x1000, 0x2000, NO_ACCESS, 0x8 | 0x80] {
            assert!(!needs_wait(&[look(access)]), "{access:#x}");
        }
        assert!(!needs_wait(&[]));
    }
}
