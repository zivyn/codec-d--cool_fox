//! A stand-in for the indirect light the title traces rays for.
//!
//! With the ray-tracing structure builder off (`crate::pipeline::builder_runs`)
//! the title's rays find nothing. Its program that resolves the diffuse light
//! of its screen probes (`CS_ScreenProbes_TemporalResolve_*`) then leaves the
//! sky's light outdoors, nothing indoors (walls are black where no lamp or
//! sun reaches) and, in blocks of the picture's tiles, what the probes'
//! memory held. Right after that program the light is raised to an estimate
//! and bounded by a multiple of it.
//!
//! That a stand-in can be put there, and in which of the program's images,
//! is beloooo's find (VIctory-Belobug-AMD, 06031237, 4f45ab88, cd9f78ce,
//! GPL-3.0-or-later), which puts a constant there, shaded in creases from the
//! scene's depth. The estimate here is worked out from the frame instead, so
//! it follows the hour, the room and its lamps by itself: the light the
//! visible surfaces send out (the title keeps it for its own screen traces:
//! `CS_CacheScreenSpaceRadiance`, the frame before) is averaged in 64x36
//! cells, and every point gets the mean of the cells weighted by the share of
//! its surroundings they cover, as a directional value (first-order harmonics
//! at 160x90 points, taken between them by depth).
//!
//! What it assumes: what is not on screen sends out what is (with a slow
//! mean of the last seconds, so that turning into a dark corner is not black
//! at once). No occlusion but the creases'.
//!
//! `VICTORY_AMBIENT`:
//! - unset or `0`/`off`: the title's own output.
//! - a number: that share of the estimate (1: all of it).
//! - `<share>,floor:<level>`: the estimate, and nowhere less than the
//!   constant `<level>` (the estimate follows the light of the visible
//!   surfaces, so a room only a sun patch lights has a mean near nothing and
//!   the stand-in leaves it dark; the floor holds it up).
//! - `constant:<level>`: the constant, as beloooo's fork has it.
//!
//! The title has several programs that resolve the diffuse light (the
//! variants of `CS_ScreenProbes_TemporalResolve`); they keep the image and
//! the depth at other places in their tables, but at the same shader
//! registers, so the places are taken from each program's own resource
//! list ([`places`]). A program the stand-in cannot follow says why, once,
//! and every few hundred frames a line counts what each did ([`report`]).
//!
//! `VICTORY_AMBIENT_SHADE=<strength>,<radius in metres>` sets the creases.
//!
//! The air of a room is lit by the same probes: without rays they hold the
//! sky's light indoors too, a brown veil at sunset. `VICTORY_AMBIENT_AIR`
//! bounds what the air scatters by a multiple of the light around, and with `,max:<level>`
//! by a constant too (the multiple alone grows with the room).

use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::s;

use crate::host::{HostList, host};

/// Programs of the title the stand-in follows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Role {
    /// Resolves the diffuse light.
    Resolve,
    /// Keeps the light visible surfaces send out.
    Radiance,
    /// Adds the lamps' light to what the air scatters (the last program
    /// that writes that volume before it is summed up along the view).
    Air,
}

/// The program's part in this, by its name in the container.
pub(crate) fn role(container: &[u8]) -> Option<Role> {
    let has = |name: &[u8]| container.windows(name.len()).any(|w| w == name);
    if has(b"CS_ScreenProbes_TemporalResolve") {
        Some(Role::Resolve)
    } else if has(b"CS_CacheScreenSpaceRadiance") {
        Some(Role::Radiance)
    } else if has(b"CS_AccumulateLocalLightInscatteringVolume") {
        Some(Role::Air)
    } else {
        None
    }
}

/// Root parameter and place in its table, in dwords.
pub(crate) type Place = (u8, u32);

/// Where a program has the images the stand-in follows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Places {
    /// The image the program writes (the diffuse light, R9G9B9E5 as 32-bit
    /// integers; the light visible surfaces send out; the air's light).
    pub(crate) image: Place,
    /// The scene's depth (the resolve reads it; else the image again).
    pub(crate) depth: Place,
}

/// Shader registers (in every variant of a program, at whatever place its
/// table gives them): of the image written and, for the resolve, of the depth.
const LIGHT_REGISTER: u32 = 36;
const DEPTH_REGISTER: u32 = 213;
const RADIANCE_REGISTER: u32 = 45;
const AIR_REGISTER: u32 = 44;

