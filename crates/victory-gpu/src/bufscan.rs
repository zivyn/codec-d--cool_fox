//! Diagnostics: the buffers the title's compute programs write, scanned on the card for floats that
//! are not numbers (`VICTORY_BUFFER_NAN`, diagnostics builds; off by default).
//!
//! The census of float render targets cannot see the state a character pass keeps from frame to
//! frame (the hair simulation's vertex records, the skinned vertices, the previous pose): it lives in
//! structured and raw buffers. With `VICTORY_BUFFER_NAN=@<file>` (one 16-digit program hash a line,
//! `# name` after it is only shown in the log; the list is the user's and not in the repository) or
//! `=all`, after every dispatch of those programs the spans of buffers the program wrote (what its
//! footprint says it stores; the whole view where the program was not analysed) are scanned by a compute shader of ours:
//! a reduction that counts, per (buffer, program) slot, the dwords that are NaN or infinite as 32-bit
//! floats and, for the fields of a record that hold 16-bit floats, the halves that are, by the field
//! of the record, and keeps the first one and the order in which slots first hit. Nothing is read
//! back until a present that is due (`VICTORY_BUFFER_NAN_EVERY` frames, 5 by default): then the
//! few kilobytes of counters are copied, and a slot that has newly counted something is logged once
//! with the frame, the program, the buffer, the first element (record and field) and the counts by
//! field; later growth is summarised.
//!
//! Counters only grow, so a persistent non-finite value shows as a count that rises by the size of
//! the scanned span each frame, and an integer field that happens to look like one shows as a count
//! that is there from the first scan and sits in one field: the field histogram tells them apart.
//!
//! `VICTORY_BUFFER_NAN_MAX` bounds the scans of a frame (4000 by default). A dispatch whose writes the
//! analysis cannot bound, and a span past 8 MiB, are counted and not scanned.
//!
//! The kernel and the bookkeeping here compile everywhere and are tested (on the software adapter);
//! what finds the spans after a dispatch is `crate::draw::bufnan`, in diagnostics builds.

#![cfg_attr(not(feature = "diagnostics"), allow(dead_code))]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::s;

/// Dwords of counters per slot.
pub(crate) const WIDTH: usize = 40;
/// Slots, the first of them the header (dword 0: the order counter).
pub(crate) const SLOTS: usize = 1024;
const BYTES: u64 = (SLOTS * WIDTH * 4) as u64;
/// The most bytes of one span that is scanned.
pub(crate) const MOST_SPAN: u64 = 8 << 20;

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=8, b0), UAV(u0), UAV(u1)"
#define W 40
cbuffer Scan : register(b0) {
    uint dwords;     // dwords in the span
    uint base;       // the span's first dword, counted from the start of the view
    uint stride;     // record dwords, 1..32
    uint slot;
    uint f32_mask;   // record dwords that are 32-bit floats
    uint f16_mask;   // record dwords that are two 16-bit floats
    uint f16_low;    // record dwords whose low half is a 16-bit float
    uint threads;    // threads of the dispatch
};
RWByteAddressBuffer src : register(u0);
RWByteAddressBuffer res : register(u1);

void first_hit(uint o, uint count_at, uint order_at) {
    uint prev;
    res.InterlockedAdd(o + count_at * 4, 1, prev);
    if (prev == 0) {
        uint seq;
        res.InterlockedAdd(0, 1, seq);
        res.Store(o + order_at * 4, seq + 1);
    }
}

[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint o = slot * W * 4;
    if (id.x == 0) {
        uint unused;
        res.InterlockedAdd(o + 6 * 4, dwords, unused);
    }
    for (uint i = id.x; i < dwords; i += threads) {
        uint v = src.Load(i * 4);
        uint at = base + i;
        uint f = at % stride;
        uint bit = 1u << f;
        uint h = min(f, 15u);
        uint unused;
        if ((f32_mask & bit) != 0 && (v & 0x7F800000u) == 0x7F800000u) {
            first_hit(o, 0, 2);
            res.InterlockedAdd(o + (8 + h) * 4, 1, unused);
            res.InterlockedMax(o + 4 * 4, 0xFFFFFFFFu - at, unused);
        }
        uint halves = 0;
        if ((f16_mask & bit) != 0) {
            halves += (((v >> 10) & 31u) == 31u) ? 1u : 0u;
            halves += (((v >> 26) & 31u) == 31u) ? 1u : 0u;
        }
        if ((f16_low & bit) != 0) {
            halves += (((v >> 10) & 31u) == 31u) ? 1u : 0u;
        }
        if (halves != 0) {
            uint prev;
            res.InterlockedAdd(o + 1 * 4, halves, prev);
            if (prev == 0) {
                uint seq;
                res.InterlockedAdd(0, 1, seq);
                res.Store(o + 3 * 4, seq + 1);
            }
            res.InterlockedAdd(o + (24 + h) * 4, halves, unused);
            res.InterlockedMax(o + 5 * 4, 0xFFFFFFFFu - at, unused);
        }
    }
}
"#;

