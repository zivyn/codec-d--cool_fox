//! Immediate samplers bound through a table of the sampler heap, not as
//! static samplers of the root signature (`VICTORY_SAMPLER_TABLE`).
//!
//! Why: the card's driver (NVIDIA) keeps static samplers in a table of its
//! own and puts the place each one has in that table into the compiled
//! pipeline (`library::with_static_samplers` has the measurement). A
//! pipeline stored in the pipeline library by one process (`victory warm`'s
//! children) and loaded by another (the title) reads through the places of
//! the process that stored it, so it may sample through other samplers than
//! its own. Without static samplers nothing of the kind is in the pipeline:
//! it may go in the library like any other, and the library keeps every
//! pipeline of the title.
//!
//! How: the translated shaders declare their immediate samplers as `s<n>` of
//! [`SAMPLER_SPACE`], one resource each. The root signature binds each
//! through a one-descriptor table of the same register and space, with the
//! visibility the static sampler had, instead of a static sampler. The root
//! signature then holds no sampler description (it depends on the number
//! of samplers only), and the descriptor each table points at is in a
//! region of [`REGION`] slots at the end of every host sampler heap (its
//! slots are what [`slot`] gave out, the same descriptions the static
//! samplers would have held). The place of a description in the region is
//! the process's own and is set at draw time, so nothing about it is stored.
//!
//! The translation does not change: no shader cache entry, no DXIL. The
//! library name has the form of the root signature (tables, not samplers)
//! and a tag of its own ([`TAG`]), so no entry stored for static samplers
//! is ever looked up for a table.

use std::sync::{Mutex, OnceLock};

use victory_shader::emit::SAMPLER_SPACE;
use windows::Win32::Graphics::Direct3D12::*;

use crate::host::host;

/// Slots at the end of each host sampler heap that hold the descriptions of
/// the immediate samplers of all pipelines (the title uses a few tens).
pub(crate) const REGION: u32 = 64;

/// The most descriptors a shader-visible sampler heap holds.
pub(crate) const MAX_HEAP: u32 = 2048;

/// Mixed into the library name of a pipeline whose root signature has
/// sampler tables.
pub(crate) const TAG: &[u8] = b"sampler-table-v1";

/// Whether the table form is the default: off until the pilot has confirmed
/// that it keeps the white-out away with the library in use.
const DEFAULT_ON: bool = false;

/// `VICTORY_SAMPLER_TABLE=1` (`=0`): immediate samplers go through tables of
/// the sampler heap, which lets pipelines that use them go in the pipeline
/// library.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| match std::env::var_os("VICTORY_SAMPLER_TABLE") {
        Some(v) if v == "0" => false,
        Some(v) if v == "1" => true,
        _ => DEFAULT_ON,
    })
}

/// How a title's sampler heap of `count` slots is made as a host heap: the
/// title slots it shows and the region slots at its end. A heap that is
/// full gives up its last slots to the region (a title that wrote a sampler
/// there is warned about by `HostHeap::mirror`); one over the device's limit
/// stays as it was.
pub(crate) fn heap_layout(count: u32) -> (u32, u32) {
    if on() { layout(count) } else { (count, 0) }
}

/// [`heap_layout`] with the table on.
fn layout(count: u32) -> (u32, u32) {
    if count > MAX_HEAP {
        (count, 0)
    } else if count + REGION <= MAX_HEAP {
        (count, REGION)
    } else {
        (MAX_HEAP - REGION, REGION)
    }
}

/// The fields of a description as words, to compare two (the floats by
/// their bits).
#[derive(Clone, Copy, PartialEq)]
struct Key([u32; 13]);

fn key(d: &D3D12_SAMPLER_DESC) -> Key {
    Key([
        d.Filter.0 as u32,
        d.AddressU.0 as u32,
        d.AddressV.0 as u32,
        d.AddressW.0 as u32,
        d.MipLODBias.to_bits(),
        d.MaxAnisotropy,
        d.ComparisonFunc.0 as u32,
        d.BorderColor[0].to_bits(),
        d.BorderColor[1].to_bits(),
        d.BorderColor[2].to_bits(),
        d.BorderColor[3].to_bits(),
        d.MinLOD.to_bits(),
        d.MaxLOD.to_bits(),
    ])
}

/// The descriptions given a slot, in slot order.
static REGISTRY: Mutex<Vec<D3D12_SAMPLER_DESC>> = Mutex::new(Vec::new());

