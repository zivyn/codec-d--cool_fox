//! Occlusion queries (`VICTORY_REAL_OCCLUSION=1`).
//!
//! The title tests the sun, light halos and flares for visibility with
//! occlusion queries: a heap per buffered frame, a small shape drawn between
//! `BeginQuery` and `EndQuery` for each, and the whole range resolved at the
//! end of the frame into a buffer it reads some frames later. A visible
//! fraction of the shape's samples makes the glow bright, none puts it out.
//! Until now every query was answered as fully visible (`list::resolve_queries`
//! stores a million samples), so a glow showed through walls. With the switch
//! on, a title heap of the occlusion kind has a host heap behind it, the
//! host list gets the same `BeginQuery`/`EndQuery` at the same places among
//! the draws, and the resolve goes to a host read-back buffer whose samples
//! are copied to the title's destination when the list has run, like any
//! other read back.
//!
//! What this does not do: pipeline statistics and stream output heaps keep
//! their made-up answers; a query that begins in one host list and ends in
//! another (a title list cut at a wait between them) is closed at the cut,
//! and the end that comes later is dropped.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Whether occlusion queries run on the host (`VICTORY_REAL_OCCLUSION=1`).
pub(crate) fn real() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_REAL_OCCLUSION").is_some_and(|v| v == "1"))
}

/// The queries a host list has begun and not ended, and the heaps they name
/// (kept until the list is reset: the title may release its heap before the
/// list has run).
#[derive(Debug)]
pub(crate) struct Open<H> {
    /// Heap key, query type, index.
    open: Vec<(usize, u32, u32)>,
    heaps: Vec<(usize, H)>,
}

impl<H> Default for Open<H> {
    fn default() -> Self {
        Open {
            open: Vec::new(),
            heaps: Vec::new(),
        }
    }
}

impl<H: Clone> Open<H> {
    /// Keeps `heap` (named by `key`) with the list.
    pub(crate) fn keep(&mut self, key: usize, heap: &H) {
        if !self.heaps.iter().any(|(k, _)| *k == key) {
            self.heaps.push((key, heap.clone()));
        }
    }

    /// A query begins: false if that very query is already open (a second
    /// begin is dropped, the first one counts).
    pub(crate) fn begin(&mut self, key: usize, heap: &H, kind: u32, index: u32) -> bool {
        if self.open.contains(&(key, kind, index)) {
            return false;
        }
        self.keep(key, heap);
        self.open.push((key, kind, index));
        true
    }

    /// A query ends: false if it is not open in this list (it began in an
    /// earlier part, or never), then nothing is recorded.
    pub(crate) fn end(&mut self, key: usize, kind: u32, index: u32) -> bool {
        match self.open.iter().position(|q| *q == (key, kind, index)) {
            Some(at) => {
                self.open.swap_remove(at);
                true
            }
            None => false,
        }
    }

    /// Every open query, to be ended before the list closes.
    pub(crate) fn drain(&mut self) -> Vec<(H, u32, u32)> {
        let open = std::mem::take(&mut self.open);
        open.into_iter()
            .filter_map(|(key, kind, index)| {
                self.heaps
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, h)| (h.clone(), kind, index))
            })
            .collect()
    }

    /// How many are open.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.open.len()
    }
}

/// The bytes a resolve of `count` queries from `first` into a buffer of
/// `room` bytes at `offset` writes (8 each), if they fit the heap
/// (`capacity` queries) and the buffer.
pub(crate) fn resolve_bytes(
    first: u32,
    count: u32,
    capacity: u32,
    offset: u64,
    room: u64,
) -> Option<u64> {
    if count == 0 || first.checked_add(count)? > capacity {
        return None;
    }
    let bytes = u64::from(count) * 8;
    (offset.checked_add(bytes)? <= room).then_some(bytes)
}

// What the title did with its queries since the last line.
static BEGINS: AtomicU64 = AtomicU64::new(0);
static ENDS: AtomicU64 = AtomicU64::new(0);
/// Resolves answered by the host, queries in them.
static REAL: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Resolves answered with made-up values, queries in them.
static MADE_UP: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Begins and ends that went to no host heap.
static NOT_RUN: AtomicU64 = AtomicU64::new(0);
/// Samples that came back: results, results of nothing, the largest, the sum.
static SAMPLES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// `BeginQuery` was called by the title.
pub(crate) fn began() {
    BEGINS.fetch_add(1, Relaxed);
}

/// `EndQuery` was called by the title.
pub(crate) fn ended() {
    ENDS.fetch_add(1, Relaxed);
}

/// A begin or end that no host list recorded.
pub(crate) fn not_run() {
    NOT_RUN.fetch_add(1, Relaxed);
}

