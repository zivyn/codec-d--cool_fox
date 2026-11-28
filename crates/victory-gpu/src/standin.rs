//! The title's R11G11B10_FLOAT texels for a host texture that stands in for them in 16-bit float RGBA
//! (`VICTORY_R11G11B10_AS_F16=1`, [`crate::resource::target_format`]).
//!
//! The title fills such a texture from memory (a copy from a buffer footprint) or from another texture of its format
//! that the host did not have to change (one that no pass renders to or writes). The host copies neither: the texels
//! differ in size. A compute pass converts them into a buffer of 16-bit float RGBA rows, and the copy is made from that
//! buffer ([`buffer_region`], [`texture_region`]). The conversion is exact: an 11-bit float (5 exponent bits, 6
//! mantissa bits) and a 10-bit one (5 and 5) are 16-bit floats with the mantissa's low bits zero, infinities and NaNs
//! included, so it is a shift of each channel's bits; alpha is 1.0, which is what a read of R11G11B10 gives.

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::s;

use crate::host::HostList;
use crate::resource::PlacedFootprint;

/// R11G11B10_FLOAT.
const PACKED: u32 = 26;
/// R16G16B16A16_FLOAT.
pub(crate) const HALVES: u32 = 10;

/// The 16-bit float RGBA texel (red, green, blue, alpha as 16-bit floats) of an R11G11B10_FLOAT one: the channels'
/// bits, moved up to the place a 16-bit float has them.
#[cfg(test)]
pub(crate) fn texel(bits: u32) -> [u16; 4] {
    [
        ((bits & 0x7ff) << 4) as u16,
        ((bits >> 11 & 0x7ff) << 4) as u16,
        ((bits >> 22 & 0x3ff) << 5) as u16,
        0x3c00,
    ]
}

/// An unsigned small float (5 exponent bits, `man` mantissa bits) as a number.
#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
pub(crate) fn small(bits: u32, man: u32) -> f32 {
    let e = bits >> man;
    let m = bits & ((1 << man) - 1);
    match e {
        0 => m as f32 / (1 << man) as f32 * 2f32.powi(-14),
        31 if m == 0 => f32::INFINITY,
        31 => f32::NAN,
        _ => (1.0 + m as f32 / (1 << man) as f32) * 2f32.powi(e as i32 - 15),
    }
}

/// A 16-bit float as a number.
#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
pub(crate) fn half(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    sign * small(u32::from(bits & 0x7fff), 10)
}

/// What the probe watch looks at in a voxel of a light volume: whether it holds no light (every channel zero, or below,
/// for a format that has a sign), how bright its brightest channel is, and whether any channel is below zero (R11G11B10
/// has no sign: the stand-in, which has, shows what the format would have clamped).
#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Voxel {
    pub(crate) zero: bool,
    pub(crate) magnitude: f32,
    pub(crate) negative: bool,
}

#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
impl Voxel {
    /// A texel of R11G11B10_FLOAT.
    pub(crate) fn packed(bits: u32) -> Voxel {
        let big = small(bits & 0x7ff, 6).max(small(bits >> 11 & 0x7ff, 6)).max(small(bits >> 22, 5));
        Voxel { zero: bits == 0, magnitude: big, negative: false }
    }

    /// A texel of R16G16B16A16_FLOAT (alpha is not light).
    pub(crate) fn halves(texel: [u16; 4]) -> Voxel {
        let channels = [half(texel[0]), half(texel[1]), half(texel[2])];
        // (What the packed format would hold: no sign. A NaN stays one, as it does there.)
        let clamped = channels.map(|c| if c.is_nan() { c } else { c.max(0.0) });
        Voxel {
            zero: clamped.iter().all(|&c| c == 0.0),
            magnitude: clamped[0].max(clamped[1]).max(clamped[2]),
            negative: channels.iter().any(|&c| c < 0.0),
        }
    }
}

/// The conversion pass: one thread a texel, reading R11G11B10 words of a buffer (a root view, so no heap) and writing
/// 8-byte texels of another.
pub(crate) const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=8, b0), SRV(t0), UAV(u0)"
cbuffer Region : register(b0) {
    uint width;
    uint height;
    uint depth;
    uint src_offset;
    uint src_pitch;
    uint src_slice;
    uint dst_pitch;
    uint dst_slice;
};
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= width || id.y >= height || id.z >= depth) {
        return;
    }
    uint p = src.Load(src_offset + id.z * src_slice + id.y * src_pitch + id.x * 4);
    uint r = (p & 0x7ff) << 4;
    uint g = ((p >> 11) & 0x7ff) << 4;
    uint b = ((p >> 22) & 0x3ff) << 5;
    dst.Store2(id.z * dst_slice + id.y * dst_pitch + id.x * 8, uint2(r | (g << 16), b | (0x3c00 << 16)));
}
"#;