/// Where the program with these used resources has what the stand-in needs,
/// or why it has not.
pub(crate) fn places(role: Role, used: &[victory_shader::UsedResource]) -> Result<Places, &'static str> {
    use victory_shader::{ResourceType, RootType};
    let find = |kind, register, none, twice| -> Result<Place, &'static str> {
        let mut found = used.iter().filter(|u| u.kind == kind && u.lower_bound == register);
        let first = found.next().ok_or(none)?;
        if found.next().is_some() {
            return Err(twice);
        }
        if first.root != RootType::Table {
            return Err("its image is not in a descriptor table");
        }
        Ok((first.slot, first.table_offset_dwords))
    };
    let register = match role {
        Role::Resolve => LIGHT_REGISTER,
        Role::Radiance => RADIANCE_REGISTER,
        Role::Air => AIR_REGISTER,
    };
    let image = find(ResourceType::Uav, register, "no image at the register it writes", "two images at the register it writes")?;
    let depth = match role {
        Role::Resolve => find(ResourceType::Srv, DEPTH_REGISTER, "no depth at the register it reads", "two depths at the register it reads")?,
        Role::Radiance | Role::Air => image,
    };
    Ok(Places { image, depth })
}

/// The name of the program, as the container has it: from the part of the
/// name that gave it its role to the end of the word.
pub(crate) fn variant_name(container: &[u8]) -> String {
    const STARTS: [&[u8]; 3] = [b"CS_ScreenProbes_TemporalResolve", b"CS_CacheScreenSpaceRadiance", b"CS_AccumulateLocalLightInscatteringVolume"];
    for start in STARTS {
        if let Some(at) = container.windows(start.len()).position(|w| w == start) {
            let word = container[at..].iter().take_while(|b| b.is_ascii_alphanumeric() || **b == b'_').count();
            return String::from_utf8_lossy(&container[at..at + word]).into_owned();
        }
    }
    String::new()
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Set {
    Off,
    /// The share of the estimate, and the constant it does not go below.
    Estimate(f32, f32),
    Constant(f32),
}

fn parse(set: &str) -> Set {
    let number = |t: &str| t.trim().parse::<f32>().ok().filter(|v| v.is_finite() && *v > 0.0);
    let mut parts = set.split(',');
    let head = parts.next().unwrap_or_default().trim();
    match head.strip_prefix("constant:") {
        Some(level) => number(level).map_or(Set::Off, Set::Constant),
        None => number(head).map_or(Set::Off, |share| {
            let floor = parts.find_map(|p| p.trim().strip_prefix("floor:").and_then(number));
            Set::Estimate(share, floor.unwrap_or(0.0))
        }),
    }
}

fn set() -> Set {
    static SET: OnceLock<Set> = OnceLock::new();
    *SET.get_or_init(|| parse(&std::env::var("VICTORY_AMBIENT").unwrap_or_default()))
}

/// Whether the stand-in is asked for (it still rests where the title's rays
/// find something: [`BUILDER_RUNS`]).
pub(crate) fn wanted() -> bool {
    set() != Set::Off
}

/// Why the stand-in does nothing while the structure builder runs.
pub(crate) const BUILDER_RUNS: &str = "the structure builder runs (the title's rays find something)";

/// What `VICTORY_AMBIENT_AIR=<multiple>[,max:<level>]` asks for: the most the air scatters,
/// as a multiple of the light around, and nowhere more than the constant `<level>` (the
/// multiple alone grows with the room: a bright room's air may then scatter a lot; the level
/// is in the light image's unit, as `floor:` is). Unset or 0: what the title has.
fn air_setting(set: &str) -> (f32, f32) {
    let number = |t: &str| t.trim().parse::<f32>().ok().filter(|v| v.is_finite() && *v > 0.0);
    let mut parts = set.split(',');
    let multiple = parts.next().and_then(number).unwrap_or(0.0);
    let most = parts.find_map(|p| p.trim().strip_prefix("max:").and_then(number)).unwrap_or(f32::MAX);
    (multiple, most)
}

fn air_set() -> (f32, f32) {
    static SET: OnceLock<(f32, f32)> = OnceLock::new();
    *SET.get_or_init(|| air_setting(&std::env::var("VICTORY_AMBIENT_AIR").unwrap_or_default()))
}

fn air() -> f32 {
    air_set().0
}

/// Strength and radius of the creases' shade.
fn shade() -> [f32; 2] {
    static SET: OnceLock<[f32; 2]> = OnceLock::new();
    *SET.get_or_init(|| {
        let mut shade = [1.0, 0.6];
        let set = std::env::var("VICTORY_AMBIENT_SHADE").unwrap_or_default();
        for (to, v) in shade.iter_mut().zip(set.split(',')) {
            if let Ok(v) = v.trim().parse() {
                *to = v;
            }
        }
        shade
    })
}

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=13, b0), DescriptorTable(SRV(t0)), DescriptorTable(UAV(u0)), UAV(u1), UAV(u2)"
#define CX 64
#define CY 36
#define CELLS (CX * CY)
#define PX 160
#define PY 90
cbuffer Set : register(b0) {
    float level; float constant; float strength; float radius;
    float near; float tan_y; uint width; uint height;
    float follow; float most; float air; float base; float air_max;
};
Texture2D<float> depth : register(t0);
// Cells: the light they send out (then, at CELLS, the slow mean), from
// CELLS + 1 on where they are.
RWStructuredBuffer<float4> cells : register(u1);
// Three a point: red, green and blue, each a mean and a direction.
RWStructuredBuffer<float4> points : register(u2);
static const float3 LUMA = float3(0.2126, 0.7152, 0.0722);