/// The samples of resolved queries, now in title memory.
pub(crate) fn landed(counts: &[u64]) {
    SAMPLES[0].fetch_add(counts.len() as u64, Relaxed);
    SAMPLES[1].fetch_add(counts.iter().filter(|&&c| c == 0).count() as u64, Relaxed);
    SAMPLES[2].fetch_max(counts.iter().copied().max().unwrap_or(0), Relaxed);
    SAMPLES[3].fetch_add(
        counts.iter().fold(0u64, |a, &c| a.saturating_add(c)),
        Relaxed,
    );
}

/// A `ResolveQueryData` of the title: `kind` is the query type, `on_host`
/// whether the host answered it. The first of each shape is logged, then a
/// line every few seconds with the totals.
pub(crate) fn resolved(kind: u32, count: u32, heap: usize, on_host: bool) {
    let side = if on_host { &REAL } else { &MADE_UP };
    side[0].fetch_add(1, Relaxed);
    side[1].fetch_add(u64::from(count), Relaxed);
    static SEEN: std::sync::Mutex<Vec<(u32, u32, bool)>> = std::sync::Mutex::new(Vec::new());
    {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        if seen.len() < 32 && !seen.contains(&(kind, count, on_host)) {
            seen.push((kind, count, on_host));
            tracing::info!(
                query_type = kind,
                count,
                heap = format_args!("{heap:#x}"),
                host = on_host,
                real_occlusion = real(),
                "the title resolves queries"
            );
        }
    }
    tick();
}

/// The totals line, at most every 5 seconds.
fn tick() {
    static LAST: AtomicU64 = AtomicU64::new(0);
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let now = START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64;
    let last = LAST.load(Relaxed);
    if now < last + 5000 || LAST.compare_exchange(last, now, Relaxed, Relaxed).is_err() {
        return;
    }
    let s: Vec<u64> = SAMPLES.iter().map(|a| a.swap(0, Relaxed)).collect();
    let seconds = ((now - last) as f64 / 1000.0).max(0.001);
    tracing::info!(
        "occlusion queries per second (VICTORY_REAL_OCCLUSION={}): begin {:.0}, end {:.0}, \
         resolves answered by the host {:.1} ({:.0} queries), made up {:.1} ({:.0} queries), \
         begin or end not run {:.0}; samples back: {} results, {} of them zero, largest {}, mean {:.0}",
        u8::from(real()),
        BEGINS.swap(0, Relaxed) as f64 / seconds,
        ENDS.swap(0, Relaxed) as f64 / seconds,
        REAL[0].swap(0, Relaxed) as f64 / seconds,
        REAL[1].swap(0, Relaxed) as f64 / seconds,
        MADE_UP[0].swap(0, Relaxed) as f64 / seconds,
        MADE_UP[1].swap(0, Relaxed) as f64 / seconds,
        NOT_RUN.swap(0, Relaxed) as f64 / seconds,
        s[0],
        s[1],
        s[2],
        if s[0] > 0 {
            s[3] as f64 / s[0] as f64
        } else {
            0.0
        }
    );
}

/// The host side: a `BeginQuery` and an `EndQuery` recorded on the host
/// list, and the resolve.
#[cfg(windows)]
pub(crate) mod host {
    use crate::host::{Host, HostList};
    use windows::Win32::Graphics::Direct3D12::{D3D12_QUERY_TYPE, ID3D12QueryHeap};

    /// Begins query `index` of `heap` on `list` (the title's `BeginQuery`).
    pub(crate) fn begin(
        list: &HostList,
        heap: &ID3D12QueryHeap,
        capacity: u32,
        kind: u32,
        index: u32,
    ) {
        if kind > 1 || index >= capacity {
            super::not_run();
            return;
        }
        list.query_begin(heap, kind, index);
    }

    /// Ends query `index` of `heap` on `list` (the title's `EndQuery`).
    pub(crate) fn end(
        list: &HostList,
        heap: &ID3D12QueryHeap,
        capacity: u32,
        kind: u32,
        index: u32,
    ) {
        if kind > 1 || index >= capacity {
            super::not_run();
            return;
        }
        list.query_end(heap, kind, index);
    }

