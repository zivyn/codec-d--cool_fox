//! Raw buffer reads through the host's windows (`VICTORY_BINDLESS_BUFFERS=1`).
//!
//! A buffer the title binds through a descriptor table was read through a view the host made of it:
//! one `CreateShaderResourceView` and one descriptor copy for every table entry of every draw, and
//! a raw twin of every buffer view of the title's heap. The shader only ever used the heap slot of
//! the entry (the title's records are not the console's descriptors, so no program reads a base
//! address out of one).
//!
//! With the switch on, a program that reads such an entry as raw dwords (`buffer_load_dword*`
//! without an index or a format, `s_buffer_load_dword*` of an SRV) asks the host's **window** of
//! the heap slot first: four dwords a slot in the root SRV `t<SLOT_VIEW_WINDOWS>`, the number of
//! the host copy the view lies in (0: no window, read the view as before), the byte its view
//! begins at in that copy, and its length. The copy is read through the one raw view of it the
//! host keeps in the heap for good (slot `USER_WINDOW_BASE + number`): so the host makes no view
//! for the entry, only writes sixteen bytes, and copies no descriptor.
//!
//! Bounds are the view's own: a dword is read only where it lies whole inside the length, and is 0
//! where it does not, as a raw view of that length reads (the raw twin the view path uses is that
//! window of the copy). A slot whose window the host did not write (a copy it has no number for, a
//! view that does not begin on 16 bytes, which the raw twin leaves null) has number 0, and the
//! program takes the branch that reads its view, which is what it did before; so one translation
//! serves a host that has the windows on, off, or on for some bindings only.
//!
//! What the host needs to know of the program: `HostShader::bindless` lists the table entries
//! (root parameter, offset in dwords) that the program reads through windows alone. An entry any
//! instruction reads through its view (a typed or structured access, a store, a constant buffer's
//! read, an image access) is not in it: its view must be made for the program.
//!
//! The switch is read once, when a program is translated, and is part of the shader cache's key.

use std::collections::HashSet;

use super::super::shader::{
    SLOT_VIEW_WINDOWS, SLOT_WINDOWS_COUNT, USER_WINDOW_BASE, WINDOW_COPIES,
};
use super::*;

#[cfg(test)]
thread_local! {
    /// A test's choice of the switch, for its thread only.
    pub(in crate::emit) static TEST_BINDLESS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether table buffers are read through windows (`VICTORY_BINDLESS_BUFFERS=1`).
pub(in crate::emit) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_BINDLESS.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BINDLESS_BUFFERS").is_some_and(|v| v == "1"))
}

/// What the lowering knows of the table entries read through windows.
#[derive(Default)]
pub(super) struct Entries {
    /// Windows by block and descriptor load: the copy number, the byte its view begins at, its length.
    windows: HashMap<(usize, Site, u8), (V, V, V)>,
    /// Entries (root parameter, offset in dwords) some access reads through a window.
    windowed: HashSet<(u8, u32)>,
    /// Entries some access reads through the view.
    viewed: HashSet<(u8, u32)>,
    /// The values some scalar or vector arithmetic (or a join) reads, found once ([`Lower::words_are_numbers`]).
    read: Option<HashSet<Value>>,
    /// The view branch of a window access is being made: its handle is no use of the view in itself.
    pub(super) fallback: bool,
}

impl Entries {
    /// The entries only windows read, ordered.
    pub(super) fn only_windows(&self) -> Vec<(u8, u32)> {
        let mut out: Vec<_> = self.windowed.difference(&self.viewed).copied().collect();
        out.sort_unstable();
        out
    }
}

impl<'a> Lower<'a> {
    /// Whether the dwords `code..code + count` the scalar load `inst` wrote are read as numbers by some instruction
    /// (as [`Lower::read_as_number`] says of each, from one pass over the program instead of one for each).
    pub(super) fn words_are_numbers(&mut self, inst: &Inst, code: u8, count: u32) -> bool {
        if self.bindless.read.is_none() {
            let mut read = HashSet::new();
            for b in &self.x.p.blocks {
                for p in &b.phis {
                    read.extend(p.args.iter().copied());
                }
                for i in &b.insts {
                    if matches!(i.fields, Fields::Sop { .. } | Fields::Valu(_)) {
                        read.extend(i.reads.iter().map(|r| r.1));
                    }
                }
            }
            self.bindless.read = Some(read);
        }
        let read = self.bindless.read.as_ref();
        (0..count).any(|k| {
            inst.write(Reg::S(code + k as u8))
                .is_some_and(|v| read.is_some_and(|r| r.contains(&v)))
        })
    }

    /// Notes that a handle of the view `src` names is made: its table entries are read through their
    /// views by this program.
    pub(super) fn note_view(&mut self, src: &Source) {
        if self.bindless.fallback {
            return;
        }
        for leaf in src.leaves() {
            if matches!(leaf, Source::Loaded { .. } | Source::Fetched { .. })
                && let Some(u) = self.used_kind(leaf)
            {
                self.bindless.viewed.insert((u.slot, u.table_offset_dwords));
            }
        }
    }

