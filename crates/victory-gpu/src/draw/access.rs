//! What the shaders of a dispatch read and write, by view and byte (`State::access`): the store and load footprints of
//! `victory_shader::footprint` evaluated with the groups of the dispatch and the root constants the title set, against
//! the views the title bound. One analysis feeds the declared writes ([`crate::host::precise::declare`]) and the model
//! of the title's UAV waits (`VICTORY_PRECISE_WAITS`).
//!
//! Anything that is not understood says anywhere: a shader whose code was not gone through, title memory by address,
//! a view picked by a computed index or by a root constant, a table that is not set, a view of a kind not listed here.
//! Constant buffers in tables are not counted: they are copied to memory of the list's own when the list is submitted.

use victory_shader::footprint::{Bind, Footprint, Target};
use victory_shader::{ResourceType, RootType};

use super::{Root, State};
use crate::host::precise::{self, Access};
use crate::pipeline::host::{HostPipeline, Stage};

impl State {
    /// What the shaders of the draw or dispatch just bound (`bind` done, the dispatch's groups still given by
    /// [`precise::extent`]) read and write.
    pub(crate) fn access(&self, p: &HostPipeline, compute: bool) -> Access {
        let mut access = Access::none();
        let constants = |slot: u8, dword: u32| match self.roots_ref(compute).get(usize::from(slot)) {
            Some(Root::Constants(v)) => v.get(dword as usize).copied(),
            _ => None,
        };
        for stage in &p.stages {
            let t = stage.translated();
            let stores = t.stores;
            // Title memory by address, and views picked by a number the shader or the title computes.
            if t.host.root_srvs.contains(&victory_shader::emit::SLOT_MEMORY) {
                access.read_anywhere();
                if t.stores_by_address {
                    access.write_anywhere();
                }
            }
            if !t.host.root_indexed_views.is_empty() {
                let picked = picked(stage.shader.footprint().map(|f| &**f), stores, true);
                if picked.reads {
                    access.read_anywhere();
                }
                if picked.writes {
                    access.write_anywhere();
                }
            }
            let threads = if compute { t.threads } else { Some([u32::MAX; 3]) };
            let (Some(footprint), Some(threads)) = (stage.shader.footprint(), threads) else {
                // Not analysed: every view bound counts whole, read and (if it can store) written.
                self.views(&mut access, stage, compute, None, stores);
                continue;
            };
            if footprint.by_address {
                access.write_anywhere();
            }
            if footprint.by_address_loads {
                access.read_anywhere();
            }
            // A descriptor picked by a computed index: any view of the table, a texture as well as a buffer.
            let picked = picked(Some(&**footprint), stores, false);
            if picked.reads {
                access.read_anywhere();
            }
            if picked.writes {
                access.write_anywhere();
            }
            let bind = Bind { threads, groups: precise::groups(), constants: &constants };
            self.views(&mut access, stage, compute, Some((footprint, &bind)), stores);
        }
        access
    }

    /// The views of `stage` as `access` counts them: by `footprint` where it is given, else whole.
    fn views(&self, access: &mut Access, stage: &Stage, compute: bool, footprint: Option<(&Footprint, &Bind)>, stores: bool) {
        {
            let t = stage.translated();
            for u in t.used.iter().filter(|u| u.root == RootType::Table && u.kind != ResourceType::Cbv) {
                let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
                    // (Its table is not set: the view is null, or the draw would have been dropped.)
                    continue;
                };
                let Some(view) = crate::descriptor::view(table + u64::from(u.table_offset_dwords) * 4) else {
                    continue;
                };
                let target = precise::table(u);
                let writes = u.kind == ResourceType::Uav;
                if let Some((address, len)) = crate::descriptor::host::buffer_copy(&view).map(|(_, a, l)| (a, l)) {
                    let reads = footprint.and_then(|(f, b)| f.load_ranges(target, b));
                    access.read(precise::spans(address, len, reads));
                    if writes && stores {
                        let written = footprint.and_then(|(f, b)| f.ranges(target, b));
                        access.write(precise::spans(address, len, written));
                    }
                } else {
                    match view {
                        // (A texture is counted whole, if the code names its descriptor among those it reads or writes
                        // by image instructions, or if it names none it can be sure of.)
                        crate::descriptor::View::Shader(resource, _) => {
                            if images(footprint, |f| &f.image_reads, target) {
                                texture(access, texture_key(resource), true, false);
                            }
                        }
                        crate::descriptor::View::Unordered(resource, _, _) => {
                            let read = images(footprint, |f| &f.image_reads, target);
                            let write = images(footprint, |f| &f.image_writes, target);
                            texture(access, texture_key(resource), read, write);
                        }
                        crate::descriptor::View::Placed(..) => {
                            access.read_anywhere();
                            access.write_anywhere();
                        }
                        _ => {}
                    }
                }
            }
            // Root views: the host copy from the title's address to its end (to the limit of a root view).
            let root = |slot: u8| match self.roots_ref(compute).get(usize::from(slot)) {
                Some(&Root::View(address)) => crate::shadow::find(address, 1).map(|s| (address, s.left(address))),
                _ => None,
            };
            for &(slot, _) in &stage.root_srvs {
                if let Some((address, len)) = root(slot) {
                    access.read(precise::spans(address, len, footprint.and_then(|(f, b)| f.load_ranges(precise::root(slot), b))));
                }
            }
            for &(slot, _) in &stage.root_cbvs {
                if let Some((address, len)) = root(slot) {
                    access.read(precise::spans(address, len.min(1 << 16), None));
                }
            }
            for &(slot, _) in &stage.root_uavs {
                if let Some((address, len)) = root(slot) {
                    let target = precise::root(slot);
                    access.read(precise::spans(address, len, footprint.and_then(|(f, b)| f.load_ranges(target, b))));
                    if stores {
                        access.write(precise::spans(address, len, footprint.and_then(|(f, b)| f.ranges(target, b))));
                    }
                }
            }
        }
    }
}