// The point seen at a texel, in metres from the eye (x right, y up, z away).
float3 seen(int2 p, float2 size) {
    float d = depth.Load(int3(clamp(p, int2(0, 0), int2(size) - 1), 0));
    float z = min(near / max(d, 1e-7), 1000.0);
    float2 ndc = (float2(p) + 0.5) / size * 2.0 - 1.0;
    return float3(ndc.x * tan_y * size.x / size.y * z, -ndc.y * tan_y * z, z);
}

#ifdef KEEP
RWTexture2D<float3> radiance : register(u0);
[RootSignature(RS)]
[numthreads(8, 4, 1)]
void keep(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    radiance.GetDimensions(w, h);
    float3 sum = 0;
    for (uint y = 0; y < 5; y++) for (uint x = 0; x < 5; x++) {
        uint2 p = uint2((float2(at.xy) + (float2(x, y) + 0.5) / 5.0) * float2(w, h) / float2(CX, CY));
        float3 c = max(radiance[p], 0.0);
        c = any(isnan(c)) || any(isinf(c)) ? 0.0 : c;
        // Lamps and glints count, but not for everything.
        sum += c / max(1.0, dot(c, LUMA) / 20.0);
    }
    cells[at.y * CX + at.x] = float4(sum / 25.0, 1.0);
}
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void mean() {
    float3 sum = 0;
    for (uint i = 0; i < CELLS; i++) sum += cells[i].rgb;
    float4 was = cells[CELLS];
    float3 now = sum / CELLS;
    cells[CELLS] = float4(was.a > 0.0 ? lerp(was.rgb, now, follow) : now, 1.0);
}
#endif

#ifdef PLACE
[RootSignature(RS)]
[numthreads(8, 4, 1)]
void place(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    float3 p = seen(int2((float2(at.xy) + 0.5) * size / float2(CX, CY)), size);
    cells[CELLS + 1 + at.y * CX + at.x] = float4(p, p.z * p.z);
}
#endif

#ifdef GATHER
[RootSignature(RS)]
[numthreads(8, 2, 1)]
void gather(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    float3 p = seen(int2((float2(at.xy) + 0.5) * size / float2(PX, PY)), size);
    float r2 = 0.25 + 0.0025 * p.z * p.z;
    float whole = 0;
    float3 toward = 0, light = 0, red = 0, green = 0, blue = 0;
    for (uint i = 0; i < CELLS; i++) {
        float4 c = cells[CELLS + 1 + i];
        float3 L = cells[i].rgb;
        float3 d = c.xyz - p;
        float dd = dot(d, d);
        float w = c.w / (dd + r2);
        float3 dir = d / sqrt(dd + 0.25 * r2);
        whole += w;
        toward += w * dir;
        light += w * L;
        red += w * L.r * dir;
        green += w * L.g * dir;
        blue += w * L.b * dir;
    }
    whole = max(whole, 1e-6);
    light /= whole;
    toward /= whole;
    uint at0 = (at.y * PX + at.x) * 3;
    points[at0] = float4(red / whole - light.r * toward, light.r);
    points[at0 + 1] = float4(green / whole - light.g * toward, light.g);
    points[at0 + 2] = float4(blue / whole - light.b * toward, light.b);
}
#endif

#ifdef AIR
RWTexture3D<float3> scattered : register(u0);
// No more than a multiple of the light around: of the room as the last
// seconds showed it, and of the surface seen in this direction (towards a
// window the air outside may be as bright as the sky is).
[RootSignature(RS)]
[numthreads(8, 8, 2)]
void clear_air(uint3 at : SV_DispatchThreadID) {
    uint w, h, d;
    scattered.GetDimensions(w, h, d);
    if (at.x >= w || at.y >= h || at.z >= d) return;
    uint2 q = min(uint2((float2(at.xy) + 0.5) / float2(w, h) * float2(PX, PY)), uint2(PX - 1, PY - 1));
    uint at0 = (q.y * PX + q.x) * 3;
    float3 seen_there = float3(points[at0].w, points[at0 + 1].w, points[at0 + 2].w);
    float far = float(at.z) / float(d);
    float cap = min(air * lerp(dot(cells[CELLS].rgb, LUMA), dot(seen_there, LUMA), far), air_max);
    float3 c = scattered[at];
    float has = dot(c, LUMA);
    if (has > cap) scattered[at] = c * (cap / has);
}
#endif