/// What the kernel takes of a record: its length in dwords and which dwords hold what.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) stride: u32,
    pub(crate) f32_mask: u32,
    pub(crate) f16_mask: u32,
    pub(crate) f16_low: u32,
}

/// The layout of records of `stride` bytes. A record of 36 bytes is the hair simulation's vertex: six
/// 32-bit floats, then two dwords of 16-bit floats (the first holds two, the second one in its low
/// half) and a dword of flags. Any other is read as 32-bit floats throughout, which is right for
/// vertex data and wrong for integer fields (the field histogram shows them).
pub(crate) fn layout(stride: u64) -> Layout {
    let dwords = if stride % 4 == 0 && (4..=128).contains(&stride) {
        (stride / 4) as u32
    } else {
        1
    };
    if dwords == 9 {
        return Layout {
            stride: 9,
            f32_mask: 0x3F,
            f16_mask: 0x40,
            f16_low: 0x80,
        };
    }
    let all = if dwords >= 32 {
        u32::MAX
    } else {
        (1u32 << dwords) - 1
    };
    Layout {
        stride: dwords,
        f32_mask: all,
        f16_mask: 0,
        f16_low: 0,
    }
}

/// Byte ranges `[from, to)` sorted, merged where they touch or overlap, with each end moved out to a
/// multiple of 4.
pub(crate) fn merge(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.retain(|r| r.0 < r.1);
    for r in &mut ranges {
        r.0 &= !3;
        r.1 = r.1.next_multiple_of(4);
    }
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.0 <= last.1 => last.1 = last.1.max(r.1),
            _ => out.push(r),
        }
    }
    out
}

/// The programs to watch: `None` is all of them (`=all`), else hash to the name the list gave it.
pub(crate) fn parse_list(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let (hash, name) = line.split_once('#').unwrap_or((line, ""));
        let hash = hash.trim();
        if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            out.insert(hash.to_ascii_lowercase(), name.trim().to_owned());
        }
    }
    out
}

struct Settings {
    /// `None`: every compute program.
    watch: Option<HashMap<String, String>>,
    every: u64,
    most: u64,
}

fn settings() -> Option<&'static Settings> {
    static SETTINGS: OnceLock<Option<Settings>> = OnceLock::new();
    SETTINGS
        .get_or_init(|| {
            let value = std::env::var("VICTORY_BUFFER_NAN").ok()?;
            let value = value.trim();
            let watch = match value {
                "" | "0" => return None,
                "all" => None,
                _ => {
                    let path = value.strip_prefix('@').map(std::path::PathBuf::from).or_else(|| {
                        std::env::current_exe().ok().map(|exe| exe.with_file_name("buffer-nan.txt"))
                    })?;
                    match std::fs::read_to_string(&path) {
                        Ok(text) => Some(parse_list(&text)),
                        Err(e) => {
                            tracing::warn!(error = %e, path = %path.display(), "VICTORY_BUFFER_NAN: the list of programs cannot be read; nothing is scanned");
                            return None;
                        }
                    }
                }
            };
            let number = |name: &str, default: u64| {
                std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).filter(|&n| n > 0).unwrap_or(default)
            };
            let settings = Settings {
                watch,
                every: number("VICTORY_BUFFER_NAN_EVERY", 5),
                most: number("VICTORY_BUFFER_NAN_MAX", 4000),
            };
            tracing::info!(
                programs = settings.watch.as_ref().map_or(usize::MAX, HashMap::len),
                every = settings.every,
                most = settings.most,
                "VICTORY_BUFFER_NAN: buffers written by the listed compute programs are scanned for NaN and infinities"
            );
            Some(settings)
        })
        .as_ref()
}

/// Whether a scan is asked for at all.
pub(crate) fn on() -> bool {
    settings().is_some()
}

