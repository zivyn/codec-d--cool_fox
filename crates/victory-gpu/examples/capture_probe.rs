//! What a recorded frame (`VICTORY_RECORD`, `docs/replay.md`) bound for one
//! pipeline, read from the capture's files alone: no device is made and
//! nothing is run (`victory replay` hangs some cards).
//!
//!     cargo run --release -p victory-gpu --example capture_probe -- <capture-n folder> <text> [ids]
//!
//! For the first draw of a pipeline whose name holds `text` (the runtime names
//! host pipelines by their shaders' hashes): the root signature's static
//! samplers (of the pipeline and of the list), its root constants, the heap
//! slots constants that are heap places lead to (view descriptions and the
//! resources' descriptions), some words of the constant buffers among them as
//! the CPU last wrote them, and the start-of-frame contents of the 3D RGBA16F
//! textures shown (along x and z through the middle). Resource ids after the
//! text: their contents too (R32_FLOAT rows, RGBA32F texels; `STEP=<n>`).
//! Written to find why PS_CompositeNMB's colour tables posterized.
#![allow(dead_code)]
#[path = "../src/replay/wire.rs"]
mod wire;

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use windows::Win32::Graphics::Direct3D12::*;
use wire::*;

fn half(h: u16) -> f32 {
    let (sign, exp, man) = (
        (h >> 15) as u32,
        ((h >> 10) & 0x1f) as i32,
        (h & 0x3ff) as f32,
    );
    let v = match exp {
        0 => man * 2f32.powi(-24),
        31 => f32::INFINITY,
        e => (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    };
    if sign == 1 { -v } else { v }
}

fn samplers(blob: &[u8]) {
    unsafe {
        let mut d: Option<ID3D12VersionedRootSignatureDeserializer> = None;
        let r = D3D12CreateVersionedRootSignatureDeserializer(
            blob.as_ptr().cast(),
            blob.len(),
            &<ID3D12VersionedRootSignatureDeserializer as windows::core::Interface>::IID,
            std::ptr::from_mut(&mut d).cast(),
        );
        if r.is_err() {
            println!("  root signature: no deserializer {r:?}");
            return;
        }
        let d = d.unwrap();
        let Ok(desc) = d.GetRootSignatureDescAtVersion(D3D_ROOT_SIGNATURE_VERSION_1_1) else {
            return;
        };
        let desc = &(*desc).Anonymous.Desc_1_1;
        let s = std::slice::from_raw_parts(desc.pStaticSamplers, desc.NumStaticSamplers as usize);
        for x in s {
            println!(
                "  static sampler s{} space {} vis {} filter {:#x} addr {} {} {} lod {}..{} bias {} aniso {} cmp {}",
                x.ShaderRegister,
                x.RegisterSpace,
                x.ShaderVisibility.0,
                x.Filter.0,
                x.AddressU.0,
                x.AddressV.0,
                x.AddressW.0,
                x.MinLOD,
                x.MaxLOD,
                x.MipLODBias,
                x.MaxAnisotropy,
                x.ComparisonFunc.0
            );
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let folder = std::path::PathBuf::from(&args[1]);
    let text = &args[2];
    let frame = Frame::read(&folder.join("frame.bin"))?;
    let resources: HashMap<u32, &Resource> = frame.resources.iter().map(|r| (r.id, r)).collect();
    let roots: HashMap<u32, &Blob> = frame.roots.iter().map(|(i, b)| (*i, b)).collect();
    let heaps: HashMap<u32, &DescriptorHeap> =
        frame.descriptor_heaps.iter().map(|h| (h.id, h)).collect();
    let pipes: HashMap<u32, &Pipeline> = frame.pipelines.iter().map(|p| (p.id, p)).collect();
    for h in &frame.descriptor_heaps {
        println!(
            "descriptor heap {} kind {} count {} flags {}",
            h.id, h.kind, h.count, h.flags
        );
    }
    let wanted: Vec<u32> = frame
        .pipelines
        .iter()
        .filter(|p| p.name.contains(text.as_str()))
        .map(|p| p.id)
        .collect();
    for p in &wanted {
        let pipe = pipes[p];
        println!("pipeline {p} '{}'", pipe.name);
        for part in &pipe.parts {
            if let Part::Root { root } = part {
                println!(" root {root}");
                if let Some(b) = roots.get(root) {
                    samplers(&b.0);
                }
            }
        }
    }
    let called = |id: u32| match resources.get(&id) {
        None => format!("#{id} (none)"),
        Some(r) => format!(
            "#{id} '{}' dim {} {}x{}x{} m{} f{} flags {:#x} layout {}",
            r.name,
            r.desc.dimension,
            r.desc.width,
            r.desc.height,
            r.desc.depth,
            r.desc.mips,
            r.desc.format,
            r.desc.flags,
            r.desc.layout
        ),
    };
    let mut late: HashMap<(u32, u32), Slot> = HashMap::new();
    let mut all_deltas: Vec<Delta> = Vec::new();
    let mut deltas_file = std::fs::File::open(folder.join("deltas.bin"))?;
    // The bytes of buffer `res` at `at`, as the CPU last wrote them by now.
    let mut word_at = |deltas: &[Delta], res: u32, at: u64| -> Option<u32> {
        let d = deltas
            .iter()
            .rev()
            .find(|d| d.res == res && d.at <= at && at + 4 <= d.at + d.len)?;
        let mut b = [0u8; 4];
        deltas_file
            .seek(SeekFrom::Start(d.file + (at - d.at)))
            .ok()?;
        deltas_file.read_exact(&mut b).ok()?;
        Some(u32::from_le_bytes(b))
    };
    let mut shown = 0;
    let mut lut_ids: Vec<u32> = Vec::new();
    for s in &frame.submits {
        for (heap, at, slot) in &s.views {
            late.insert((*heap, *at), *slot);
        }
        let slot_at = |heap: u32, at: u32| -> Slot {
            if let Some(s) = late.get(&(heap, at)) {
                return *s;
            }
            let Some(h) = heaps.get(&heap) else {
                return Slot::EMPTY;
            };
            let after = h.runs.partition_point(|r| r.0 <= at);
            match after.checked_sub(1).map(|n| &h.runs[n]) {
                Some((first, count, slot)) if at - first < *count => *slot,
                _ => Slot::EMPTY,
            }
        };
        all_deltas.extend(s.deltas.iter().cloned());
        for &l in s.lists.iter().filter(|&&l| l != MISSING) {
            let mut pso = 0;
            let mut heap = 0;
            let mut list_root = 0u32;
            let mut consts: HashMap<u32, Vec<u32>> = HashMap::new();
            for c in Cmd::all(&frame.lists[l as usize].commands.0)? {
                match c {
                    Cmd::Pipeline { pso: p } => pso = p,
                    Cmd::Heaps { heaps } => heap = heaps.first().copied().unwrap_or(0),
                    Cmd::RootSignature {
                        graphics: true,
                        root,
                    } => {
                        consts.clear();
                        list_root = root;
                    }
                    Cmd::Constants {
                        graphics: true,
                        param,
                        offset,
                        values,
                    } => {
                        let all = consts.entry(param).or_default();
                        let end = offset as usize + values.len();
                        if all.len() < end {
                            all.resize(end, 0);
                        }
                        all[offset as usize..end].copy_from_slice(&values);
                    }
                    c if c.runs_shaders() && wanted.contains(&pso) && shown < 1 => {
                        shown += 1;
                        println!("draw {c:?} heap {heap} list root {list_root}");
                        if let Some(b) = roots.get(&list_root) {
                            samplers(&b.0);
                        }
                        let mut params: Vec<_> = consts.iter().collect();
                        params.sort();
                        for (param, values) in params {
                            println!(" param {param}: {values:x?}");
                            for &v in values {
                                if v & 31 != 0 || v >> 5 < 64 {
                                    continue;
                                }
                                for k in 0..48u32 {
                                    let at = (v >> 5) + k;
                                    let sl = slot_at(heap, at);
                                    if sl.kind == 0 {
                                        continue;
                                    }
                                    let w = |i: usize| {
                                        u32::from_le_bytes(
                                            sl.desc[4 * i..4 * i + 4].try_into().unwrap(),
                                        )
                                    };
                                    if sl.kind == 1 {
                                        let base = u64::from(w(0)) | u64::from(w(1)) << 32;
                                        let mut text = String::new();
                                        for off in [
                                            0x50u64, 0x26c, 0x310, 0x318, 0x3c0, 0x400, 0x408,
                                            0x40c, 0x410, 0x500, 0x514, 0x51c, 0x520, 0x524, 0x528,
                                        ] {
                                            if let Some(v) =
                                                word_at(&all_deltas, sl.res, base + off)
                                            {
                                                text += &format!(
                                                    " [{off:#x}]={v:#x}/{}",
                                                    f32::from_bits(v)
                                                );
                                            }
                                        }
                                        println!("   cbv slot {at} words:{text}");
                                    }
                                    println!(
                                        "   slot {at} (+{} bytes) kind {} described {} desc [{:x} {:x} {:x} {:x} {:x} {:x} {:x} {:x}] {}",
                                        k * 32,
                                        sl.kind,
                                        sl.described,
                                        w(0),
                                        w(1),
                                        w(2),
                                        w(3),
                                        w(4),
                                        w(5),
                                        w(6),
                                        w(7),
                                        called(sl.res)
                                    );
                                    if let Some(r) = resources.get(&sl.res)
                                        && r.desc.dimension == 4
                                        && !lut_ids.contains(&sl.res)
                                    {
                                        lut_ids.push(sl.res);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // Start-of-frame contents of the 3D textures shown.
    let mut file = std::fs::File::open(folder.join("contents.bin"))?;
    // Small float textures named on the command line: every texel.
    for id in args[3..].iter().filter_map(|a| a.parse::<u32>().ok()) {
        let r = resources[&id];
        let Some(c) = frame.contents.iter().find(|c| c.res == id) else {
            println!("{}: no contents", called(id));
            continue;
        };
        println!("{}: contents len {} file {:#x}", called(id), c.len, c.file);
        if c.file == ZERO {
            println!("  all zero");
            continue;
        }
        let mut bytes = vec![0u8; c.len as usize];
        file.seek(SeekFrom::Start(c.file))?;
        file.read_exact(&mut bytes)?;
        let f = |i: usize| f32::from_le_bytes(bytes[4 * i..4 * i + 4].try_into().unwrap());
        match r.desc.format {
            41 => {
                let n = (r.desc.width as usize).min(bytes.len() / 4);
                let step = std::env::var("STEP")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(64usize);
                for i in (0..n).step_by(step) {
                    println!("  [{i:4}] {}", f(i));
                }
                // Distinct values and the largest jump between neighbours.
                let mut distinct: Vec<u32> = (0..n).map(|i| f(i).to_bits()).collect();
                distinct.dedup();
                let jump = (1..n).map(|i| (f(i) - f(i - 1)).abs()).fold(0f32, f32::max);
                println!(
                    "  runs of equal values: {} of {n}; largest step {jump}",
                    distinct.len()
                );
            }
            2 => {
                for i in 0..(bytes.len() / 16).min(16) {
                    println!(
                        "  texel {i}: {:?}",
                        [f(4 * i), f(4 * i + 1), f(4 * i + 2), f(4 * i + 3)]
                    );
                }
            }
            _ => {}
        }
    }
    for id in lut_ids {
        let r = resources[&id];
        let Some(c) = frame.contents.iter().find(|c| c.res == id) else {
            println!("{}: no contents", called(id));
            continue;
        };
        println!(
            "{}: contents first {} count {} len {} file {:#x}",
            called(id),
            c.first,
            c.count,
            c.len,
            c.file
        );
        if c.file == ZERO || r.desc.format != 10 {
            continue;
        }
        let (w, h, d) = (
            r.desc.width as usize,
            r.desc.height as usize,
            r.desc.depth as usize,
        );
        let pitch = (w * 8).div_ceil(256) * 256;
        let mut bytes = vec![0u8; (pitch * h * d).min(c.len as usize)];
        file.seek(SeekFrom::Start(c.file))?;
        file.read_exact(&mut bytes)?;
        let texel = |x: usize, y: usize, z: usize| -> [f32; 4] {
            let at = z * pitch * h + y * pitch + x * 8;
            [0, 1, 2, 3].map(|i| {
                half(u16::from_le_bytes([
                    bytes[at + 2 * i],
                    bytes[at + 2 * i + 1],
                ]))
            })
        };
        let (my, mz) = (h / 2, d / 2);
        println!("  along x (y={my}, z={mz}):");
        for x in 0..w {
            println!("    x {x:2} {:?}", texel(x, my, mz));
        }
        println!("  along z (x={}, y={my}):", w / 2);
        for z in 0..d {
            println!("    z {z:2} {:?}", texel(w / 2, my, z));
        }
    }
    Ok(())
}
