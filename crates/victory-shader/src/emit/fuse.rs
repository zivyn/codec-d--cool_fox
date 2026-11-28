//! Dispatches made one (`VICTORY_FUSE_DISPATCH=1`, read when a program is translated; part of the shader cache's key).
//!
//! The title's tail is thousands of tiny dispatches a frame, many of them runs of the same program one after the
//! other with only their user data (root constants, the offsets of their descriptor tables) and their group counts
//! different. Each is a command of the card of its own, with the launch and the dependent chain of loads (constants,
//! descriptor, parameter, data) in front of its few groups. A compute program translated with the switch on does not
//! read its user data from the root constants of its stage but from a record of a table, and does not take its group id
//! from the dispatch but from the table too: the host (`victory-gpu` `draw::fuse`) makes the table of a run of
//! dispatches of the program, one record each, and dispatches the groups of all of them at once.
//!
//! The table is the root CBV [`SLOT_FUSE`] (`b250` in the root CBV space), 16-byte rows:
//!
//! - row 0: the members `n`, the rows of a record `R`, the groups of the whole run, the first row of the records;
//!   a table with `n` 0 is the table of a dispatch that is run as the title made it (one member, group ids the
//!   dispatch's own, whatever its group counts: a single dispatch, or one whose counts the card reads from memory):
//!   its records begin at row 1 and its starts are not there;
//! - the starts of the members, four to a row from row 1: start `k` is the number of the member's first group in the
//!   run (ascending, the first 0);
//! - member `k`: `R` rows from the first row of the records plus `k * R`: its group counts `(x, y, z)` and a pad, then
//!   its user data, four dwords to a row.
//!
//! The groups of the dispatch are numbered `y * FUSE_WIDTH + x` (a dispatch of 65,535 groups or fewer is one row of
//! them); a group past the run's end leaves at once. The program finds its member by eight halvings of the starts (a
//! run has at most [`FUSE_MEMBERS`]), takes its number in the member's dispatch from the difference with the start, and
//! the group id the console program reads is that number cut into the member's group counts (`x` fastest). Nothing
//! else of the program changes: the threads of a group are the program's own, the console programs never read the
//! size of their dispatch (the title passes it in constants, which are user data), and a group of a member meets
//! only the groups of its member at a barrier.
//!
//! A program that reads a root view of the title (a CBV, SRV or UAV of its own, title memory by address) is lowered as
//! it always was: the run's table is its only root view. Dispatches counted in threads (the structure builder's) are no
//! runs either.

use super::super::shader::{FUSE_MEMBERS, FUSE_ROWS, FUSE_WIDTH, SLOT_FUSE};
use super::{Attr, Bin, Lower, Lowered, Term, V, Val, pred};
use crate::native::{HwStage, StageIo};

#[cfg(test)]
thread_local! {
    /// A test's choice of the switch, for its thread only.
    pub(in crate::emit) static TEST_FUSE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether compute programs are translated to read their user data and group ids from a run's table
/// (`VICTORY_FUSE_DISPATCH=1`).
pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_FUSE.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FUSE_DISPATCH").is_some_and(|v| v == "1"))
}

/// Whether to try the program in the form of a run's member.
pub(super) fn wanted(parts: &super::Parts) -> bool {
    on() && parts.native.stage == HwStage::Cs
        && matches!(&parts.native.io, StageIo::Cs(cs) if cs.threads.iter().all(|&t| t > 0))
        && !crate::spill::is_spilled(parts.native)
}

/// Whether a program lowered that way has no root view but the table (else it is lowered as before).
pub(super) fn kept(l: &Lowered) -> bool {
    l.root_cbvs == [SLOT_FUSE] && l.root_srvs.is_empty() && l.root_uavs.is_empty()
}

/// What the prologue of a member made.
#[derive(Default)]
pub(super) struct Fused {
    pub on: bool,
    /// The first row of the member's record.
    base: Option<V>,
    /// The group id of the console program: the group's number in its member's dispatch, by component.
    local: Option<[V; 3]>,
}