/// The name the list gives the program `hash`, if it is watched (empty for `=all` or no name).
pub(crate) fn watched(hash: &str) -> Option<String> {
    let s = settings()?;
    match &s.watch {
        None => Some(String::new()),
        Some(map) => map.get(&hash.to_ascii_lowercase()).cloned(),
    }
}

/// How the scans went, for the summary line.
pub(crate) static COUNT: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
/// Indexes of [`COUNT`].
pub(crate) mod count {
    /// Dispatches of watched programs.
    pub(crate) const WATCHED: usize = 0;
    /// Spans scanned.
    pub(crate) const SPANS: usize = 1;
    /// Bytes scanned.
    pub(crate) const BYTES: usize = 2;
    /// Dispatches whose writes the analysis does not bound.
    pub(crate) const ANYWHERE: usize = 3;
    /// Spans past the limit for one.
    pub(crate) const BIG: usize = 4;
    /// Spans left out by the frame's limit.
    pub(crate) const CAPPED: usize = 5;
    /// Spans with no slot left.
    pub(crate) const NO_SLOT: usize = 6;
    /// Spans with no host copy.
    pub(crate) const NO_COPY: usize = 7;
}

pub(crate) fn bump(which: usize, by: u64) {
    COUNT[which].fetch_add(by, Ordering::Relaxed);
}

/// The scans of this frame so far, and the frame.
static FRAME_SCANS: Mutex<(u64, u64)> = Mutex::new((0, 0));

/// Whether another scan fits in this frame's limit.
pub(crate) fn admit(frame: u64) -> bool {
    let Some(s) = settings() else { return false };
    let mut f = FRAME_SCANS.lock().unwrap_or_else(|e| e.into_inner());
    if f.0 != frame {
        *f = (frame, 0);
    }
    f.1 += 1;
    f.1 <= s.most
}

/// What a slot is: the buffer view and the program that wrote it.
#[derive(Clone, Debug)]
pub(crate) struct SlotInfo {
    pub(crate) hash: String,
    pub(crate) name: String,
    /// Where the view starts (title address) and how long it is.
    pub(crate) view: u64,
    pub(crate) len: u64,
    pub(crate) stride: u64,
}

#[derive(Default)]
struct Registry {
    by_key: HashMap<(u64, u64, String), usize>,
    info: Vec<SlotInfo>,
    /// The counts at the last sample (32-bit, 16-bit), and whether the slot was reported.
    seen: Vec<([u32; 2], bool)>,
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

/// The slot (from 1) of the buffer `shadow` (its number), view `view` written by program `hash`,
/// made on first sight; `None` with no slot left.
pub(crate) fn slot(shadow: u64, info: &SlotInfo) -> Option<u32> {
    let mut guard = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let reg = guard.get_or_insert_with(Registry::default);
    let key = (shadow, info.view, info.hash.clone());
    if let Some(&i) = reg.by_key.get(&key) {
        return Some(i as u32 + 1);
    }
    if reg.info.len() + 1 >= SLOTS {
        return None;
    }
    reg.info.push(info.clone());
    reg.seen.push(([0; 2], false));
    let i = reg.info.len() - 1;
    reg.by_key.insert(key, i);
    Some(i as u32 + 1)
}

/// What the kernel counted for a slot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) n32: u32,
    pub(crate) n16: u32,
    /// Which slot to count something first (from 1), 0 if none.
    pub(crate) order32: u32,
    pub(crate) order16: u32,
    /// The first dword (from the start of the view) that was one.
    pub(crate) first32: Option<u32>,
    pub(crate) first16: Option<u32>,
    pub(crate) scanned: u32,
    pub(crate) by_field32: [u32; 16],
    pub(crate) by_field16: [u32; 16],
}

/// The counters of slot `slot` in the copied results.
pub(crate) fn row(data: &[u32], slot: usize) -> Row {
    let w = &data[slot * WIDTH..(slot + 1) * WIDTH];
    let first = |v: u32| (v != 0).then(|| u32::MAX - v);
    let mut r = Row {
        n32: w[0],
        n16: w[1],
        order32: w[2],
        order16: w[3],
        first32: first(w[4]),
        first16: first(w[5]),
        scanned: w[6],
        ..Row::default()
    };
    r.by_field32.copy_from_slice(&w[8..24]);
    r.by_field16.copy_from_slice(&w[24..40]);
    r
}