/// What a shader may do through descriptors it picks by a number: any view of the heap or of a table. `footprint`:
/// the shader's analysis, `None` where it has none; `stores`: it can store to buffers; `heap`: the views are the title's
/// heap picked by a root constant or a computed index (`root_indexed_views`), else by a computed index in a table
/// (`Target::TableAny`). The views picked may be textures, which `stores` does not count (image instructions): a shader
/// that may write one writes anywhere, unless its code is known to have no image write.
pub(crate) fn picked(footprint: Option<&Footprint>, stores: bool, heap: bool) -> Picked {
    let mut picked = Picked::default();
    if heap {
        picked.reads = true;
        picked.writes = stores || footprint.is_none_or(|f| f.images_unknown || !f.image_writes.is_empty());
    }
    if let Some(f) = footprint {
        let by_index = |t: &Target| matches!(t, Target::TableAny { .. });
        let buffers = f.stores.iter().chain(&f.loads).any(|s| by_index(&s.target));
        let write_images = f.image_writes.iter().any(by_index);
        let images = write_images || f.image_reads.iter().any(by_index);
        if buffers || images {
            picked.reads = true;
            picked.writes |= stores || write_images;
        }
    }
    picked
}

/// What [`picked`] says: reads and writes anywhere.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Picked {
    pub(crate) reads: bool,
    pub(crate) writes: bool,
}

/// The key of a texture's spans: its host texture, which two title textures over the same memory share (`alias_of`, a
/// copy of one onto the other), so that their accesses meet. `None` where the title texture has no host texture to
/// say: its accesses are anywhere.
fn texture_key(resource: usize) -> Option<usize> {
    crate::resource::made_copy(resource as crate::object::This).map(|copy| std::sync::Arc::as_ptr(&copy) as usize)
}

/// A dispatch reads and/or writes the whole texture whose spans are keyed `key`; anywhere if it has no key.
fn texture(access: &mut Access, key: Option<usize>, read: bool, write: bool) {
    match key {
        Some(key) => {
            if read {
                access.read([(key, 0, u64::MAX)]);
            }
            if write {
                access.write([(key, 0, u64::MAX)]);
            }
        }
        None => {
            if read {
                access.read_anywhere();
            }
            if write {
                access.write_anywhere();
            }
        }
    }
}