pub(crate) struct Pass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pass {}
// SAFETY: as above.
unsafe impl Sync for Pass {}

/// The pass on `device`.
pub(crate) fn make(device: &ID3D12Device) -> windows::core::Result<Pass> {
    let code = crate::host::compile(SOURCE, s!("main"), s!("cs_5_1"))?;
    // SAFETY: the root signature is inside the shader; plain creations.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &code)?;
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
            ..Default::default()
        };
        let pso = device.CreateComputePipelineState(&desc)?;
        Ok(Pass { root, pso })
    }
}

fn pass() -> Option<&'static Pass> {
    static PASS: std::sync::OnceLock<Option<Pass>> = std::sync::OnceLock::new();
    PASS.get_or_init(|| {
        let host = crate::host::host()?;
        make(&host.device)
            .inspect_err(|e| tracing::warn!(error = %e, "no conversion of R11G11B10 texels for the 16-bit float stand-in"))
            .ok()
    })
    .as_ref()
}

/// What the pass converts: `width` x `height` x `depth` texels of a buffer, from `src_offset`, rows `src_pitch` bytes
/// apart and slices `src_slice`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Region {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) depth: u32,
    pub(crate) src_offset: u32,
    pub(crate) src_pitch: u32,
    pub(crate) src_slice: u32,
}

impl Region {
    /// The bytes between the rows of the output (the host wants multiples of 256).
    pub(crate) fn dst_pitch(&self) -> u32 {
        (self.width * 8).next_multiple_of(256)
    }

    /// The bytes of the output.
    pub(crate) fn dst_bytes(&self) -> u64 {
        u64::from(self.dst_pitch()) * u64::from(self.height) * u64::from(self.depth)
    }

    /// Whether the pass can read it: it loads words.
    fn aligned(&self) -> bool {
        (self.src_offset | self.src_pitch | self.src_slice) & 3 == 0
    }
}