#ifdef RAISE
RWTexture2D<uint> light : register(u0);
float3 unpack(uint v) {
    return float3(v & 511, (v >> 9) & 511, (v >> 18) & 511) * exp2(float(v >> 27) - 24.0);
}
uint pack(float3 c) {
    c = clamp(c, 0.0, 65408.0);
    float top = max(c.r, max(c.g, c.b));
    int e = clamp(int(floor(log2(max(top, 1e-9)))) + 16, 0, 31);
    float scale = exp2(float(24 - e));
    if (top * scale >= 511.5) { e = min(e + 1, 31); scale = exp2(float(24 - e)); }
    uint3 m = min(uint3(c * scale + 0.5), 511u);
    return m.r | (m.g << 9) | (m.b << 18) | (uint(e) << 27);
}
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void raise(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    if (at.x >= width || at.y >= height) return;
    int2 p = int2(at.xy);
    float3 c = seen(p, size);
    float3 l = seen(p + int2(-1, 0), size), r = seen(p + int2(1, 0), size);
    float3 u = seen(p + int2(0, -1), size), d = seen(p + int2(0, 1), size);
    float3 dx = abs(l.z - c.z) < abs(r.z - c.z) ? c - l : r - c;
    float3 dy = abs(u.z - c.z) < abs(d.z - c.z) ? c - u : d - c;
    float3 n = normalize(cross(dy, dx) + float3(0, 0, -1e-9));
    if (dot(n, c) > 0.0) n = -n;
    // How open it is (as in beloooo's cd9f78ce).
    float open = 1.0;
    float px = min(radius * size.y / (2.0 * tan_y * c.z), 128.0);
    float noise = frac(52.9829189 * frac(dot(float2(p), float2(0.06711056, 0.00583715))));
    if (strength > 0.0 && px >= 1.5 && c.z < 400.0) {
        float sum = 0.0;
        const int N = 12;
        for (int i = 0; i < N; i++) {
            float a = i * 2.399963 + noise * 6.2831853;
            float2 o = float2(cos(a), sin(a)) * (sqrt((i + 0.5) / N) * px);
            float3 v = seen(p + int2(round(o)), size) - c;
            float vv = dot(v, v);
            float over = dot(v, n) - 0.004 * c.z - 0.005;
            sum += max(over, 0.0) / sqrt(vv + 1e-6) * saturate(1.0 - vv / (radius * radius));
        }
        open = saturate(1.0 - strength * 2.0 * sum / N);
    }
    float3 found;
    if (constant > 0.0) {
        found = float3(1.0, 0.98, 0.94) * constant * open;
        light[p] = pack(found);
        return;
    }
    // Between the four points around, those at its own depth first; which
    // four is dithered a little.
    float2 g = (float2(p) + 0.5) / size * float2(PX, PY) - 0.5 + (noise - 0.5) * 0.5;
    int2 g0 = int2(floor(g));
    float2 f = g - float2(g0);
    float4 sr = 0, sg = 0, sb = 0;
    float sum = 0;
    for (int k = 0; k < 4; k++) {
        int2 q = clamp(g0 + int2(k & 1, k >> 1), int2(0, 0), int2(PX, PY) - 1);
        float z = seen(int2((float2(q) + 0.5) * size / float2(PX, PY)), size).z;
        float2 b = float2(k & 1 ? f.x : 1 - f.x, k >> 1 ? f.y : 1 - f.y);
        float w = (b.x * b.y + 1e-3) / (0.02 + abs(z - c.z) / c.z);
        uint at0 = (q.y * PX + q.x) * 3;
        sr += w * points[at0];
        sg += w * points[at0 + 1];
        sb += w * points[at0 + 2];
        sum += w;
    }
    float3 all = float3(sr.w, sg.w, sb.w) / sum;
    found = all + 1.2 * float3(dot(sr.xyz, n), dot(sg.xyz, n), dot(sb.xyz, n)) / sum;
    found = clamp(found, 0.2 * all, 2.5 * all);
    found = lerp(found, cells[CELLS].rgb, 0.2) * level * open;
    found = max(found, float3(1.0, 0.98, 0.94) * base * open);
    float3 was = unpack(light[p]);
    // What the program left counts as far as it can be true: the sky's light
    // outdoors, not the blocks of other memory.
    float top = max(dot(found, LUMA) * most, 1e-6);
    float has = dot(was, LUMA);
    was *= min(1.0, top / max(has, 1e-6));
    light[p] = pack(max(was, found));
}
#endif
"#;

