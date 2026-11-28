//! Diagnostics (`VICTORY_BUFFER_NAN`, see `crate::bufscan`): after a dispatch of a watched program, the
//! spans of buffers it wrote go to the scan. The spans are what `State::access` says the program's
//! stores can reach with the groups and root constants of this dispatch; where that is not bounded
//! the dispatch is counted and not scanned.

use victory_shader::{ResourceType, RootType};

use super::{Root, State};
use crate::bufscan::{self, Item, SlotInfo, count};
use crate::host::HostList;
use crate::pipeline::host::HostPipeline;

/// A buffer view a program writes: where it starts (title address), its length and its record size.
type View = (u64, u64, u64);

impl State {
    /// Records the scans of what the dispatch of `p` just recorded wrote. Replaces the list's compute
    /// root signature and pipeline when it returns `true` (the caller binds them again).
    pub(crate) fn buffer_scan(&mut self, list: &HostList, p: &HostPipeline) -> bool {
        if !bufscan::on() {
            return false;
        }
        let Some(stage) = p.stages.first() else {
            return false;
        };
        let hash = &stage.shader.hash;
        let Some(name) = bufscan::watched(hash) else {
            return false;
        };
        bufscan::bump(count::WATCHED, 1);
        let Some(gpu) = bufscan::gpu() else {
            return false;
        };
        let Some(spans) = self.access(p, true).writes else {
            bufscan::bump(count::ANYWHERE, 1);
            return false;
        };
        let ranges = spans
            .iter()
            .filter(|s| s.0 == 0)
            .map(|s| (s.1, s.2))
            .collect();
        let views = self.written_views(p);
        let frame = crate::frame::current();
        let mut items = Vec::new();
        for (from, to) in bufscan::merge(ranges) {
            if to - from > bufscan::MOST_SPAN {
                bufscan::bump(count::BIG, 1);
                continue;
            }
            if !bufscan::admit(frame) {
                bufscan::bump(count::CAPPED, 1);
                continue;
            }
            let Some(shadow) = crate::shadow::find(from, to - from) else {
                bufscan::bump(count::NO_COPY, 1);
                continue;
            };
            let (view, len, stride) = views
                .iter()
                .copied()
                .find(|v| v.0 <= from && to <= v.0 + v.1)
                .unwrap_or((from, to - from, 0));
            let info = SlotInfo {
                hash: hash.clone(),
                name: name.clone(),
                view,
                len,
                stride,
            };
            let Some(slot) = bufscan::slot(shadow.id(), &info) else {
                bufscan::bump(count::NO_SLOT, 1);
                continue;
            };
            let (Ok(dwords), Ok(base)) = (
                u32::try_from((to - from) / 4),
                u32::try_from((from - view) / 4),
            ) else {
                continue;
            };
            bufscan::bump(count::SPANS, 1);
            bufscan::bump(count::BYTES, to - from);
            items.push(Item {
                gpu: shadow.gpu(from),
                dwords,
                base,
                slot,
                layout: bufscan::layout(stride),
            });
        }
        bufscan::record(&list.list, &gpu.scanner, gpu.counters_gpu, &items);
        !items.is_empty()
    }

    /// The buffer views the stage of `p` can write that are bound now: by table (with the record size
    /// the program declares) and by root view (size unknown).
    fn written_views(&self, p: &HostPipeline) -> Vec<View> {
        let mut views = Vec::new();
        for stage in &p.stages {
            let t = stage.translated();
            for u in t
                .used
                .iter()
                .filter(|u| u.root == RootType::Table && u.kind == ResourceType::Uav)
            {
                let Some(&Root::Table(table)) = self.roots_ref(true).get(usize::from(u.slot))
                else {
                    continue;
                };
                let Some(view) =
                    crate::descriptor::view(table + u64::from(u.table_offset_dwords) * 4)
                else {
                    continue;
                };
                if let Some((address, len)) = crate::descriptor::host::buffer_range(&view) {
                    views.push((address, len, u64::from(u.stride)));
                }
            }
            for &(slot, _) in &stage.root_uavs {
                if let Some(&Root::View(address)) = self.roots_ref(true).get(usize::from(slot))
                    && let Some(shadow) = crate::shadow::find(address, 1)
                {
                    views.push((address, shadow.left(address), 0));
                }
            }
        }
        views
    }
}