impl Lower<'_> {
    /// The lowering as a member of a run (`on`) or as it always was.
    pub(super) fn fused(mut self, on: bool) -> Self {
        self.fuse.on = on;
        self
    }

    /// The row `row` of the table `h`.
    fn fuse_row(&mut self, block: usize, h: V, row: V) -> V {
        self.s.op(
            block,
            "cbufferLoadLegacy.i32",
            Attr::ReadOnly,
            self.s.t.cbuf_ret,
            59,
            &[h, row],
        )
    }

    /// Start `k` of the members, a dword of row `1 + k / 4`.
    fn fuse_start(&mut self, block: usize, h: V, k: V) -> V {
        let (one, two, three) = (self.s.m.i32(1), self.s.m.i32(2), self.s.m.i32(3));
        let quad = self.s.m.bin(block, Bin::LShr, k, two);
        let row = self.s.m.bin(block, Bin::Add, quad, one);
        let v = self.fuse_row(block, h, row);
        let at = self.s.m.bin(block, Bin::And, k, three);
        let mut found = self.s.m.extract(block, v, 0);
        for c in 1..4 {
            let this = self.s.m.extract(block, v, c);
            let is = self.s.m.i32(c);
            let is = self.s.m.cmp(block, pred::EQ, at, is);
            found = self.s.m.select(block, is, this, found);
        }
        found
    }

    /// The program's start: finds the member the group belongs to and what the program reads of the console's
    /// registers from it. A group past the run's end leaves; the program goes on in the block the others reach.
    pub(super) fn fuse_prologue(&mut self) {
        let original = self.s.entry;
        let h = self.s.root_cbv_handle(SLOT_FUSE);
        let zero = self.s.m.i32(0);
        let header = self.fuse_row(original, h, zero);
        let members = self.s.m.extract(original, header, 0);
        let rows = self.s.m.extract(original, header, 1);
        let total = self.s.m.extract(original, header, 2);
        let records = self.s.m.extract(original, header, 3);
        let (Val::I32(x), Val::I32(y), Val::I32(z)) = (
            self.compute_id("groupId.i32", 94, 0),
            self.compute_id("groupId.i32", 94, 1),
            self.compute_id("groupId.i32", 94, 2),
        ) else {
            unreachable!()
        };
        let width = self.s.m.i32(FUSE_WIDTH);
        let row = self.s.m.bin(original, Bin::Mul, y, width);
        let group = self.s.m.bin(original, Bin::Add, x, row);
        // (A table of no members is the title's own dispatch: every group runs, and it has its own ids.)
        let own = self.s.m.cmp(original, pred::EQ, members, zero);
        let there = self.s.m.cmp(original, pred::IULT, group, total);
        let inside = self.s.m.bin(original, Bin::Or, own, there);
        let (body, done) = (self.s.m.new_block(), self.s.m.new_block());
        self.s
            .m
            .terminate(original, Term::CondBr(inside, body, done));
        self.s.m.terminate(done, Term::Ret);
        self.s.entry = body;
        // The last member that starts at or before the group.
        let mut k = zero;
        let mut step = (FUSE_MEMBERS as u32 + 1) / 2;
        while step > 0 {
            let by = self.s.m.i32(step);
            let next = self.s.m.bin(body, Bin::Add, k, by);
            let exists = self.s.m.cmp(body, pred::IULT, next, members);
            let look = self.s.m.select(body, exists, next, k);
            let start = self.fuse_start(body, h, look);
            let reached = self.s.m.cmp(body, pred::IULE, start, group);
            let take = self.s.m.bin(body, Bin::And, exists, reached);
            k = self.s.m.select(body, take, next, k);
            step /= 2;
        }
        let start = self.fuse_start(body, h, k);
        let at = self.s.m.bin(body, Bin::Mul, k, rows);
        let base = self.s.m.bin(body, Bin::Add, records, at);
        let counts = self.fuse_row(body, h, base);
        let (cx, cy) = (
            self.s.m.extract(body, counts, 0),
            self.s.m.extract(body, counts, 1),
        );
        let number = self.s.m.bin(body, Bin::Sub, group, start);
        let lx = self.s.m.bin(body, Bin::URem, number, cx);
        let rest = self.s.m.bin(body, Bin::UDiv, number, cx);
        let ly = self.s.m.bin(body, Bin::URem, rest, cy);
        let lz = self.s.m.bin(body, Bin::UDiv, rest, cy);
        let lx = self.s.m.select(body, own, x, lx);
        let ly = self.s.m.select(body, own, y, ly);
        let lz = self.s.m.select(body, own, z, lz);
        self.fuse.base = Some(base);
        self.fuse.local = Some([lx, ly, lz]);
    }

    /// The group id component `c` the console program reads.
    pub(super) fn fuse_group(&self, c: u8) -> Option<V> {
        self.fuse.local.and_then(|l| l.get(usize::from(c)).copied())
    }

    /// User-data dword `index` of the member's record, loaded in the program's first block.
    pub(super) fn fuse_user(&mut self, index: u32) -> V {
        let base = self.fuse.base.expect("the prologue came first");
        let entry = self.s.entry;
        let h = self.s.root_cbv_handle(SLOT_FUSE);
        let one = self.s.m.i32(1 + index / 4);
        let row = self.s.m.bin(entry, Bin::Add, base, one);
        let r = self.fuse_row(entry, h, row);
        self.s.m.extract(entry, r, index % 4)
    }
}