/// The description a heap sampler needs to read as the static sampler `s`
/// does: the same filter, address modes, bias, anisotropy, comparison,
/// border colour and LOD range. None for a border colour the heap's float
/// description cannot give (the integer ones).
pub(crate) fn description(s: &D3D12_STATIC_SAMPLER_DESC) -> Option<D3D12_SAMPLER_DESC> {
    let border = match s.BorderColor {
        D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK => [0.0, 0.0, 0.0, 0.0],
        D3D12_STATIC_BORDER_COLOR_OPAQUE_BLACK => [0.0, 0.0, 0.0, 1.0],
        D3D12_STATIC_BORDER_COLOR_OPAQUE_WHITE => [1.0, 1.0, 1.0, 1.0],
        _ => return None,
    };
    Some(D3D12_SAMPLER_DESC {
        Filter: s.Filter,
        AddressU: s.AddressU,
        AddressV: s.AddressV,
        AddressW: s.AddressW,
        MipLODBias: s.MipLODBias,
        MaxAnisotropy: s.MaxAnisotropy,
        ComparisonFunc: s.ComparisonFunc,
        BorderColor: border,
        MinLOD: s.MinLOD,
        MaxLOD: s.MaxLOD,
    })
}

/// The region slot that holds `desc` (given one if there is none yet). None
/// if the region is full.
pub(crate) fn slot_of(desc: &D3D12_SAMPLER_DESC) -> Option<u32> {
    let mut all = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let wanted = key(desc);
    if let Some(at) = all.iter().position(|d| key(d) == wanted) {
        return Some(at as u32);
    }
    if all.len() as u32 >= REGION {
        return None;
    }
    all.push(*desc);
    Some(all.len() as u32 - 1)
}

/// [`slot_of`] of the description of the static sampler `s`.
pub(crate) fn slot(s: &D3D12_STATIC_SAMPLER_DESC) -> Option<u32> {
    slot_of(&description(s)?)
}

/// How many slots have been given.
pub(crate) fn len() -> u32 {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner()).len() as u32
}

/// The description of slot `slot`.
pub(crate) fn desc(slot: u32) -> Option<D3D12_SAMPLER_DESC> {
    REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(slot as usize)
        .copied()
}

/// The slots of the samplers of one stage, in register order, or None if
/// one of them has none (the pipeline then keeps static samplers).
pub(crate) fn slots(samplers: &[D3D12_STATIC_SAMPLER_DESC]) -> Option<Vec<u32>> {
    samplers.iter().map(slot).collect()
}

/// The range of the table that binds immediate sampler `register`: one
/// descriptor of its register and space. Volatile, as every range must be
/// with the heaps indexed directly.
pub(crate) fn range(register: u32) -> D3D12_DESCRIPTOR_RANGE1 {
    D3D12_DESCRIPTOR_RANGE1 {
        RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER,
        NumDescriptors: 1,
        BaseShaderRegister: register,
        RegisterSpace: SAMPLER_SPACE,
        Flags: D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE,
        OffsetInDescriptorsFromTableStart: 0,
    }
}

/// The root parameter of a table of one `range`, for the stage of
/// `visibility`.
pub(crate) fn parameter(
    range: *const D3D12_DESCRIPTOR_RANGE1,
    visibility: D3D12_SHADER_VISIBILITY,
) -> D3D12_ROOT_PARAMETER1 {
    D3D12_ROOT_PARAMETER1 {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
        Anonymous: D3D12_ROOT_PARAMETER1_0 {
            DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 {
                NumDescriptorRanges: 1,
                pDescriptorRanges: range,
            },
        },
        ShaderVisibility: visibility,
    }
}

/// Writes slots `from..to` of the region that starts at CPU handle `start`
/// (`step` bytes a slot) into a sampler heap.
pub(crate) fn write(device: &ID3D12Device, start: usize, step: usize, from: u32, to: u32) -> bool {
    for slot in from..to {
        let Some(desc) = desc(slot) else { return false };
        // SAFETY: a slot of the region of a live sampler heap.
        unsafe {
            device.CreateSampler(
                &desc,
                D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: start + slot as usize * step,
                },
            );
        }
    }
    true
}