    /// The window of the table entry the load `load` reads dword `first` of: the copy's number (0: none),
    /// the byte its view begins at in the copy, its length. Read in the current block.
    fn window_of(&mut self, load: Site, first: u8) -> Result<(V, V, V), Why> {
        let key = (self.cur, load, first);
        if let Some(&w) = self.bindless.windows.get(&key) {
            return Ok(w);
        }
        let c = self.cur;
        let i32t = self.s.t.i32;
        let slot = self.heap_index(load, first, false)?;
        // Inside the table whatever the index (lanes that do not run compute one too).
        let last = self.s.m.i32(SLOT_WINDOWS_COUNT - 1);
        let slot = self
            .s
            .op(c, "binary.i32", Attr::ReadNone, i32t, 40, &[slot, last]);
        self.s.m.tag(slot, Tag::Clamp);
        let k4 = self.s.m.i32(4);
        let byte = self.s.m.bin(c, Bin::Shl, slot, k4);
        let table = self.s.root_srv_handle(SLOT_VIEW_WINDOWS);
        let (iu, mask, align) = (self.s.m.undef(i32t), self.s.m.i8(7), self.s.m.i32(4));
        let r = self.s.op(
            c,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            self.s.t.res_ret,
            139,
            &[table, byte, iu, mask, align],
        );
        let [number, start, len] = [0, 1, 2].map(|k| {
            let f = self.s.m.extract(c, r, k);
            self.as_int(f)
        });
        self.bindless.windows.insert(key, (number, start, len));
        Ok((number, start, len))
    }

    /// `count` dwords from byte `addr` of the buffer the table entry `src` names, as integers, when the
    /// switch is on and the entry is an SRV of a table that is read as raw dwords: through the window
    /// of the entry if the host made one, else through its view. None: not such an entry, nothing
    /// was emitted, and the caller reads the view as it always did.
    pub(super) fn window_dwords(
        &mut self,
        site: Site,
        src: &Source,
        addr: V,
        count: u32,
    ) -> Result<Option<Vec<V>>, Why> {
        if !on() {
            return Ok(None);
        }
        let Source::Loaded { load, first } = src else {
            return Ok(None);
        };
        let (load, first) = (*load, *first);
        let Some(a) = self.scalar.get(&load).copied() else {
            return Ok(None);
        };
        let table = self
            .x
            .native
            .layout
            .get(a.param)
            .is_some_and(|p| p.root == RootType::Table);
        // (A waterfall loop's load at the first lane's offset reads each lane's own entry.)
        if !table || self.x.res.waterfall.loads.contains(&load) {
            return Ok(None);
        }
        let Some(u) = self.used_kind(src) else {
            return Ok(None);
        };
        if u.kind != ResourceType::Srv {
            return Ok(None);
        }
        let entry = (u.slot, u.table_offset_dwords);
        let (number, start, len) = self.window_of(load, first)?;
        self.bindless.windowed.insert(entry);
        let (zero, k4) = (self.s.m.i32(0), self.s.m.i32(4));
        let c = self.cur;
        let i32t = self.s.t.i32;

        // One read, no branch: the heap slot is the copy's raw view with a window and the entry's view (its raw
        // twin, as `handle` makes it) without, and the byte is the window's start past the offset.
        let valid = self.s.m.cmp(c, pred::NE, number, zero);
        let none = self.s.m.cmp(c, pred::EQ, number, zero);
        let index = self.heap_index(load, first, false)?;
        let (index, bounded) = self.raw_twin(src, index, false, (11, 0));
        let base = self.host_word(RootType::Other(USER_WINDOW_BASE), 0, 0);
        let most = self.s.m.i32(WINDOW_COPIES);
        let copy = self
            .s
            .op(c, "binary.i32", Attr::ReadNone, i32t, 40, &[number, most]);
        self.s.m.tag(copy, Tag::Clamp);
        let copy = self.s.m.bin(c, Bin::Add, base, copy);
        let slot = self.s.m.select(c, valid, copy, index);
        let h = self
            .s
            .heap_handle_at(c, slot, false, true, (11, 0), bounded);
        let begin = self.s.m.select(c, valid, start, zero);
        let from = self.s.m.bin(c, Bin::Add, begin, addr);
        let dwords = self.raw_dwords(h, from, count);
        let mut out = Vec::with_capacity(dwords.len());
        for (k, x) in dwords.into_iter().enumerate() {
            let off = self.s.m.i32(4 * k as u32);
            let a = self.s.m.bin(c, Bin::Add, addr, off);
            // Through a window a dword must lie whole inside the view's length: not past its end (`a <= len`, so
            // the difference does not wrap) and at least four bytes left. Without one the view bounds the read.
            let before = self.s.m.cmp(c, pred::IULE, a, len);
            let left = self.s.m.bin(c, Bin::Sub, len, a);
            let room = self.s.m.cmp(c, pred::IUGE, left, k4);
            let inside = self.s.m.bin(c, Bin::And, before, room);
            let fine = self.s.m.bin(c, Bin::Or, inside, none);
            out.push(self.s.m.select(c, fine, x, zero));
        }
        Ok(Some(out))
    }
}