/// Bytes of the cells' buffer and of the points'.
const CELLS: u64 = 64 * 36;
const CELL_BYTES: u64 = (2 * CELLS + 1) * 16;
const POINT_BYTES: u64 = 160 * 90 * 48;

struct Pass {
    root: ID3D12RootSignature,
    keep: ID3D12PipelineState,
    mean: ID3D12PipelineState,
    place: ID3D12PipelineState,
    gather: ID3D12PipelineState,
    raise: ID3D12PipelineState,
    air: ID3D12PipelineState,
    cells: ID3D12Resource,
    points: ID3D12Resource,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pass {}
// SAFETY: as above.
unsafe impl Sync for Pass {}

fn make(device: &ID3D12Device, buffer: impl Fn(u64) -> Option<ID3D12Resource>) -> windows::core::Result<Pass> {
    let mut root: Option<ID3D12RootSignature> = None;
    let mut pipeline = |part: &str, entry| -> windows::core::Result<ID3D12PipelineState> {
        let code = crate::host::compile(&format!("#define {part}\n{SOURCE}"), entry, s!("cs_5_1"))?;
        // SAFETY: the root signature is inside the shader; plain creations.
        unsafe {
            if root.is_none() {
                root = Some(device.CreateRootSignature(0, &code)?);
            }
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
                ..Default::default()
            };
            device.CreateComputePipelineState(&desc)
        }
    };
    let keep = pipeline("KEEP", s!("keep"))?;
    let mean = pipeline("KEEP", s!("mean"))?;
    let place = pipeline("PLACE", s!("place"))?;
    let gather = pipeline("GATHER", s!("gather"))?;
    let raise = pipeline("RAISE", s!("raise"))?;
    let air = pipeline("AIR", s!("clear_air"))?;
    let made = |bytes| buffer(bytes).ok_or_else(windows::core::Error::empty);
    Ok(Pass {
        root: root.ok_or_else(windows::core::Error::empty)?,
        keep,
        mean,
        place,
        gather,
        raise,
        air,
        cells: made(CELL_BYTES)?,
        points: made(POINT_BYTES)?,
    })
}

fn pass() -> Option<&'static Pass> {
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let host = host()?;
        make(&host.device, |bytes| host.uav_buffer(bytes))
            .inspect_err(|e| tracing::warn!(error = %e, "no stand-in for the indirect light"))
            .ok()
    })
    .as_ref()
}

fn uav_barrier(list: &ID3D12GraphicsCommandList) {
    let barrier = D3D12_RESOURCE_BARRIER { Type: D3D12_RESOURCE_BARRIER_TYPE_UAV, ..Default::default() };
    // SAFETY: an open list; the barrier names no resource.
    unsafe { list.ResourceBarrier(&[barrier]) };
}

/// Whether the light of a frame has been kept: the estimate has something
/// to begin with.
static KEPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the passes read and write, as views in the heap the list has set.
pub(crate) struct Views {
    /// The image the title's program just wrote.
    pub(crate) image: D3D12_GPU_DESCRIPTOR_HANDLE,
    /// The scene's depth, for [`Role::Resolve`].
    pub(crate) depth: D3D12_GPU_DESCRIPTOR_HANDLE,
    /// The size of the image, and how many slices it has.
    pub(crate) size: [u32; 2],
    pub(crate) slices: u32,
}