/// Record and field of the dword `at` of a view of records of `stride` bytes.
pub(crate) fn element(at: u32, stride: u64) -> (u64, u64) {
    let dwords = (stride / 4).max(1);
    (u64::from(at) / dwords, u64::from(at) % dwords)
}

/// What to say of a slot that has newly counted something.
pub(crate) fn describe(i: &SlotInfo, r: &Row) -> String {
    let fields = |counts: &[u32; 16]| {
        counts
            .iter()
            .enumerate()
            .filter(|c| *c.1 != 0)
            .map(|(f, n)| format!("{f}:{n}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let first = |at: Option<u32>| match at {
        Some(at) => {
            let (record, field) = element(at, i.stride);
            format!("record {record} field {field}")
        }
        None => "none".to_owned(),
    };
    format!(
        "program {} {} buffer {:#x} ({} bytes, records of {} bytes): 32-bit {} (first at {}, by field [{}], came {}), 16-bit {} (first at {}, by field [{}], came {}), {} dwords scanned",
        i.hash,
        i.name,
        i.view,
        i.len,
        i.stride,
        r.n32,
        first(r.first32),
        fields(&r.by_field32),
        r.order32,
        r.n16,
        first(r.first16),
        fields(&r.by_field16),
        r.order16,
        r.scanned
    )
}

/// The kernel and its root signature.
pub(crate) struct Scanner {
    pub(crate) root: ID3D12RootSignature,
    pub(crate) pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Scanner {}
// SAFETY: as above.
unsafe impl Sync for Scanner {}

/// Compiles the kernel (HLSL 5.1 with its root signature inside, as the argument clamp is made).
pub(crate) fn make(device: &ID3D12Device) -> windows::core::Result<Scanner> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("bufscan"),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            tracing::warn!("buffer scan: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes; the root signature is inside the shader.
    unsafe {
        let bytes =
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok(Scanner { root, pso })
    }
}

/// A buffer of `size` bytes in the card's memory that shaders write, in the unordered access state
/// and kept there (a buffer that starts in the common state and is promoted by use may fall back to
/// it between lists).
pub(crate) fn counters(device: &ID3D12Device, size: u64) -> windows::core::Result<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation of a device-local buffer.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            None,
            &mut buffer,
        )?;
    }
    buffer.ok_or_else(windows::core::Error::empty)
}

/// One span to scan.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Item {
    /// The host GPU address of its first dword.
    pub(crate) gpu: u64,
    pub(crate) dwords: u32,
    /// Its first dword's place in the view.
    pub(crate) base: u32,
    pub(crate) slot: u32,
    pub(crate) layout: Layout,
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    }
}

/// Records the scans of `items` into `list`, behind a wait for every earlier shader write and in front
/// of one for these reads. Replaces the list's compute root signature and pipeline.
pub(crate) fn record(
    list: &ID3D12GraphicsCommandList,
    scanner: &Scanner,
    counters_gpu: u64,
    items: &[Item],
) {
    if items.is_empty() {
        return;
    }
    // SAFETY: an open list; our root signature, pipeline and counters; the addresses are of live
    // buffers the caller found.
    unsafe {
        list.ResourceBarrier(&[uav_barrier()]);
        list.SetComputeRootSignature(&scanner.root);
        list.SetPipelineState(&scanner.pso);
        for item in items {
            let groups = item.dwords.div_ceil(64).clamp(1, 256);
            let c = [
                item.dwords,
                item.base,
                item.layout.stride,
                item.slot,
                item.layout.f32_mask,
                item.layout.f16_mask,
                item.layout.f16_low,
                groups * 64,
            ];
            list.SetComputeRoot32BitConstants(0, 8, c.as_ptr().cast(), 0);
            list.SetComputeRootUnorderedAccessView(1, item.gpu);
            list.SetComputeRootUnorderedAccessView(2, counters_gpu);
            list.Dispatch(groups, 1, 1);
        }
        list.ResourceBarrier(&[uav_barrier()]);
    }
}