/// Rows of one member's record: its group counts, then its `dwords` of user data.
pub const fn fuse_record_rows(dwords: usize) -> usize {
    1 + dwords.div_ceil(4)
}

/// Rows of the table of a run of `members` with `dwords` of user data each.
pub const fn fuse_rows(members: usize, dwords: usize) -> usize {
    1 + members.div_ceil(4) + members * fuse_record_rows(dwords)
}

/// Whether a run of `members` with `dwords` of user data each fits in a root CBV and the program's halvings.
pub const fn fuse_fits(members: usize, dwords: usize) -> bool {
    members <= FUSE_MEMBERS && fuse_rows(members, dwords) <= FUSE_ROWS
}

/// Rows of the table of a dispatch run as the title made it ([`fuse_fill_native`]).
pub const fn fuse_native_rows(dwords: usize) -> usize {
    1 + fuse_record_rows(dwords)
}

/// Writes the table of one dispatch that is run as the title made it (its own group ids, whatever its group counts
/// are, even if they are in memory) into `out` (`fuse_native_rows(dwords) * 4` dwords or more). `None` if `out` is too
/// short.
pub fn fuse_fill_native(out: &mut [u32], dwords: usize, user: &[u32]) -> Option<()> {
    let rows = fuse_record_rows(dwords);
    if user.len() != dwords || out.len() < fuse_native_rows(dwords) * 4 {
        return None;
    }
    out[..4].copy_from_slice(&[0, rows as u32, 0, 1]);
    // (Counts of one in each direction: the program cuts the group's number into them and does not use the result.)
    out[4..8].copy_from_slice(&[1, 1, 1, 0]);
    out[8..8 + dwords].copy_from_slice(user);
    out[8 + dwords..4 + rows * 4].fill(0);
    Some(())
}

/// The group counts of the dispatch of a run of `total` groups.
pub const fn fuse_dims(total: u64) -> [u32; 3] {
    if total <= FUSE_WIDTH as u64 {
        [total as u32, 1, 1]
    } else {
        [FUSE_WIDTH, total.div_ceil(FUSE_WIDTH as u64) as u32, 1]
    }
}