/// After the title's program of that role, in `list` with the program's view
/// heap set. Replaces the list's compute root signature and pipeline; the
/// size of the image it raised, or why it recorded nothing.
pub(crate) fn after(list: &HostList, role: Role, views: &Views) -> Result<[u32; 2], &'static str> {
    use std::sync::atomic::Ordering;
    let Some(pass) = pass() else {
        return Err("the passes did not build");
    };
    let (level, constant, base) = match set() {
        Set::Off => return Err("not asked for"),
        Set::Estimate(level, floor) => (level, 0.0f32, floor),
        Set::Constant(level) => (0.0, level, 0.0),
    };
    if role != Role::Radiance && constant == 0.0 && !KEPT.load(Ordering::Relaxed) {
        return Err("the light of the frame before is not kept yet (no program that keeps it ran)");
    }
    if role == Role::Air && (constant > 0.0 || air() == 0.0) {
        return Err("the air is not bounded (VICTORY_AMBIENT_AIR unset, or a constant)");
    }
    list.keep(&pass.cells);
    list.keep(&pass.points);
    let [strength, radius] = shade();
    let floats = [level, constant, strength, radius, 0.1, 0.45];
    let mut words = [0u32; 13];
    for (to, v) in words.iter_mut().zip(floats) {
        *to = v.to_bits();
    }
    words[6] = views.size[0];
    words[7] = views.size[1];
    words[8] = 0.04f32.to_bits();
    words[9] = 4.0f32.to_bits();
    words[10] = air().to_bits();
    words[11] = base.to_bits();
    words[12] = air_set().1.to_bits();
    let l = &list.list;
    // SAFETY: an open list with the heap of the views set; the title's
    // program has just used both views, so their textures are in the states
    // these views need; buffers the list keeps.
    unsafe {
        uav_barrier(l);
        l.SetComputeRootSignature(&pass.root);
        l.SetComputeRoot32BitConstants(0, 13, words.as_ptr().cast(), 0);
        l.SetComputeRootDescriptorTable(1, views.depth);
        l.SetComputeRootDescriptorTable(2, views.image);
        l.SetComputeRootUnorderedAccessView(3, pass.cells.GetGPUVirtualAddress());
        l.SetComputeRootUnorderedAccessView(4, pass.points.GetGPUVirtualAddress());
        let run = |pso: &ID3D12PipelineState, x, y, z| {
            l.SetPipelineState(pso);
            l.Dispatch(x, y, z);
            uav_barrier(l);
        };
        match role {
            Role::Radiance => {
                run(&pass.keep, 8, 9, 1);
                run(&pass.mean, 1, 1, 1);
                KEPT.store(true, Ordering::Relaxed);
            }
            Role::Air => run(&pass.air, views.size[0].div_ceil(8), views.size[1].div_ceil(8), views.slices.div_ceil(2)),
            Role::Resolve => {
                if constant == 0.0 {
                    run(&pass.place, 8, 9, 1);
                    run(&pass.gather, 20, 45, 1);
                }
                run(&pass.raise, views.size[0].div_ceil(8), views.size[1].div_ceil(8), 1);
            }
        }
    }
    Ok(views.size)
}

/// What the stand-in did, per program (variant) of the title.
#[derive(Default)]
struct Coverage {
    variants: Vec<Variant>,
    /// The frame of the last report.
    since: u64,
}

const APPLIED: &str = "applied";

struct Variant {
    hash: String,
    name: String,
    role: Role,
    /// Since the last report: dispatches raised, and the ones not, by why.
    applied: u64,
    skipped: Vec<(&'static str, u64)>,
    /// Over the run.
    applied_total: u64,
    skipped_total: u64,
    /// The outcomes already told (once each, over the run).
    told: Vec<&'static str>,
}

impl Coverage {
    /// Counts one dispatch; whether its outcome is new for the program (and
    /// so is told).
    fn note(&mut self, hash: &str, name: impl FnOnce() -> String, role: Role, outcome: Result<(), &'static str>) -> bool {
        let at = match self.variants.iter().position(|v| v.hash == hash) {
            Some(at) => at,
            None => {
                self.variants.push(Variant {
                    hash: hash.to_owned(),
                    name: name(),
                    role,
                    applied: 0,
                    skipped: Vec::new(),
                    applied_total: 0,
                    skipped_total: 0,
                    told: Vec::new(),
                });
                self.variants.len() - 1
            }
        };
        let v = &mut self.variants[at];
        let key = match outcome {
            Ok(()) => {
                v.applied += 1;
                v.applied_total += 1;
                APPLIED
            }
            Err(why) => {
                v.skipped_total += 1;
                match v.skipped.iter_mut().find(|s| s.0 == why) {
                    Some(s) => s.1 += 1,
                    None => v.skipped.push((why, 1)),
                }
                why
            }
        };
        let new = !v.told.contains(&key);
        if new {
            v.told.push(key);
        }
        new
    }

    /// The report lines for the frames since the last report (the first is
    /// the sum), and the counts start again.
    fn report(&mut self, frame: u64) -> Vec<String> {
        use std::fmt::Write;
        let frames = frame - self.since;
        self.since = frame;
        let mut lines = Vec::new();
        let (mut applied, mut skipped) = (0, 0);
        for v in &mut self.variants {
            let n: u64 = v.skipped.iter().map(|s| s.1).sum();
            applied += v.applied;
            skipped += n;
            let mut why = String::new();
            for (reason, count) in v.skipped.iter().filter(|s| s.1 > 0) {
                let _ = write!(why, " skipped {count}: {reason};");
            }
            lines.push(format!(
                "{:?} {} ({}): applied {} of {} dispatches in {frames} frames, {} of {} so far;{why}",
                v.role,
                v.name,
                v.hash,
                v.applied,
                v.applied + n,
                v.applied_total,
                v.applied_total + v.skipped_total,
            ));
            v.applied = 0;
            v.skipped.clear();
        }
        lines.insert(0, format!("{} programs seen; {applied} dispatches raised, {skipped} not, in {frames} frames", self.variants.len()));
        lines
    }
}

static COVERAGE: Mutex<Option<Coverage>> = Mutex::new(None);

/// After a dispatch of a program the stand-in follows: what it did, counted
/// per program and told once for each new outcome of one.
pub(crate) fn note(hash: &str, container: &[u8], role: Role, outcome: Result<[u32; 2], &'static str>) {
    let size = outcome.as_ref().ok().copied();
    let outcome = outcome.map(|_| ());
    let name = || variant_name(container);
    let new = COVERAGE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Coverage::default)
        .note(hash, name, role, outcome);
    if !new {
        return;
    }
    let program = name();
    match outcome {
        Ok(()) => tracing::info!(program, hash, ?role, ?size, set = ?set(), "stand-in for the indirect light: applied"),
        Err(why) => tracing::warn!(program, hash, ?role, why, "stand-in for the indirect light: skipped"),
    }
}