    /// Resolves `count` queries of `heap` from `first` on `list`, to be
    /// copied to `dst` (title memory, `room` bytes from `offset`) once the
    /// list has run. False if it does not fit; nothing is recorded then.
    #[allow(clippy::too_many_arguments)] // the method's own arguments
    pub(crate) fn resolve(
        host: &Host,
        list: &HostList,
        heap: &ID3D12QueryHeap,
        capacity: u32,
        kind: u32,
        first: u32,
        count: u32,
        dst: *mut u8,
        offset: u64,
        room: u64,
    ) -> bool {
        let Some(bytes) = super::resolve_bytes(first, count, capacity, offset, room) else {
            return false;
        };
        let Ok(at) = usize::try_from(offset) else {
            return false;
        };
        let Some(back) = host.read_back_buffer(bytes) else {
            return false;
        };
        // SAFETY: an open list; our heap and read-back buffer, which the
        // list keeps until it is reset; `bytes` fit the buffer.
        unsafe {
            list.list
                .ResolveQueryData(heap, D3D12_QUERY_TYPE(kind as i32), first, count, &back, 0);
        }
        list.query_keep(heap);
        // SAFETY: the title's buffer holds `bytes` from `offset` (checked).
        let to = unsafe { dst.add(at) };
        list.defer_occlusion_back(back, to, bytes as usize);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_begin_of_an_open_query_is_dropped() {
        let mut open = Open::<u32>::default();
        assert!(open.begin(1, &10, 0, 3));
        assert!(!open.begin(1, &10, 0, 3));
        // Another index, another type or another heap is another query.
        assert!(open.begin(1, &10, 0, 4));
        assert!(open.begin(1, &10, 1, 3));
        assert!(open.begin(2, &20, 0, 3));
        assert_eq!(open.len(), 4);
    }

    #[test]
    fn an_end_without_a_begin_in_this_list_is_dropped() {
        let mut open = Open::<u32>::default();
        // The begin was in the part before a cut.
        assert!(!open.end(1, 0, 3));
        assert!(open.begin(1, &10, 0, 3));
        assert!(open.end(1, 0, 3));
        // Each begin has one end.
        assert!(!open.end(1, 0, 3));
        assert_eq!(open.len(), 0);
    }

    #[test]
    fn queries_open_at_the_close_are_handed_back_with_their_heaps() {
        let mut open = Open::<u32>::default();
        assert!(open.begin(1, &10, 0, 3));
        assert!(open.begin(2, &20, 0, 5));
        assert!(open.begin(1, &10, 0, 6));
        assert!(open.end(1, 0, 6));
        let mut left = open.drain();
        left.sort();
        assert_eq!(left, vec![(10, 0, 3), (20, 0, 5)]);
        assert!(open.drain().is_empty());
        // The heaps stay kept until the list is reset (the struct is dropped).
        assert_eq!(open.heaps.len(), 2);
    }

    #[test]
    fn a_resolve_lands_8_bytes_per_query_inside_the_heap_and_the_buffer() {
        // The title resolves a frame's slots from 0 into the start of its buffer.
        assert_eq!(resolve_bytes(0, 5, 64, 0, 40), Some(40));
        assert_eq!(resolve_bytes(0, 5, 64, 0, 39), None);
        // From the middle of the heap, at an offset.
        assert_eq!(resolve_bytes(10, 4, 64, 16, 48), Some(32));
        assert_eq!(resolve_bytes(10, 4, 64, 17, 48), None);
        // Past the heap, nothing at all, or a sum that wraps.
        assert_eq!(resolve_bytes(62, 4, 64, 0, 1 << 20), None);
        assert_eq!(resolve_bytes(0, 0, 64, 0, 1 << 20), None);
        assert_eq!(resolve_bytes(u32::MAX, 2, u32::MAX, 0, 1 << 20), None);
        assert_eq!(resolve_bytes(0, 2, 64, u64::MAX - 8, u64::MAX), None);
    }

    #[test]
    fn the_switch_is_off_unless_asked_for() {
        // (The test process does not set it.)
        if std::env::var_os("VICTORY_REAL_OCCLUSION").is_none() {
            assert!(!real());
        }
    }

    #[test]
    fn samples_that_came_back_are_counted() {
        landed(&[0, 4096, 17]);
        assert!(SAMPLES[0].load(Relaxed) >= 3);
        assert!(SAMPLES[2].load(Relaxed) >= 4096);
    }
}

/// The host list's queries on a real device (the software adapter in CI):
/// two draws that are seen and one that is behind them, through the same
/// methods the title's calls use, and the samples in the buffer the title
/// would read.
#[cfg(all(test, windows))]
mod gpu_tests {
    use crate::host::{compile, host};
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_D32_FLOAT, DXGI_SAMPLE_DESC};
    use windows::core::s;

    /// A triangle that covers the target, at the depth the root constant says.
    const CORNER: &str = r#"
#define RS "RootConstants(num32BitConstants = 1, b0)"
cbuffer c : register(b0) { float depth; };
[RootSignature(RS)]
float4 corner(uint id : SV_VertexID) : SV_Position {
    float2 p = float2((id << 1) & 2, id & 2);
    return float4(p * float2(2, -2) + float2(-1, 1), depth, 1);
}
"#;

    const SIZE: u32 = 64;