/// Records the pass on `l`: the texels of `region` of the buffer at `src_gpu` (readable by a shader) written to `dst`,
/// a buffer of unordered access in the common state, which is left in the copy source state.
pub(crate) fn record(l: &ID3D12GraphicsCommandList, pass: &Pass, src_gpu: u64, dst: &ID3D12Resource, region: &Region) {
    let pitch = region.dst_pitch();
    let words = [
        region.width,
        region.height,
        region.depth,
        region.src_offset,
        region.src_pitch,
        region.src_slice,
        pitch,
        pitch * region.height,
    ];
    // SAFETY: an open list; buffers the caller keeps until the list has run.
    unsafe {
        l.ResourceBarrier(&[crate::host::transition(dst, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
        l.SetComputeRootSignature(&pass.root);
        l.SetPipelineState(&pass.pso);
        l.SetComputeRoot32BitConstants(0, 8, words.as_ptr().cast(), 0);
        l.SetComputeRootShaderResourceView(1, src_gpu);
        l.SetComputeRootUnorderedAccessView(2, dst.GetGPUVirtualAddress());
        l.Dispatch(region.width.div_ceil(8), region.height.div_ceil(8), region.depth);
        l.ResourceBarrier(&[crate::host::transition(
            dst,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
    }
}

/// Converted texels, in a buffer in the copy source state.
pub(crate) struct Converted {
    pub(crate) buffer: ID3D12Resource,
    pub(crate) footprint: PlacedFootprint,
}

/// [`record`] into a buffer of its own, which the list keeps.
fn convert(list: &HostList, src_gpu: u64, region: Region) -> Option<Converted> {
    if !region.aligned() || region.width == 0 || region.height == 0 || region.depth == 0 {
        return None;
    }
    let (host, pass) = (crate::host::host()?, pass()?);
    let buffer = host.uav_buffer(region.dst_bytes())?;
    record(&list.list, pass, src_gpu, &buffer, &region);
    list.keep(&buffer);
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| tracing::info!("R11G11B10 texels the title copies into a 16-bit float stand-in are converted (VICTORY_R11G11B10_AS_F16)"));
    let footprint = PlacedFootprint::new(0, HALVES, [region.width, region.height, region.depth], region.dst_pitch());
    Some(Converted { buffer, footprint })
}

/// The box of a copy as a size: width, height, depth, and where it starts.
fn extent(b: &D3D12_BOX) -> Option<([u32; 3], [u32; 3])> {
    let size = [b.right.checked_sub(b.left)?, b.bottom.checked_sub(b.top)?, b.back.checked_sub(b.front)?];
    Some((size, [b.left, b.top, b.front]))
}

/// The texels of the title's footprint `at` (of R11G11B10_FLOAT) in `src`, or of `source_box` of it, converted. `src`
/// must be readable by a shader (an upload buffer is) and hold the footprint from its offset.
pub(crate) fn buffer_region(list: &HostList, src: &ID3D12Resource, at: &PlacedFootprint, source_box: Option<&D3D12_BOX>) -> Option<Converted> {
    if at.format() != PACKED {
        return None;
    }
    let [fw, fh, fd] = at.size();
    let pitch = at.row_pitch();
    let slice = u64::from(pitch) * u64::from(fh);
    let (size, [left, top, front]) = match source_box {
        Some(b) => extent(b)?,
        None => ([fw, fh, fd], [0, 0, 0]),
    };
    let offset = at.offset + u64::from(front) * slice + u64::from(top) * u64::from(pitch) + u64::from(left) * 4;
    let region = Region {
        width: size[0],
        height: size[1],
        depth: size[2],
        src_offset: u32::try_from(offset).ok()?,
        src_pitch: pitch,
        src_slice: u32::try_from(slice).ok()?,
    };
    // SAFETY: a live buffer.
    convert(list, unsafe { src.GetGPUVirtualAddress() }, region)
}

/// `subresource` of the host texture (of R11G11B10_FLOAT, in the copy source state), or its `source_box`, converted.
pub(crate) fn texture_region(list: &HostList, texture: &ID3D12Resource, subresource: u32, source_box: Option<&D3D12_BOX>) -> Option<Converted> {
    // SAFETY: a live texture.
    let d = unsafe { texture.GetDesc() };
    if d.Format.0 as u32 != PACKED {
        return None;
    }
    let level = u32::from(d.MipLevels).max(1);
    let mip = subresource % level;
    let shrink = |n: u64| u32::try_from((n >> mip).max(1)).unwrap_or(u32::MAX);
    let size = match source_box {
        Some(b) => extent(b)?.0,
        None => [
            shrink(d.Width),
            shrink(u64::from(d.Height)),
            if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { shrink(u64::from(d.DepthOrArraySize)) } else { 1 },
        ],
    };
    if size.contains(&0) {
        return None;
    }
    let host = crate::host::host()?;
    // The texels as they are, into a buffer a shader reads.
    let pitch = (size[0] * 4).next_multiple_of(256);
    let bytes = u64::from(pitch) * u64::from(size[1]) * u64::from(size[2]);
    let staging = host.uav_buffer(bytes)?;
    let to = D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(&staging) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: 0,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: DXGI_FORMAT(PACKED as i32),
                    Width: size[0],
                    Height: size[1],
                    Depth: size[2],
                    RowPitch: pitch,
                },
            },
        },
    };
    let from = D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: subresource },
    };
    // SAFETY: an open list; the texture is in the copy source state (the caller's), the buffer is ours and held below.
    unsafe {
        list.list.ResourceBarrier(&[crate::host::transition(
            &staging,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )]);
        list.list.CopyTextureRegion(&to, 0, 0, 0, &from, source_box.map(std::ptr::from_ref));
        list.list.ResourceBarrier(&[crate::host::transition(
            &staging,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )]);
    }
    list.keep(&staging);
    // SAFETY: a live buffer.
    let src_gpu = unsafe { staging.GetGPUVirtualAddress() };
    convert(
        list,
        src_gpu,
        Region {
            width: size[0],
            height: size[1],
            depth: size[2],
            src_offset: 0,
            src_pitch: pitch,
            src_slice: pitch * size[1],
        },
    )
}

/// A whole title texture of R11G11B10_FLOAT (`from`, in the copy source state) copied into the stand-in `to` (in the
/// copy destination state), subresource by subresource. Whether it was.
pub(crate) fn copy_whole(list: &HostList, to: &ID3D12Resource, from: &ID3D12Resource) -> bool {
    // SAFETY: live textures.
    let (a, b) = unsafe { (to.GetDesc(), from.GetDesc()) };
    let same = a.Dimension == b.Dimension
        && a.Width == b.Width
        && a.Height == b.Height
        && a.DepthOrArraySize == b.DepthOrArraySize
        && a.MipLevels == b.MipLevels;
    if !same {
        return false;
    }
    let layers = if a.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { u32::from(a.DepthOrArraySize) };
    for i in 0..u32::from(a.MipLevels).max(1) * layers {
        let Some(c) = texture_region(list, from, i, None) else {
            return false;
        };
        copy_converted(list, to, i, [0, 0, 0], &c);
    }
    true
}