/// The region of the sampler heap of our own, bound when the title binds
/// none: its first slot (the heap's slot 0 is its null sampler, the region
/// follows it) as a GPU handle, and the size of a slot, with the first `len`
/// descriptions written.
pub(crate) fn fallback(len: u32) -> Option<(u64, u64)> {
    static WRITTEN: Mutex<u32> = Mutex::new(0);
    let host = host()?;
    let heap = host.fallback_sampler_heap()?;
    // SAFETY: a plain query.
    let step = unsafe {
        host.device
            .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER)
    } as usize;
    // SAFETY: a live heap.
    let (cpu, gpu) = unsafe {
        (
            heap.GetCPUDescriptorHandleForHeapStart().ptr + step,
            heap.GetGPUDescriptorHandleForHeapStart().ptr + step as u64,
        )
    };
    let mut written = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if len > REGION {
        return None;
    }
    if *written < len {
        if !write(&host.device, cpu, step, *written, len) {
            return None;
        }
        *written = len;
    }
    Some((gpu, step as u64))
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    use windows::core::Interface;

    use super::*;

    fn point(register: u32, visibility: D3D12_SHADER_VISIBILITY) -> D3D12_STATIC_SAMPLER_DESC {
        D3D12_STATIC_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_MIRROR,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            MipLODBias: 0.5,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
            BorderColor: D3D12_STATIC_BORDER_COLOR_OPAQUE_WHITE,
            MinLOD: 0.0,
            MaxLOD: 12.0,
            ShaderRegister: register,
            RegisterSpace: SAMPLER_SPACE,
            ShaderVisibility: visibility,
        }
    }

    /// The root signature of the production flags with these parameters,
    /// serialized and read back (version 1.1, as the host makes it).
    fn round_trip(
        params: &[D3D12_ROOT_PARAMETER1],
        samplers: &[D3D12_STATIC_SAMPLER_DESC],
    ) -> Vec<u8> {
        let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
            Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
            Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                    NumParameters: params.len() as u32,
                    pParameters: params.as_ptr(),
                    NumStaticSamplers: samplers.len() as u32,
                    pStaticSamplers: samplers.as_ptr(),
                    Flags: D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
                        | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED,
                },
            },
        };
        let mut blob: Option<ID3DBlob> = None;
        let mut error: Option<ID3DBlob> = None;
        // SAFETY: the description points at live arrays.
        let made =
            unsafe { D3D12SerializeVersionedRootSignature(&desc, &mut blob, Some(&mut error)) };
        if let (Err(e), Some(error)) = (&made, &error) {
            // SAFETY: the error blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    error.GetBufferPointer().cast::<u8>(),
                    error.GetBufferSize(),
                )
            };
            panic!("{e}: {}", String::from_utf8_lossy(text));
        }
        made.expect("the root signature serializes");
        let blob = blob.expect("a blob");
        // SAFETY: the blob's own bytes.
        unsafe {
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
        }
        .to_vec()
    }

    /// What a serialized root signature says: its parameters (type,
    /// visibility, and for a table its ranges as (type, count, register,
    /// space, flags, offset)) and its number of static samplers.
    type Read = (Vec<(i32, i32, Vec<(i32, u32, u32, u32, i32, u32)>)>, u32);

    fn read(bytes: &[u8]) -> Read {
        let mut out: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: the blob's bytes; the interface is released with the wrapper.
        let reader = unsafe {
            D3D12CreateVersionedRootSignatureDeserializer(
                bytes.as_ptr().cast(),
                bytes.len(),
                &ID3D12VersionedRootSignatureDeserializer::IID,
                &mut out,
            )
            .expect("the blob deserializes");
            ID3D12VersionedRootSignatureDeserializer::from_raw(out)
        };
        // SAFETY: the deserializer owns the description it returns.
        unsafe {
            let versioned = &*reader
                .GetRootSignatureDescAtVersion(D3D_ROOT_SIGNATURE_VERSION_1_1)
                .expect("version 1.1");
            let desc = versioned.Anonymous.Desc_1_1;
            let params = (0..desc.NumParameters as usize)
                .map(|i| {
                    let p = &*desc.pParameters.add(i);
                    let ranges = if p.ParameterType == D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE {
                        let t = p.Anonymous.DescriptorTable;
                        (0..t.NumDescriptorRanges as usize)
                            .map(|j| {
                                let r = &*t.pDescriptorRanges.add(j);
                                (
                                    r.RangeType.0,
                                    r.NumDescriptors,
                                    r.BaseShaderRegister,
                                    r.RegisterSpace,
                                    r.Flags.0,
                                    r.OffsetInDescriptorsFromTableStart,
                                )
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    (p.ParameterType.0, p.ShaderVisibility.0, ranges)
                })
                .collect();
            (params, desc.NumStaticSamplers)
        }
    }

    #[test]
    fn a_static_sampler_becomes_a_table_of_its_register_space_and_stage() {
        let samplers = [
            point(0, D3D12_SHADER_VISIBILITY_PIXEL),
            point(1, D3D12_SHADER_VISIBILITY_PIXEL),
            point(0, D3D12_SHADER_VISIBILITY_VERTEX),
        ];
        let ranges: Vec<_> = samplers.iter().map(|s| range(s.ShaderRegister)).collect();
        let params: Vec<_> = samplers
            .iter()
            .zip(&ranges)
            .map(|(s, r)| parameter(r, s.ShaderVisibility))
            .collect();
        let (read, statics) = read(&round_trip(&params, &[]));
        assert_eq!(statics, 0, "no static sampler is left");
        assert_eq!(read.len(), 3);
        for ((kind, visibility, ranges), s) in read.iter().zip(&samplers) {
            assert_eq!(*kind, D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE.0);
            assert_eq!(*visibility, s.ShaderVisibility.0);
            assert_eq!(
                ranges,
                &[(
                    D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER.0,
                    1,
                    s.ShaderRegister,
                    SAMPLER_SPACE,
                    D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE.0,
                    0
                )]
            );
        }
    }

    #[test]
    fn the_root_signature_of_tables_holds_nothing_of_the_samplers_themselves() {
        // Two stages with different descriptions of the same registers: the
        // same root signature bytes (so the library name and the pipeline
        // do not depend on them).
        let one = [point(0, D3D12_SHADER_VISIBILITY_PIXEL)];
        let mut other = one;
        other[0].Filter = D3D12_FILTER_MIN_MAG_MIP_LINEAR;
        other[0].AddressU = D3D12_TEXTURE_ADDRESS_MODE_BORDER;
        let build = |s: &[D3D12_STATIC_SAMPLER_DESC; 1]| {
            let ranges: Vec<_> = s.iter().map(|s| range(s.ShaderRegister)).collect();
            let params: Vec<_> = s
                .iter()
                .zip(&ranges)
                .map(|(s, r)| parameter(r, s.ShaderVisibility))
                .collect();
            round_trip(&params, &[])
        };
        assert_eq!(build(&one), build(&other));
        // And not the root signature of the static samplers.
        assert_ne!(build(&one), round_trip(&[], &one));
    }

    #[test]
    fn a_static_sampler_and_the_description_of_its_slot_say_the_same() {
        let mut s = point(3, D3D12_SHADER_VISIBILITY_ALL);
        s.Filter = D3D12_FILTER_COMPARISON_MIN_MAG_LINEAR_MIP_POINT;
        s.ComparisonFunc = D3D12_COMPARISON_FUNC_GREATER;
        s.MaxAnisotropy = 8;
        for (preset, color) in [
            (
                D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK,
                [0.0, 0.0, 0.0, 0.0],
            ),
            (D3D12_STATIC_BORDER_COLOR_OPAQUE_BLACK, [0.0, 0.0, 0.0, 1.0]),
            (D3D12_STATIC_BORDER_COLOR_OPAQUE_WHITE, [1.0, 1.0, 1.0, 1.0]),
        ] {
            s.BorderColor = preset;
            let d = description(&s).expect("a float border colour");
            assert_eq!(d.BorderColor, color);
            assert_eq!(
                (
                    d.Filter,
                    d.AddressU,
                    d.AddressV,
                    d.AddressW,
                    d.MipLODBias,
                    d.MaxAnisotropy
                ),
                (
                    s.Filter,
                    s.AddressU,
                    s.AddressV,
                    s.AddressW,
                    s.MipLODBias,
                    s.MaxAnisotropy
                )
            );
            assert_eq!(
                (d.ComparisonFunc, d.MinLOD, d.MaxLOD),
                (s.ComparisonFunc, s.MinLOD, s.MaxLOD)
            );
        }
        // An integer border colour has no float description: static samplers stay.
        s.BorderColor = D3D12_STATIC_BORDER_COLOR_OPAQUE_BLACK_UINT;
        assert!(description(&s).is_none());
    }

    #[test]
    fn equal_descriptions_share_a_slot_and_the_slots_come_in_order() {
        // (The registry is the process's: compare against what it gave.)
        let a = description(&point(0, D3D12_SHADER_VISIBILITY_PIXEL)).unwrap();
        let mut b = a;
        b.Filter = D3D12_FILTER_MIN_MAG_MIP_LINEAR;
        b.MipLODBias = -1.25;
        let (sa, sb) = (slot_of(&a).unwrap(), slot_of(&b).unwrap());
        assert_ne!(sa, sb);
        assert_eq!(slot_of(&a), Some(sa));
        assert_eq!(slot_of(&b), Some(sb));
        assert!(len() > sa.max(sb));
        assert_eq!(key(&desc(sa).unwrap()).0, key(&a).0);
        assert_eq!(key(&desc(sb).unwrap()).0, key(&b).0);
        // The same registers of two stages read the same slots when they hold the same description.
        let list = [
            point(0, D3D12_SHADER_VISIBILITY_PIXEL),
            point(1, D3D12_SHADER_VISIBILITY_PIXEL),
        ];
        assert_eq!(slots(&list), Some(vec![sa, sa]));
    }

    #[test]
    fn a_heap_gives_up_its_last_slots_to_the_region_only_when_it_is_full() {
        assert_eq!(layout(256), (256, REGION));
        assert_eq!(layout(MAX_HEAP - REGION), (MAX_HEAP - REGION, REGION));
        assert_eq!(layout(MAX_HEAP - REGION + 1), (MAX_HEAP - REGION, REGION));
        assert_eq!(layout(MAX_HEAP), (MAX_HEAP - REGION, REGION));
        assert_eq!(layout(MAX_HEAP + 1), (MAX_HEAP + 1, 0));
    }
}