/// At the present of `frame` (every 256th): the coverage lines, the sum and
/// one per program.
pub(crate) fn report(frame: u64) {
    let lines = match COVERAGE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        Some(coverage) => coverage.report(frame),
        None => return,
    };
    for line in lines {
        tracing::info!(frame, "ambient coverage: {line}");
    }
}

#[cfg(test)]
mod tests {
    use victory_shader::{ResourceType, RootType, UsedResource};

    use super::*;

    #[test]
    fn the_air_setting_has_a_multiple_and_a_most() {
        assert_eq!(air_setting(""), (0.0, f32::MAX));
        assert_eq!(air_setting("1.5"), (1.5, f32::MAX));
        assert_eq!(air_setting("1.5,max:4"), (1.5, 4.0));
        assert_eq!(air_setting(" 0.5 , max: 0.25 "), (0.5, 0.25));
        assert_eq!(air_setting("off,max:3"), (0.0, 3.0));
        assert_eq!(air_setting("1,max:-2"), (1.0, f32::MAX));
    }

    #[test]
    fn settings() {
        assert_eq!(parse(""), Set::Off);
        assert_eq!(parse("off"), Set::Off);
        assert_eq!(parse("0"), Set::Off);
        assert_eq!(parse("0.8"), Set::Estimate(0.8, 0.0));
        assert_eq!(parse(" 2.0 "), Set::Estimate(2.0, 0.0));
        assert_eq!(parse("constant:0.3"), Set::Constant(0.3));
        assert_eq!(parse("constant:0.4,floor:0.1"), Set::Constant(0.4));
        assert_eq!(parse("constant:nan"), Set::Off);
        assert_eq!(parse("constant:"), Set::Off);
        assert_eq!(parse("floor:0.1"), Set::Off);
    }

    #[test]
    fn settings_with_a_floor() {
        assert_eq!(parse("0.8,floor:0.1"), Set::Estimate(0.8, 0.1));
        assert_eq!(parse("0.8, floor: 0.25"), Set::Estimate(0.8, 0.25));
        // A floor that is not a positive number is no floor.
        assert_eq!(parse("0.8,floor:-1"), Set::Estimate(0.8, 0.0));
        assert_eq!(parse("0.8,floor:x"), Set::Estimate(0.8, 0.0));
        assert_eq!(parse("0.8,other"), Set::Estimate(0.8, 0.0));
        assert_eq!(parse("0,floor:0.1"), Set::Off);
    }

    fn used(kind: ResourceType, root: RootType, slot: u8, offset: u32, register: u32) -> UsedResource {
        UsedResource { kind, root, table_offset_dwords: offset, lower_bound: register, stride: 4, dwords: 8, user_data: slot, slot }
    }

    /// A program's resources, the way the title's variants differ: the same
    /// registers, at the places their tables give them.
    fn variant(image_at: u32, depth_at: u32, extra: &[UsedResource]) -> Vec<UsedResource> {
        let mut list = vec![
            used(ResourceType::Srv, RootType::Table, 0, 8, 7),
            used(ResourceType::Srv, RootType::Table, 0, depth_at, DEPTH_REGISTER),
            used(ResourceType::Uav, RootType::Table, 1, 128, LIGHT_REGISTER + 1),
            used(ResourceType::Uav, RootType::Table, 1, image_at, LIGHT_REGISTER),
            // Another kind at the same register number is no match.
            used(ResourceType::Cbv, RootType::Table, 1, 300, LIGHT_REGISTER),
            used(ResourceType::Srv, RootType::Table, 1, 301, LIGHT_REGISTER),
        ];
        list.extend_from_slice(extra);
        list
    }

    #[test]
    fn places_follow_the_registers_of_each_variant() {
        let one = places(Role::Resolve, &variant(176, 464, &[])).unwrap();
        assert_eq!(one, Places { image: (1, 176), depth: (0, 464) });
        // The same registers, at other places in the table.
        let other = places(Role::Resolve, &variant(184, 480, &[])).unwrap();
        assert_eq!(other, Places { image: (1, 184), depth: (0, 480) });
    }