/// Whether the code reads (or writes) the texture of `target` by image instructions: yes where it was not gone through,
/// where an image access could not be attributed, and where its list names the target (or one of its table, picked by
/// a computed index).
fn images(footprint: Option<(&Footprint, &Bind)>, list: fn(&Footprint) -> &Vec<Target>, target: Target) -> bool {
    let Some((f, _)) = footprint else {
        return true;
    };
    f.images_unknown
        || list(f).iter().any(|t| {
            *t == target
                || matches!((t, target), (Target::TableAny { user_data: a }, Target::Table { user_data: b, .. }) if *a == b)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_shader::footprint::{Expr, Store};

    fn store(target: Target) -> Store {
        Store { target, offset: Expr::Const(0), bytes: 4 }
    }

    const TABLE: Target = Target::Table { user_data: 2, dword: 0 };
    const ANY: Target = Target::TableAny { user_data: 2 };

    /// A shader whose views come from the heap by an index writes anywhere if it can write a texture, even where
    /// `stores` (buffer stores) is false: image stores are not in it. Only code known to write no image is bounded.
    #[test]
    fn views_picked_from_the_heap_are_written_anywhere_unless_the_code_writes_no_image() {
        let none = Footprint::default();
        // No analysis, or an image write that could not be attributed, or one that names a descriptor: anywhere.
        assert_eq!(picked(None, false, true), Picked { reads: true, writes: true });
        let unknown = Footprint { images_unknown: true, ..Footprint::default() };
        assert!(picked(Some(&unknown), false, true).writes);
        let writes = Footprint { image_writes: vec![TABLE], ..Footprint::default() };
        assert!(picked(Some(&writes), false, true).writes);
        // The code that is known to store no image, and no buffer: reads anywhere, writes nothing.
        assert_eq!(picked(Some(&none), false, true), Picked { reads: true, writes: false });
        let reads = Footprint { image_reads: vec![TABLE], ..Footprint::default() };
        assert_eq!(picked(Some(&reads), false, true), Picked { reads: true, writes: false });
        // A buffer store through them.
        assert!(picked(Some(&none), true, true).writes);
    }

    /// A descriptor picked by a computed index in a table: a texture written through it is written anywhere (it was
    /// missed: only buffer stores and loads were looked at), a texture or a buffer read through it is read anywhere.
    #[test]
    fn a_computed_index_into_a_table_writes_anywhere_for_a_texture_too() {
        let none = Footprint::default();
        assert_eq!(picked(Some(&none), false, false), Picked::default());
        assert_eq!(picked(Some(&none), true, false), Picked::default());
        let image = Footprint { image_writes: vec![ANY], ..Footprint::default() };
        assert_eq!(picked(Some(&image), false, false), Picked { reads: true, writes: true });
        let read = Footprint { image_reads: vec![ANY], ..Footprint::default() };
        assert_eq!(picked(Some(&read), false, false), Picked { reads: true, writes: false });
        // Buffers as before: stores through it count only where the program can store.
        let buffer = Footprint { stores: vec![store(ANY)], ..Footprint::default() };
        assert_eq!(picked(Some(&buffer), true, false), Picked { reads: true, writes: true });
        assert_eq!(picked(Some(&buffer), false, false), Picked { reads: true, writes: false });
        let load = Footprint { loads: vec![store(ANY)], ..Footprint::default() };
        assert_eq!(picked(Some(&load), false, false), Picked { reads: true, writes: false });
        // An entry of the table that is named is no computed pick.
        let named = Footprint { image_writes: vec![TABLE], stores: vec![store(TABLE)], ..Footprint::default() };
        assert_eq!(picked(Some(&named), true, false), Picked::default());
    }

    /// Two title textures that share a host texture have the same key, so the write of one and the read of the other
    /// meet: a wait held between them is made. Textures with other host textures do not meet; a texture without a host
    /// texture is anywhere, and meets everything that was written.
    #[test]
    fn aliased_textures_meet_and_the_rest_do_not() {
        use crate::lazy_wait::Lazy;
        // A write to a texture keyed `first`, a title wait, then a read of one keyed `second`: whether a wait is made
        // in front of the read.
        let after_write = |first: Option<usize>, second: Option<usize>| {
            let mut lazy = Lazy::new();
            // (The first wait of a list is for what earlier lists left: the first command takes it.)
            lazy.title_wait();
            let mut earlier = Access::none();
            texture(&mut earlier, Some(1), true, false);
            assert!(lazy.command(&earlier));
            let mut written = Access::none();
            texture(&mut written, first, false, true);
            lazy.command(&written);
            lazy.title_wait();
            let mut read = Access::none();
            texture(&mut read, second, true, false);
            lazy.command(&read)
        };
        assert!(after_write(Some(7), Some(7)), "two title textures over one host texture: read after write");
        assert!(!after_write(Some(7), Some(8)), "two host textures");
        assert!(after_write(Some(7), None), "a texture with no host texture is read anywhere");
        assert!(after_write(None, Some(8)), "a texture with no host texture is written anywhere");
        // Write after read, and write after write, of one host texture.
        let after = |first: (bool, bool), second: (bool, bool)| {
            let mut lazy = Lazy::new();
            lazy.title_wait();
            let mut earlier = Access::none();
            texture(&mut earlier, Some(1), true, false);
            assert!(lazy.command(&earlier));
            let mut a = Access::none();
            texture(&mut a, Some(5), first.0, first.1);
            lazy.command(&a);
            lazy.title_wait();
            let mut b = Access::none();
            texture(&mut b, Some(5), second.0, second.1);
            lazy.command(&b)
        };
        assert!(after((true, false), (false, true)), "write after read");
        assert!(after((false, true), (false, true)), "write after write");
        assert!(!after((true, false), (true, false)), "read after read");
    }
}