/// The card's side of the scan: the kernel, the counters and a buffer to read them back through.
pub(crate) struct Gpu {
    pub(crate) scanner: Scanner,
    pub(crate) counters: ID3D12Resource,
    pub(crate) counters_gpu: u64,
    readback: ID3D12Resource,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Gpu {}
// SAFETY: as above.
unsafe impl Sync for Gpu {}

#[cfg(windows)]
pub(crate) fn gpu() -> Option<&'static Gpu> {
    static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
    GPU.get_or_init(|| {
        settings()?;
        let host = crate::host::host()?;
        let made = (|| -> windows::core::Result<Option<Gpu>> {
            let scanner = make(&host.device)?;
            let counters = counters(&host.device, BYTES)?;
            // SAFETY: a live resource.
            let counters_gpu = unsafe { counters.GetGPUVirtualAddress() };
            let Some(readback) = host.read_back_buffer(BYTES) else { return Ok(None) };
            Ok(Some(Gpu { scanner, counters, counters_gpu, readback }))
        })();
        match made {
            Ok(gpu) => gpu,
            Err(e) => {
                tracing::warn!(error = %e, "VICTORY_BUFFER_NAN: no scan kernel; nothing is scanned");
                None
            }
        }
    })
    .as_ref()
}

static LAST: AtomicU64 = AtomicU64::new(0);

/// Whether the counters are looked at at this present.
pub(crate) fn due(frame: u64) -> bool {
    let Some(s) = settings() else { return false };
    if frame == 0 || frame % s.every != 0 {
        return false;
    }
    LAST.swap(frame, Ordering::Relaxed) != frame && gpu().is_some()
}