/// Writes the table of a run into `out` (`fuse_rows(..) * 4` dwords or more) and returns the group counts of the
/// dispatch that runs it: the members have the group counts `groups` and the dwords of user data `values` holds,
/// `dwords` each, one member after the other. `None` if the run is not one the program can run (empty, a member
/// without groups, too long, a total that is not 32 bits, a slice that does not fit).
pub fn fuse_fill(
    out: &mut [u32],
    dwords: usize,
    groups: &[[u32; 3]],
    values: &[u32],
) -> Option<[u32; 3]> {
    let n = groups.len();
    if n == 0
        || !fuse_fits(n, dwords)
        || values.len() != n * dwords
        || out.len() < fuse_rows(n, dwords) * 4
    {
        return None;
    }
    let rows = fuse_record_rows(dwords);
    let records = 1 + n.div_ceil(4);
    let mut total = 0u64;
    for (k, g) in groups.iter().enumerate() {
        let size = u64::from(g[0]) * u64::from(g[1]) * u64::from(g[2]);
        if size == 0 {
            return None;
        }
        out[4 + k] = u32::try_from(total).ok()?;
        total += size;
        let at = (records + k * rows) * 4;
        out[at..at + 3].copy_from_slice(g);
        out[at + 3] = 0;
        let mine = &values[k * dwords..(k + 1) * dwords];
        out[at + 4..at + 4 + dwords].copy_from_slice(mine);
        // (The pad of the last row.)
        out[at + 4 + dwords..at + rows * 4].fill(0);
    }
    if total > u64::from(u32::MAX >> 1) {
        return None;
    }
    // The header: the members, the rows of a record, the groups of the run, where the records start.
    out[..4].copy_from_slice(&[n as u32, rows as u32, total as u32, records as u32]);
    // (The pad of the last row of starts.)
    out[4 + n..records * 4].fill(0);
    Some(fuse_dims(total))
}

/// What the program does with a table and the group number `group` of the dispatch (`y * FUSE_WIDTH + x`): the
/// member, its own group number cut into its group counts, or `None` for a group past the run's end. The same halvings.
pub fn fuse_locate(table: &[u32], group: u32) -> Option<(usize, [u32; 3])> {
    let (n, rows, total, records) = (
        table[0] as usize,
        table[1] as usize,
        table[2],
        table[3] as usize,
    );
    if group >= total {
        return None;
    }
    let mut k = 0usize;
    let mut step = (FUSE_MEMBERS + 1) / 2;
    while step > 0 {
        let next = k + step;
        if next < n && table[4 + next] <= group {
            k = next;
        }
        step /= 2;
    }
    let at = (records + k * rows) * 4;
    let (cx, cy) = (table[at], table[at + 1]);
    let number = group - table[4 + k];
    let rest = number / cx;
    Some((k, [number % cx, rest % cy, rest / cy]))
}