    #[test]
    fn places_of_the_other_roles_are_their_image_alone() {
        let list = [
            used(ResourceType::Uav, RootType::Table, 1, 16, RADIANCE_REGISTER),
            used(ResourceType::Uav, RootType::Table, 1, 32, AIR_REGISTER),
        ];
        assert_eq!(places(Role::Radiance, &list).unwrap(), Places { image: (1, 16), depth: (1, 16) });
        assert_eq!(places(Role::Air, &list).unwrap(), Places { image: (1, 32), depth: (1, 32) });
    }

    #[test]
    fn places_say_what_is_missing() {
        let no_image: Vec<_> = variant(176, 464, &[]).into_iter().filter(|u| !(u.kind == ResourceType::Uav && u.lower_bound == LIGHT_REGISTER)).collect();
        assert!(places(Role::Resolve, &no_image).unwrap_err().contains("no image"));
        let no_depth: Vec<_> = variant(176, 464, &[]).into_iter().filter(|u| u.lower_bound != DEPTH_REGISTER).collect();
        assert!(places(Role::Resolve, &no_depth).unwrap_err().contains("no depth"));
        // The depth is not needed by the roles that have no depth.
        assert!(places(Role::Radiance, &no_depth).unwrap_err().contains("no image"));
        let twice = variant(176, 464, &[used(ResourceType::Uav, RootType::Table, 1, 192, LIGHT_REGISTER)]);
        assert!(places(Role::Resolve, &twice).unwrap_err().contains("two images"));
        let root_view = [used(ResourceType::Uav, RootType::Descriptor, 3, 0, RADIANCE_REGISTER)];
        assert!(places(Role::Radiance, &root_view).unwrap_err().contains("table"));
    }

    #[test]
    fn names_of_programs() {
        assert_eq!(variant_name(b"\0\0CS_ScreenProbes_TemporalResolve_CB_a1c765aa_Wrapped\0rest"), "CS_ScreenProbes_TemporalResolve_CB_a1c765aa_Wrapped");
        assert_eq!(variant_name(b"x CS_CacheScreenSpaceRadiance_ab12 y"), "CS_CacheScreenSpaceRadiance_ab12");
        assert_eq!(variant_name(b"nothing"), "");
        assert_eq!(variant_name(b"CS_ScreenProbes_TemporalResolve"), "CS_ScreenProbes_TemporalResolve");
    }

    #[test]
    fn coverage_counts_per_program_and_tells_each_outcome_once() {
        let mut c = Coverage::default();
        let name = || String::from("program");
        // The first outcome of a program is new, the same again is not.
        assert!(c.note("aa", name, Role::Resolve, Err("not kept yet")));
        assert!(!c.note("aa", name, Role::Resolve, Err("not kept yet")));
        assert!(c.note("aa", name, Role::Resolve, Ok(())));
        assert!(!c.note("aa", name, Role::Resolve, Ok(())));
        // Another program has its own.
        assert!(c.note("bb", name, Role::Resolve, Err("no image at the register it writes")));
        assert!(c.note("cc", name, Role::Radiance, Ok(())));
        let lines = c.report(256);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("3 programs seen; 3 dispatches raised, 3 not, in 256 frames"));
        assert!(lines[1].contains("(aa): applied 2 of 4 dispatches in 256 frames, 2 of 4 so far; skipped 2: not kept yet;"));
        assert!(lines[2].contains("(bb): applied 0 of 1") && lines[2].contains("no image at the register it writes"));
        assert!(lines[3].contains("Radiance") && lines[3].contains("applied 1 of 1"));
        // The counts start again; the totals and what was told do not.
        assert!(!c.note("aa", name, Role::Resolve, Ok(())));
        let lines = c.report(512);
        assert!(lines[0].starts_with("3 programs seen; 1 dispatches raised, 0 not, in 256 frames"));
        assert!(lines[1].contains("applied 1 of 1 dispatches in 256 frames, 3 of 5 so far;"));
    }

    #[test]
    fn roles_by_name() {
        assert_eq!(role(b"..CS_ScreenProbes_TemporalResolve_a911b49c_Wrapped.."), Some(Role::Resolve));
        assert_eq!(role(b"..CS_CacheScreenSpaceRadiance_eed2fc35.."), Some(Role::Radiance));
        assert_eq!(role(b"CS_AccumulateLocalLightInscatteringVolume_7e2f5eef"), Some(Role::Air));
        assert_eq!(role(b"CS_TemporalResolve"), None);
    }

    /// The passes compile with the system's compiler and the host takes them.
    #[test]
    fn the_passes_build() {
        let Some(host) = host() else {
            return;
        };
        make(&host.device, |bytes| host.uav_buffer(bytes)).expect("the passes");
    }
}