/// Copies the counters back, reports the slots that newly count something, and now and then the totals.
pub(crate) fn sample(frame: u64) {
    let Some(host) = crate::host::host() else {
        return;
    };
    let Some(g) = gpu() else { return };
    let Ok(list) = host.new_list() else { return };
    // SAFETY: an open list of ours and live buffers; the counters are kept in the unordered access
    // state, which the copy leaves them in.
    unsafe {
        list.list.ResourceBarrier(&[crate::host::transition(
            &g.counters,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        list.list
            .CopyBufferRegion(&g.readback, 0, &g.counters, 0, BYTES);
        list.list.ResourceBarrier(&[crate::host::transition(
            &g.counters,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);
    }
    list.close();
    host.execute(&[&list]);
    let mut data = vec![0u32; SLOTS * WIDTH];
    let mut mapped = std::ptr::null_mut();
    // SAFETY: the copy finished; the mapping spans the buffer.
    let ok = unsafe {
        let ok = g.readback.Map(0, None, Some(&mut mapped)).is_ok() && !mapped.is_null();
        if ok {
            std::ptr::copy_nonoverlapping(mapped.cast::<u32>(), data.as_mut_ptr(), data.len());
            g.readback.Unmap(0, None);
        }
        ok
    };
    if !ok {
        return;
    }
    let mut guard = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let Some(reg) = guard.as_mut() else { return };
    let mut growing = Vec::new();
    for i in 0..reg.info.len() {
        let r = row(&data, i + 1);
        let (before, reported) = reg.seen[i];
        if r.n32 == 0 && r.n16 == 0 {
            continue;
        }
        if !reported {
            tracing::warn!(frame, "BUFFER_NAN first: {}", describe(&reg.info[i], &r));
        } else if r.n32 != before[0] || r.n16 != before[1] {
            growing.push((
                i,
                r.n32.wrapping_sub(before[0]),
                r.n16.wrapping_sub(before[1]),
            ));
        }
        reg.seen[i] = ([r.n32, r.n16], true);
    }
    if !growing.is_empty() {
        growing.sort_by_key(|g| std::cmp::Reverse(g.1.saturating_add(g.2)));
        let top: Vec<String> = growing
            .iter()
            .take(6)
            .map(|(i, d32, d16)| {
                format!(
                    "{} {:#x} +{d32}/+{d16}",
                    reg.info[*i].hash, reg.info[*i].view
                )
            })
            .collect();
        tracing::warn!(
            frame,
            slots = growing.len(),
            "BUFFER_NAN growing since the last look (32-bit/16-bit counts): {}",
            top.join(", ")
        );
    }
    if frame % 120 < 5 {
        let c: Vec<u64> = COUNT.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        tracing::info!(
            frame,
            watched_dispatches = c[count::WATCHED],
            spans = c[count::SPANS],
            mib = c[count::BYTES] >> 20,
            writes_not_bounded = c[count::ANYWHERE],
            spans_too_big = c[count::BIG],
            spans_over_frame_limit = c[count::CAPPED],
            no_slot = c[count::NO_SLOT],
            no_host_copy = c[count::NO_COPY],
            slots = reg.info.len(),
            "BUFFER_NAN coverage so far"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    use windows::core::Interface;

    #[test]
    fn layouts_name_the_fields_of_the_records_they_know() {
        assert_eq!(
            layout(36),
            Layout {
                stride: 9,
                f32_mask: 0x3F,
                f16_mask: 0x40,
                f16_low: 0x80
            }
        );
        assert_eq!(
            layout(16),
            Layout {
                stride: 4,
                f32_mask: 0xF,
                f16_mask: 0,
                f16_low: 0
            }
        );
        assert_eq!(layout(128).stride, 32);
        assert_eq!(layout(128).f32_mask, u32::MAX);
        // an odd or huge stride is one dword a record
        assert_eq!(layout(0).stride, 1);
        assert_eq!(layout(6).stride, 1);
        assert_eq!(layout(4096).stride, 1);
    }

    #[test]
    fn spans_are_merged_and_made_whole_dwords() {
        assert_eq!(
            merge(vec![(10, 20), (0, 3), (3, 5), (40, 44)]),
            vec![(0, 20), (40, 44)]
        );
        assert_eq!(merge(vec![(5, 5), (7, 6)]), Vec::<(u64, u64)>::new());
        assert_eq!(merge(vec![(1, 2)]), vec![(0, 4)]);
        assert_eq!(merge(vec![(0, 8), (4, 6), (8, 12)]), vec![(0, 12)]);
    }

    #[test]
    fn the_list_of_programs_takes_hashes_and_names() {
        let list = "# comment\n3A79195F06D3ED85 # one program\n\nnot a hash\n0123456789abcdef\n123 # short\n";
        let map = parse_list(list);
        assert_eq!(map.len(), 2);
        assert_eq!(map["3a79195f06d3ed85"], "one program");
        assert_eq!(map["0123456789abcdef"], "");
    }

    #[test]
    fn a_dword_is_placed_in_its_record_and_field() {
        assert_eq!(element(11, 36), (1, 2));
        assert_eq!(element(0, 36), (0, 0));
        assert_eq!(element(5, 0), (5, 0));
        let mut data = vec![0u32; 3 * WIDTH];
        let w = &mut data[WIDTH..2 * WIDTH];
        w[0] = 3;
        w[2] = 1;
        w[4] = u32::MAX - 11;
        w[8 + 2] = 3;
        let r = row(&data, 1);
        assert_eq!(
            (r.n32, r.order32, r.first32, r.first16),
            (3, 1, Some(11), None)
        );
        assert_eq!(r.by_field32[2], 3);
        let info = SlotInfo {
            hash: "h".into(),
            name: "n".into(),
            view: 0x1000,
            len: 72,
            stride: 36,
        };
        let text = describe(&info, &r);
        assert!(
            text.contains("record 1 field 2") && text.contains("by field [2:3]"),
            "{text}"
        );
    }

    struct Harness {
        device: ID3D12Device,
        queue: ID3D12CommandQueue,
        allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        fence: ID3D12Fence,
        event: HANDLE,
        _turn: std::sync::MutexGuard<'static, ()>,
    }

    impl Harness {
        fn new() -> Option<Harness> {
            let turn = crate::indirect::tests::warp_turn();
            // SAFETY: plain DXGI query and D3D12 test harness setup.
            unsafe {
                let factory: IDXGIFactory4 =
                    CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
                let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
                let mut device: Option<ID3D12Device> = None;
                D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
                let device = device?;
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                        ..Default::default()
                    })
                    .ok()?;
                let allocator: ID3D12CommandAllocator = device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                    .ok()?;
                let list: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                    .ok()?;
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
                let event = CreateEventW(None, false, false, None).ok()?;
                Some(Harness {
                    device,
                    queue,
                    allocator,
                    list,
                    fence,
                    event,
                    _turn: turn,
                })
            }
        }

        /// A buffer in L0 memory the CPU maps, shaders may write.
        fn shared(&self, words: &[u32]) -> (ID3D12Resource, u64) {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: (words.len() * 4).max(4) as u64,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                ..Default::default()
            };
            let mut buffer: Option<ID3D12Resource> = None;
            let mut data = std::ptr::null_mut();
            // SAFETY: plain buffer creation and a mapping that spans it.
            unsafe {
                self.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut buffer,
                    )
                    .unwrap();
                let buffer = buffer.unwrap();
                buffer.Map(0, None, Some(&mut data)).unwrap();
                std::ptr::copy_nonoverlapping(words.as_ptr(), data.cast::<u32>(), words.len());
                let gpu = buffer.GetGPUVirtualAddress();
                (buffer, gpu)
            }
        }

        fn wait(&self, value: u64) {
            // SAFETY: a live queue, fence and event.
            unsafe {
                self.list.Close().unwrap();
                let cmd: ID3D12CommandList = self.list.cast().unwrap();
                self.queue.ExecuteCommandLists(&[Some(cmd)]);
                self.queue.Signal(&self.fence, value).unwrap();
                self.fence.SetEventOnCompletion(value, self.event).unwrap();
                WaitForSingleObject(self.event, INFINITE);
            }
        }
    }

    /// The kernel on the software adapter: two records of the 36-byte layout, one with a NaN and an
    /// infinity in the floats, a NaN in the first 16-bit dword's high half and an infinity in the
    /// second's low half, and an integer dword (all ones) that is nothing; and a clean buffer.
    #[test]
    fn the_kernel_counts_what_is_not_a_number_by_field() {
        let Some(h) = Harness::new() else {
            eprintln!("SKIPPED: no software adapter");
            return;
        };
        let scanner = make(&h.device).expect("the kernel compiles");
        let counters = counters(&h.device, BYTES).unwrap();
        let one = 1.0f32.to_bits();
        let mut words = vec![one; 18];
        words[9 + 2] = 0x7FC0_0000;
        words[9 + 3] = 0x7F80_0000;
        words[9 + 6] = 0x7E00_3C00;
        words[9 + 7] = 0xFFFF_7C00;
        words[9 + 8] = 0xFFFF_FFFF;
        words[6] = 0x3C00_3C00;
        words[7] = 0x1234_3C00;
        words[8] = 0xFFFF_FFFF;
        let (dirty, dirty_gpu) = h.shared(&words);
        let (clean, clean_gpu) = h.shared(&vec![one; 18]);
        let items = [
            Item {
                gpu: dirty_gpu,
                dwords: 18,
                base: 0,
                slot: 1,
                layout: layout(36),
            },
            Item {
                gpu: clean_gpu,
                dwords: 18,
                base: 0,
                slot: 2,
                layout: layout(36),
            },
        ];
        // SAFETY: a live counters buffer and open list.
        let counters_gpu = unsafe { counters.GetGPUVirtualAddress() };
        record(&h.list, &scanner, counters_gpu, &items);
        let readback = {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_READBACK,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: BYTES,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut b: Option<ID3D12Resource> = None;
            // SAFETY: plain creation of a read-back buffer.
            unsafe {
                h.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        None,
                        &mut b,
                    )
                    .unwrap();
            }
            b.unwrap()
        };
        // SAFETY: live resources on an open list.
        unsafe {
            h.list.ResourceBarrier(&[crate::host::transition(
                &counters,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            h.list.CopyBufferRegion(&readback, 0, &counters, 0, BYTES);
        }
        h.wait(1);
        let mut data = vec![0u32; SLOTS * WIDTH];
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy finished; the mapping spans the buffer.
        unsafe {
            readback.Map(0, None, Some(&mut mapped)).unwrap();
            std::ptr::copy_nonoverlapping(mapped.cast::<u32>(), data.as_mut_ptr(), data.len());
            readback.Unmap(0, None);
        }
        let _ = (&dirty, &clean, &h.allocator);
        let r = row(&data, 1);
        assert_eq!((r.n32, r.scanned), (2, 18), "{r:?}");
        assert_eq!((r.by_field32[2], r.by_field32[3]), (1, 1), "{r:?}");
        assert_eq!(r.first32, Some(11), "{r:?}");
        assert_eq!(
            (r.n16, r.by_field16[6], r.by_field16[7]),
            (2, 1, 1),
            "{r:?}"
        );
        assert_eq!(r.first16, Some(15), "{r:?}");
        assert_eq!(
            r.by_field32.iter().sum::<u32>(),
            2,
            "the integer dword is no float field: {r:?}"
        );
        assert!(r.order32 > 0 && r.order16 > 0);
        let clean = row(&data, 2);
        assert_eq!(
            (clean.n32, clean.n16, clean.scanned),
            (0, 0, 18),
            "{clean:?}"
        );
        // the header counts how many slots hit first: two counters of one slot, so 2 here
        assert_eq!(data[0], 2);
    }
}