/// The user data dword `d` of member `k` in a table.
pub fn fuse_user_dword(table: &[u32], k: usize, d: usize) -> u32 {
    let (rows, records) = (table[1] as usize, table[3] as usize);
    table[(records + k * rows) * 4 + 4 + d]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small generator (xorshift).
    struct Dice(u64);

    impl Dice {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) % n
        }
    }

    #[test]
    fn every_group_of_a_run_finds_its_member_and_its_own_number() {
        let mut dice = Dice(0x9E37_79B9_7F4A_7C15);
        for trial in 0..200 {
            let n = 1 + dice.below(if trial % 5 == 0 {
                FUSE_MEMBERS as u64
            } else {
                12
            }) as usize;
            let dwords = 4 * dice.below(5) as usize;
            let groups: Vec<[u32; 3]> = (0..n)
                .map(|_| {
                    [
                        1 + dice.below(7) as u32,
                        1 + dice.below(4) as u32,
                        1 + dice.below(3) as u32,
                    ]
                })
                .collect();
            let values: Vec<u32> = (0..n * dwords).map(|i| 0x1000 + i as u32).collect();
            if !fuse_fits(n, dwords) {
                continue;
            }
            let mut table = vec![0xdead_beef; fuse_rows(n, dwords) * 4];
            let dims = fuse_fill(&mut table, dwords, &groups, &values).expect("fits");
            let total: u32 = groups.iter().map(|g| g[0] * g[1] * g[2]).sum();
            assert_eq!(
                u64::from(dims[0]) * u64::from(dims[1]) * u64::from(dims[2]),
                u64::from(total),
                "one row of them"
            );
            let mut group = 0u32;
            for (k, g) in groups.iter().enumerate() {
                for z in 0..g[2] {
                    for y in 0..g[1] {
                        for x in 0..g[0] {
                            assert_eq!(
                                fuse_locate(&table, group),
                                Some((k, [x, y, z])),
                                "group {group} of {groups:?}"
                            );
                            group += 1;
                        }
                    }
                }
                for d in 0..dwords {
                    assert_eq!(fuse_user_dword(&table, k, d), values[k * dwords + d]);
                }
            }
            assert_eq!(fuse_locate(&table, total), None);
            assert_eq!(fuse_locate(&table, total + 7), None);
        }
    }

    #[test]
    fn the_table_of_a_dispatch_as_the_title_made_it_has_no_members_and_its_record_at_row_1() {
        let user = [11, 22, 33, 44, 55, 66];
        let mut table = vec![0xdead_beef; fuse_native_rows(6) * 4];
        assert_eq!(fuse_fill_native(&mut table, 6, &user), Some(()));
        assert_eq!(table[0], 0, "no members");
        let (rows, records) = (table[1] as usize, table[3] as usize);
        assert_eq!((rows, records), (fuse_record_rows(6), 1));
        assert_eq!(
            &table[records * 4..records * 4 + 3],
            &[1, 1, 1],
            "counts the program can divide by"
        );
        assert_eq!(&table[records * 4 + 4..records * 4 + 10], &user);
        assert_eq!(table.len(), (1 + rows) * 4);
        assert!(table[records * 4 + 10..].iter().all(|&w| w == 0), "the pad");
        assert_eq!(fuse_fill_native(&mut table[..8], 6, &user), None);
        assert_eq!(fuse_fill_native(&mut table, 6, &user[..5]), None);
    }

    #[test]
    fn a_run_wider_than_a_row_of_groups_is_two_dimensional() {
        assert_eq!(fuse_dims(1), [1, 1, 1]);
        assert_eq!(fuse_dims(65535), [65535, 1, 1]);
        assert_eq!(fuse_dims(65536), [65535, 2, 1]);
        assert_eq!(fuse_dims(65535 * 3 + 1), [65535, 4, 1]);
        let groups = [[65535, 1, 1], [5, 1, 1]];
        let mut table = vec![0; fuse_rows(2, 4) * 4];
        let dims = fuse_fill(&mut table, 4, &groups, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(dims, [65535, 2, 1]);
        // The group the dispatch's second row starts with is the member 1's first but one.
        assert_eq!(fuse_locate(&table, 65535 + 2), Some((1, [2, 0, 0])));
        assert_eq!(fuse_locate(&table, 65535 + 5), None);
    }

    #[test]
    fn a_run_the_program_cannot_run_is_refused() {
        let mut table = vec![0; 4096 * 4];
        assert_eq!(fuse_fill(&mut table, 4, &[], &[]), None);
        assert_eq!(
            fuse_fill(&mut table, 4, &[[1, 0, 1]], &[0; 4]),
            None,
            "a member without groups"
        );
        assert_eq!(
            fuse_fill(&mut table, 4, &[[1, 1, 1]], &[0; 3]),
            None,
            "user data of the wrong length"
        );
        let many = vec![[1, 1, 1]; FUSE_MEMBERS + 1];
        assert_eq!(
            fuse_fill(&mut table, 0, &many, &[]),
            None,
            "too many members"
        );
        let wide = vec![[1, 1, 1]; FUSE_MEMBERS];
        // (Seventeen rows each: past the 4,096 rows of a root CBV.)
        assert!(!fuse_fits(FUSE_MEMBERS, 64));
        assert_eq!(
            fuse_fill(&mut table, 64, &wide, &vec![0; FUSE_MEMBERS * 64]),
            None
        );
        assert!(fuse_fits(FUSE_MEMBERS, 0));
        assert_eq!(
            fuse_fill(&mut table[..8], 0, &[[1, 1, 1]], &[]),
            None,
            "a slice too short"
        );
        assert_eq!(
            fuse_fill(&mut table, 0, &[[65535, 65535, 3]], &[]),
            None,
            "more groups than 31 bits"
        );
    }
}