    #[test]
    fn occlusion_queries_count_what_a_list_draws_and_land_in_title_memory() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(host) = host() else {
            return;
        };
        let _ = host.log_messages();
        let Ok(list) = host.new_list() else {
            return;
        };
        let Ok(vs) = compile(CORNER, s!("corner"), s!("vs_5_1")) else {
            panic!("the test shader must compile");
        };
        let device = &host.device;
        // SAFETY: plain object creation with valid descriptions; the
        // description points at live data and never releases its pointer.
        let (root, pipeline, depth, heap) = unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &vs[..]).unwrap();
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: vs.as_ptr().cast(),
                    BytecodeLength: vs.len(),
                },
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                    DepthEnable: true.into(),
                    DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                    DepthFunc: D3D12_COMPARISON_FUNC_LESS,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                DSVFormat: DXGI_FORMAT_D32_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let pipeline: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc).unwrap();
            let properties = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let texture = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(SIZE),
                Height: SIZE,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_D32_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
                ..Default::default()
            };
            let clear = D3D12_CLEAR_VALUE {
                Format: DXGI_FORMAT_D32_FLOAT,
                Anonymous: D3D12_CLEAR_VALUE_0 {
                    DepthStencil: D3D12_DEPTH_STENCIL_VALUE {
                        Depth: 1.0,
                        Stencil: 0,
                    },
                },
            };
            let mut depth: Option<ID3D12Resource> = None;
            device
                .CreateCommittedResource(
                    &properties,
                    D3D12_HEAP_FLAG_NONE,
                    &texture,
                    D3D12_RESOURCE_STATE_DEPTH_WRITE,
                    Some(&clear),
                    &mut depth,
                )
                .unwrap();
            let mut heap: Option<ID3D12QueryHeap> = None;
            device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_OCCLUSION,
                        Count: 8,
                        NodeMask: 0,
                    },
                    &mut heap,
                )
                .unwrap();
            (root, pipeline, depth.unwrap(), heap.unwrap())
        };
        let target = host.dsv(&depth, None);
        let draw = |at: f32| {
            // SAFETY: an open list and live objects.
            unsafe {
                list.list.SetGraphicsRoot32BitConstant(0, at.to_bits(), 0);
                list.list.DrawInstanced(3, 1, 0, 0);
            }
        };
        // SAFETY: an open list and live objects, kept by the test until the
        // list has run.
        unsafe {
            list.list.OMSetRenderTargets(0, None, false, Some(&target));
            list.list
                .ClearDepthStencilView(target, D3D12_CLEAR_FLAG_DEPTH, 1.0, 0, Some(&[]));
            list.list.SetGraphicsRootSignature(&root);
            list.list.SetPipelineState(&pipeline);
            list.list.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: SIZE as f32,
                Height: SIZE as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            list.list.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: SIZE as i32,
                bottom: SIZE as i32,
            }]);
            list.list.IASetPrimitiveTopology(
                windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(4),
            );
        }
        // Seen: nothing is in front of it.
        list.query_begin(&heap, 0, 0);
        draw(0.5);
        list.query_end(&heap, 0, 0);
        // Not seen: the first draw wrote 0.5.
        list.query_begin(&heap, 0, 1);
        draw(0.7);
        list.query_end(&heap, 0, 1);
        // Never ended by the title's list: the list ends it at the close.
        list.query_begin(&heap, 0, 2);
        draw(0.3);
        // An end whose begin was in the part before a cut is not recorded.
        list.query_end(&heap, 0, 5);
        list.close();
        // The resolve is in the list after it, as the title's is (the end of
        // the frame): the query that was left open was ended by the close.
        let Ok(after) = host.new_list() else {
            return;
        };
        // The title's buffer: three results and a word that stays as it is.
        let mut title = [0xAAAA_AAAA_AAAA_AAAAu64; 4];
        assert!(super::host::resolve(
            host,
            &after,
            &heap,
            8,
            0,
            0,
            3,
            title.as_mut_ptr().cast(),
            0,
            32
        ));
        // More than the buffer holds is not resolved.
        assert!(!super::host::resolve(
            host,
            &after,
            &heap,
            8,
            0,
            0,
            5,
            title.as_mut_ptr().cast(),
            0,
            32
        ));
        after.close();
        // With the debug layer on: nothing it objects to was recorded.
        assert_eq!(host.log_messages(), None, "the debug layer found an error");
        host.execute(&[&list, &after]);
        list.reset();
        after.reset();
        let pixels = u64::from(SIZE * SIZE);
        assert_eq!(title[0], pixels, "the draw in front is seen by every pixel");
        assert_eq!(title[1], 0, "the draw behind it by none");
        assert_eq!(title[2], pixels, "the query left open counts what it drew");
        assert_eq!(
            title[3], 0xAAAA_AAAA_AAAA_AAAA,
            "nothing past the resolved range is written"
        );
    }
}
