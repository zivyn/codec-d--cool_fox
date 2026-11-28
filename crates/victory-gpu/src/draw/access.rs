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
                access.read_anywhere();
                if stores {
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
            let any = |t: &Footprint| {
                t.stores
                    .iter()
                    .chain(&t.loads)
                    .any(|s| matches!(s.target, Target::TableAny { .. }))
            };
            if any(footprint) {
                // A descriptor picked by a computed index: any view of the table.
                access.read_anywhere();
                if stores {
                    access.write_anywhere();
                }
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
                        crate::descriptor::View::Shader(resource, _) => access.read([(resource, 0, u64::MAX)]),
                        // (A texture stored to by image instructions is not a buffer store: every unordered view of one counts.)
                        crate::descriptor::View::Unordered(resource, _, _) => {
                            access.read([(resource, 0, u64::MAX)]);
                            access.write([(resource, 0, u64::MAX)]);
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