/// Records the copy of converted texels into subresource `subresource` of the stand-in `to` at `corner`.
pub(crate) fn copy_converted(list: &HostList, to: &ID3D12Resource, subresource: u32, corner: [u32; 3], from: &Converted) {
    let dst = D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(to) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: subresource },
    };
    let [width, height, depth] = from.footprint.size();
    let src = D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(&from.buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: 0,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: DXGI_FORMAT(HALVES as i32),
                    Width: width,
                    Height: height,
                    Depth: depth,
                    RowPitch: from.footprint.row_pitch(),
                },
            },
        },
    };
    // SAFETY: an open list; both locations name live resources, the buffer held by the list.
    unsafe { list.list.CopyTextureRegion(&dst, corner[0], corner[1], corner[2], &src, None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shift of the channels' bits is the conversion: every 11-bit and 10-bit float is the same number as the
    /// 16-bit float it becomes (infinities and NaNs too), and alpha is one.
    #[test]
    fn a_small_float_is_the_half_float_of_its_bits_moved_up() {
        for bits in 0..0x800u32 {
            let got = half(texel(bits)[0]);
            let want = small(bits, 6);
            assert!(got == want || got.is_nan() && want.is_nan(), "11-bit {bits:#x}: {got} {want}");
            assert_eq!(half(texel(bits << 11)[1]).to_bits(), want.to_bits(), "green {bits:#x}");
        }
        for bits in 0..0x400u32 {
            let got = half(texel(bits << 22)[2]);
            let want = small(bits, 5);
            assert!(got == want || got.is_nan() && want.is_nan(), "10-bit {bits:#x}: {got} {want}");
        }
        assert_eq!(texel(0xffff_ffff)[3], 0x3c00);
        assert_eq!(half(0x3c00), 1.0);
        assert_eq!(half(0xbc00), -1.0);
        assert_eq!(half(1), 2f32.powi(-24));
        // 1.0 in each channel (exponent 15).
        assert_eq!(texel(15 << 6 | (15 << 6) << 11 | (15 << 5) << 22), [0x3c00, 0x3c00, 0x3c00, 0x3c00]);
        // The infinity of each stays one; a NaN stays one.
        assert_eq!(texel(31 << 6)[0], 0x7c00);
        assert!(half(texel(31 << 6 | 1)[0]).is_nan());
    }

    /// What the probe watch reads of a voxel is the same in both formats, whatever the bits.
    #[test]
    fn a_voxel_reads_the_same_packed_or_converted() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 16) as u32
        };
        let mut words: Vec<u32> = (0..1 << 18).map(|_| next()).collect();
        // Small values (the interesting ones of a light volume) and the single channels.
        words.extend((0..0x800u32).flat_map(|b| [b, b << 11, (b & 0x3ff) << 22]));
        words.push(0);
        for bits in words {
            let (a, b) = (Voxel::packed(bits), Voxel::halves(texel(bits)));
            assert_eq!(a.zero, b.zero, "{bits:#x}");
            assert!(a.magnitude == b.magnitude || a.magnitude.is_nan() && b.magnitude.is_nan(), "{bits:#x}: {a:?} {b:?}");
            assert!(!b.negative, "{bits:#x}");
        }
    }

    /// A stand-in has a sign the packed format has not: what the format would have clamped to zero is counted, and is
    /// no light.
    #[test]
    fn a_negative_channel_is_no_light() {
        let v = Voxel::halves([0xbc00, 0, 0x8000, 0x3c00]);
        assert_eq!(v, Voxel { zero: true, magnitude: 0.0, negative: true });
        let v = Voxel::halves([0xbc00, 0x3c00, 0, 0]);
        assert_eq!(v, Voxel { zero: false, magnitude: 1.0, negative: true });
        // Alpha is not light.
        assert!(Voxel::halves([0, 0, 0, 0x3c00]).zero);
    }

    #[test]
    fn a_region_pitches_its_output_to_256_bytes() {
        let r = Region { width: 32, height: 16, depth: 64, src_offset: 0, src_pitch: 128, src_slice: 2048 };
        assert_eq!((r.dst_pitch(), r.dst_bytes()), (256, 256 * 16 * 64));
        let r = Region { width: 33, ..r };
        assert_eq!(r.dst_pitch(), 512);
        assert!(r.aligned());
        assert!(!Region { src_offset: 2, ..r }.aligned());
    }
}
