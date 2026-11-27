//! I/O raising (decision 0003): what a program's input registers, its
//! interpolation and its exports mean.
//!
//! Pixel shaders get the VGPRs `SPI_PS_INPUT_ADDR` enables, in a fixed
//! order (barycentrics, then position, front face...). `v_interp_p1` and
//! `v_interp_p2` (and their f16 forms) evaluate an attribute from a pair of
//! barycentrics; `v_interp_mov` reads one vertex's value. After the 32-bit
//! export patch ([`NativeProgram::patched_code`]) exports carry plain
//! 32-bit values: `mrt<n>` is render target n, `mrtz` depth, stencil and
//! sample mask, `null` nothing. A lane whose EXEC bit is clear at the
//! export that ends the program (`done`) is discarded: that export's guard
//! after structurisation.
//!
//! Vertex shaders that run as the hardware VS get the vertex id in v0, the
//! primitive id in v2 and the instance id in v3; the fetch shader loads
//! channel k of each attribute into its VGPR + k. `exp pos0` is the
//! position, `exp param<n>` the output with parameter n.
//!
//! Attributes and outputs are named by their row: a register of the
//! container's signature part.
//!
//! Compute programs get the thread id in the group in v0-v2 and the group
//! id in the SGPRs after the user data, as `COMPUTE_PGM_RSRC2` enables them.
//!
//! Mesh programs run as one NGG stage. The program computes its thread
//! index from the wave index (s3 bits 24-27) and the lane (`v_mbcnt`), and
//! its group id from v5 of the first lane (`v_readfirstlane`: the group
//! times the group size). Lane n exports vertex n (`exp pos`, `exp param`)
//! and primitive n (`exp prim`: 9-bit vertex indices at bits 0, 10, 20,
//! each with an edge flag above it, bit 31 culls); `param` entries with
//! bit 7 are per-primitive outputs. One lane sends the output counts
//! (`s_sendmsg` GS_ALLOC_REQ, m0 = vertices | primitives << 12).
//!
//! Merged geometry stages run a vertex shader (the export program, stage
//! ES) and the geometry shader it feeds (stage GS) as the two halves of one
//! wave, the vertices between them in LDS. The export program gets the
//! vertex id in v5, the primitive id in v7 and the instance id in v8, and
//! writes its outputs to its vertex's slot of the vertex ring (`ds_write*`
//! at the lane's slot: dword 4 * row + channel). The geometry program gets
//! its primitive's vertices as ring offsets in dwords, two 16-bit ones per
//! register (v0: vertices 0 and 1, v1: 2 and 3, v4: 4 and 5), the primitive
//! id in v2 and its instance in v3; it reads the vertices' outputs there
//! (`ds_read*`), writes each vertex it makes to the output ring
//! (`ds_write*`) and emits it or cuts the strip with `s_sendmsg` (GS, then
//! GS_DONE). The ring accesses are raised to inputs and outputs by row and
//! channel.

use crate::ShaderError;
use crate::gfx10::{Fields, Operand};
use crate::ir::{Exit, Inst, Program, Reg, Value, operand_reads};
use crate::native::{HwStage, NativeProgram, PsInput, StageIo, Usage};
use crate::resource::Site;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    VertexId,
    PrimitiveId,
    InstanceId,
    /// A vertex attribute the fetch shader loads.
    Attribute {
        row: u8,
        channel: u8,
    },
    /// `index` 0 is i, 1 is j (2 the pull model's w).
    Barycentric {
        kind: Barycentric,
        index: u8,
    },
    /// Pixel position channel (0-3: x, y, z, w).
    Position(u8),
    FrontFace,
    /// Sample id and other bits.
    Ancillary,
    SampleCoverage,
    FixedPosition,
    LineStipple,
    /// Compute: thread-group id component (an SGPR).
    GroupId(u8),
    /// Compute: thread id within the group, component (a VGPR).
    ThreadIdInGroup(u8),
    /// Compute: the thread-group info SGPR (wave index and count).
    GroupInfo,
    /// Mesh: v5, the group id times the group size (read as uniform).
    MeshGroup,
    /// Mesh: s3, the wave's index in the group in bits 24-27.
    MergedWaveInfo,
    /// Geometry: v3, the GS invocation / instance id (`SV_GSInstanceID`).
    GsInstanceId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Barycentric {
    PerspectiveSample,
    PerspectiveCenter,
    PerspectiveCentroid,
    PerspectivePullModel,
    LinearSample,
    LinearCenter,
    LinearCentroid,
}

/// The pixel input VGPRs in hardware order: one entry per
/// `SPI_PS_INPUT_ADDR` bit.
const PS_VGPRS: [(u8, Option<Barycentric>); 16] = [
    (2, Some(Barycentric::PerspectiveSample)),
    (2, Some(Barycentric::PerspectiveCenter)),
    (2, Some(Barycentric::PerspectiveCentroid)),
    (3, Some(Barycentric::PerspectivePullModel)),
    (2, Some(Barycentric::LinearSample)),
    (2, Some(Barycentric::LinearCenter)),
    (2, Some(Barycentric::LinearCentroid)),
    (1, None), // line stipple
    (1, None), // position x
    (1, None), // y
    (1, None), // z
    (1, None), // w
    (1, None), // front face
    (1, None), // ancillary
    (1, None), // sample coverage
    (1, None), // fixed-point position
];

fn ps_system(bit: usize) -> Input {
    match bit {
        7 => Input::LineStipple,
        8..=11 => Input::Position(bit as u8 - 8),
        12 => Input::FrontFace,
        13 => Input::Ancillary,
        14 => Input::SampleCoverage,
        _ => Input::FixedPosition,
    }
}

/// An attribute channel of the input signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attribute {
    pub usage: Usage,
    pub row: u8,
    pub channel: u8,
    /// The upper f16 half of the channel (packed fp16 inputs).
    pub high: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interp {
    /// This site's result (`v_interp_p2*`) is the attribute interpolated
    /// with `barycentric`; `first` are the `v_interp_p1*` halves it
    /// completes (one per path where the compiler shares a first half).
    Interpolated {
        attribute: Attribute,
        barycentric: Barycentric,
        first: Vec<Site>,
    },
    /// One vertex's value (`v_interp_mov_f32`), as encoded: 0 for P10,
    /// 1 for P20, 2 for P0 (the provoking vertex).
    Vertex { attribute: Attribute, vertex: u8 },
    /// A half of an interpolation whose i or j is computed rather than
    /// loaded: each half evaluates as the hardware does, from the
    /// attribute's vertex values (`v_interp_p1*`: P0 + i P10, with P10 =
    /// P1 - P0; `v_interp_p2*`: its old dst + j P20, with P20 = P2 - P0).
    Weighted { attribute: Attribute },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Color(u8),
    /// Depth, stencil, sample mask (channels as `DB_SHADER_CONTROL` enables).
    Depth,
    Null,
    Position(u8),
    Param {
        usage: Usage,
        row: u8,
    },
    /// `param0` of a vertex shader without outputs: the hardware wants one
    /// parameter export regardless.
    Unread,
    /// The primitive export of a merged or mesh stage.
    Primitive,
    /// A mesh program's per-primitive output (a row of `PSG1`).
    PrimitiveParam {
        usage: Usage,
        row: u8,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub site: Site,
    pub target: Target,
    /// Per channel, as the instruction enables them.
    pub values: [Option<Value>; 4],
    /// Two f16 values per dword.
    pub compressed: bool,
    /// The program's last export (for pixels: kills the lanes not in EXEC).
    pub done: bool,
}

/// One per-vertex input channel a geometry shader reads from ES->GS LDS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GsInput {
    pub vertex: u8,
    pub row: u8,
    pub channel: u8,
}

/// One output channel an ES or GS writes to ES->GS or GSVS LDS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LdsOutput {
    pub vertex: u32,
    pub row: u8,
    pub channel: u8,
    pub value: Value,
}

/// A geometry shader stream message (`s_sendmsg(MSG_GS / MSG_GS_DONE)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GsOp {
    Emit(u8),
    Cut(u8),
    EmitCut(u8),
    Done,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Io {
    /// Input values the hardware loads, with their meaning; other VGPRs
    /// read before written hold undefined values.
    pub inputs: Vec<(Value, Input)>,
    pub interps: Vec<(Site, Interp)>,
    pub exports: Vec<Export>,
    /// Compute: how many SGPRs the hardware initializes
    /// ([`ComputeIo::initialized_sgprs`](crate::native::ComputeIo::initialized_sgprs)).
    pub initialized_sgprs: Option<u8>,
    /// Geometry shader per-vertex input reads from ES->GS LDS (`ds_read*`),
    /// each entry `(dst_vgpr_offset, GsInput)`.
    pub gs_reads: Vec<(Site, Vec<(u8, GsInput)>)>,
    /// ES or GS vertex outputs written to ES->GS or GSVS LDS (`ds_write*`).
    pub lds_outputs: Vec<(Site, Vec<LdsOutput>)>,
    /// Geometry shader stream operations (`s_sendmsg`).
    pub gs_ops: Vec<(Site, GsOp)>,
}

/// Raises the I/O of `p`, built from `native`'s patched code. Fails for
/// stages whose stage data is not read and on constructs not covered yet.
pub fn raise(p: &Program, native: &NativeProgram) -> Result<Io, ShaderError> {
    let unsupported = |offset, what| ShaderError::Unsupported { offset, what };
    let (loaded, ps_inputs, params) = match (&native.io, native.stage) {
        (
            StageIo::Ps {
                inputs, input_addr, ..
            },
            HwStage::Ps,
        ) => {
            // A pixel shader exports no parameters; `[]` never reaches `Unread`.
            let loaded = ps_vgprs(*input_addr)
                .into_iter()
                .map(|(r, i)| (Reg::V(r), i))
                .collect();
            (loaded, &inputs[..], &[][..])
        }
        (StageIo::Vs { inputs, outputs }, HwStage::Vs) => {
            let mut loaded = vec![
                (Reg::V(0), Input::VertexId),
                (Reg::V(2), Input::PrimitiveId),
                (Reg::V(3), Input::InstanceId),
            ];
            for i in inputs {
                for channel in (0..4).filter(|c| i.mask >> c & 1 != 0) {
                    let input = Input::Attribute {
                        row: i.row,
                        channel,
                    };
                    loaded.push((Reg::V(i.vgpr + channel), input));
                }
            }
            (loaded, &[][..], &outputs[..])
        }
        // Compute: thread ids in v0-v2, group ids and the group info in the
        // SGPRs after the user data.
        (StageIo::Cs(cs), HwStage::Cs) => {
            let (ids, info) = cs.system_sgprs();
            let mut loaded: Vec<(Reg, Input)> = (0..cs.thread_id_vgprs())
                .map(|c| (Reg::V(c), Input::ThreadIdInGroup(c)))
                .collect();
            for (c, s) in ids.into_iter().enumerate() {
                if let Some(s) = s {
                    loaded.push((Reg::S(s), Input::GroupId(c as u8)));
                }
            }
            loaded.extend(info.map(|s| (Reg::S(s), Input::GroupInfo)));
            (loaded, &[][..], &[][..])
        }
        (StageIo::Ms(ms), HwStage::Ms) => {
            // The group input, and the phis that may carry it past a join.
            let mut group: Vec<Value> = p
                .inputs
                .iter()
                .filter(|i| i.0 == Reg::V(5))
                .map(|i| i.1)
                .collect();
            // No lane is active over the EXEC-zero edge of s_cbranch_execz or
            // _execnz, the same case as a partial-EXEC write below.
            let no_lanes = |q: usize, b: usize| match (
                p.blocks[q].insts.last().map(Inst::name),
                p.blocks[q].exit,
            ) {
                (Some("s_cbranch_execz"), Exit::Branch { taken, not_taken }) => {
                    taken == b && not_taken != b
                }
                (Some("s_cbranch_execnz"), Exit::Branch { taken, not_taken }) => {
                    not_taken == b && taken != b
                }
                _ => false,
            };
            while let Some(v) = p
                .blocks
                .iter()
                .enumerate()
                .flat_map(|(b, k)| k.phis.iter().map(move |f| (b, f)))
                .find_map(|(b, f)| {
                    let mut args = f.args.iter().zip(&p.blocks[b].preds);
                    let carried = args.any(|(a, &q)| group.contains(a) && !no_lanes(q, b));
                    (carried && !group.contains(&f.value)).then_some(f.value)
                })
            {
                group.push(v);
            }
            // The group input holds the same value in every lane only as the
            // first lane reads it. A write to v5 reads its old value too, which
            // only lanes outside EXEC keep.
            // shortcut: writes that may keep the old v5 in active lanes
            // (v_fmac_f32's accumulator, a 16-bit half or d16 load,
            // v_writelane_b32, v_movreld_b32, DPP that skips lanes) and
            // partial-EXEC writes pass; check those too if a mesh program
            // writes v5 that way.
            let per_lane = p.blocks.iter().flat_map(|b| &b.insts).find(|i| {
                i.read(Reg::V(5)).is_some_and(|v| group.contains(&v))
                    && i.name() != "v_readfirstlane_b32"
                    && (i.write(Reg::V(5)).is_none() || operand_reads(p, i).contains(&Reg::V(5)))
            });
            if let Some(i) = per_lane {
                return Err(unsupported(
                    i.machine.offset,
                    "the mesh group input read per lane",
                ));
            }
            let loaded = vec![
                (Reg::V(5), Input::MeshGroup),
                (Reg::S(3), Input::MergedWaveInfo),
            ];
            (loaded, &[][..], &ms.outputs[..])
        }
        (
            StageIo::Es {
                inputs, outputs, ..
            },
            HwStage::Es,
        ) => {
            // Merged ES+GS ABI: v0..v4 carry GS inputs; ES system VGPRs start
            // at v5 (vertex id), v7 (primitive id), v8 (instance id).
            let mut loaded = vec![
                (Reg::V(5), Input::VertexId),
                (Reg::V(7), Input::PrimitiveId),
                (Reg::V(8), Input::InstanceId),
            ];
            for i in inputs {
                for channel in (0..4).filter(|c| i.mask >> c & 1 != 0) {
                    let input = Input::Attribute {
                        row: i.row,
                        channel,
                    };
                    loaded.push((Reg::V(i.vgpr + channel), input));
                }
            }
            (loaded, &[][..], &outputs[..])
        }
        (StageIo::Gs(gs), HwStage::Gs) => {
            let loaded = vec![
                (Reg::V(2), Input::PrimitiveId),
                (Reg::V(3), Input::GsInstanceId),
            ];
            (loaded, &[][..], &gs.outputs[..])
        }
        _ => return Err(unsupported(0, "I/O of this stage")),
    };
    let inputs: Vec<(Value, Input)> = p
        .inputs
        .iter()
        .filter_map(|&(reg, v)| loaded.iter().find(|l| l.0 == reg).map(|l| (v, l.1)))
        .collect();
    let input_of = |v: Value| inputs.iter().find(|i| i.0 == v).map(|i| i.1);
    let attribute = |inst: &Inst, attr: &Operand| match *attr {
        Operand::Attr { index, chan, high } => ps_inputs
            .iter()
            .find(|i: &&PsInput| i.attr == index)
            .map(|i| Attribute {
                usage: i.usage,
                row: i.row,
                channel: chan,
                high,
            })
            .ok_or(unsupported(
                inst.machine.offset,
                "an interpolated attribute the stage data does not list",
            )),
        _ => Err(unsupported(inst.machine.offset, "an interpolation operand")),
    };
    // A VGPR value's barycentric kind and index, if it is one as loaded.
    let barycentric = |v: Option<Value>| match v.and_then(input_of) {
        Some(Input::Barycentric { kind, index }) => Some((kind, index)),
        _ => None,
    };

    let mut io = Io {
        inputs: inputs.clone(),
        initialized_sgprs: match &native.io {
            StageIo::Cs(cs) => Some(cs.initialized_sgprs()),
            _ => None,
        },
        ..Io::default()
    };
    let defs: std::collections::HashMap<Value, Site> = p
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(b, block)| {
            block
                .insts
                .iter()
                .enumerate()
                .flat_map(move |(i, inst)| inst.writes.iter().map(move |w| (w.1, (b, i))))
        })
        .collect();
    let phis: std::collections::HashMap<Value, (usize, usize)> = p
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(b, block)| {
            block
                .phis
                .iter()
                .enumerate()
                .map(move |(i, phi)| (phi.value, (b, i)))
        })
        .collect();
    let halves = Halves {
        p,
        defs: &defs,
        phis: &phis,
    };
    let mut first_halves = Vec::new();
    for (b, block) in p.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            let site = (b, i);
            let at = inst.machine.offset;
            let name = inst.name();
            match &inst.fields {
                Fields::Vintrp { dst, src, attr } => {
                    let attribute = attribute(inst, attr)?;
                    match (name, *src) {
                        ("v_interp_mov_f32", Operand::Const(vertex)) => {
                            io.interps
                                .push((site, Interp::Vertex { attribute, vertex }));
                        }
                        ("v_interp_p1_f32", _) => first_halves.push((site, attribute)),
                        ("v_interp_p2_f32", Operand::Vector { reg, .. }) => {
                            let Operand::Vector { reg: d, .. } = *dst else {
                                return Err(unsupported(at, "an interpolation operand"));
                            };
                            // The first half's result is this one's old dst.
                            let interp = halves
                                .pair(
                                    attr,
                                    inst.read(Reg::V(d)),
                                    inst.read(Reg::V(reg)),
                                    &barycentric,
                                )
                                .map_err(|what| unsupported(at, what))?;
                            push_pair(&mut io, site, interp, attribute);
                        }
                        _ => return Err(unsupported(at, "an interpolation form")),
                    }
                }
                Fields::Valu(v) if name.starts_with("v_interp_") => match name {
                    "v_interp_p1ll_f16" => {
                        first_halves.push((site, attribute(inst, &v.src[0])?));
                    }
                    "v_interp_p2_f16" => {
                        let attribute = attribute(inst, &v.src[0])?;
                        let read = |k: usize| match v.src.get(k) {
                            Some(&Operand::Vector { reg, .. }) => inst.read(Reg::V(reg)),
                            _ => None,
                        };
                        let interp = halves
                            .pair(&v.src[0], read(2), read(1), &barycentric)
                            .map_err(|what| unsupported(at, what))?;
                        push_pair(&mut io, site, interp, attribute);
                    }
                    _ => return Err(unsupported(at, "an interpolation form")),
                },
                Fields::Exp {
                    target,
                    src,
                    compr,
                    done,
                    ..
                } => {
                    let target = match *target {
                        t @ 0..=7 => Target::Color(t),
                        8 => Target::Depth,
                        9 => Target::Null,
                        t @ 12..=16 => Target::Position(t - 12),
                        20 => Target::Primitive,
                        32 if params.is_empty() => Target::Unread,
                        t @ 32..=63 => {
                            // Bit 7: a mesh program's per-primitive output.
                            let o = params.iter().find(|o| o.param & 0x7F == t - 32).ok_or(
                                unsupported(at, "a parameter export the stage data does not list"),
                            )?;
                            let (usage, row) = (o.usage, o.row);
                            if o.param & 0x80 != 0 {
                                Target::PrimitiveParam { usage, row }
                            } else {
                                Target::Param { usage, row }
                            }
                        }
                        _ => return Err(unsupported(at, "an export target")),
                    };
                    let values = src.map(|s| match s {
                        Some(Operand::Vector { reg, .. }) => inst.read(Reg::V(reg)),
                        _ => None,
                    });
                    io.exports.push(Export {
                        site,
                        target,
                        values,
                        compressed: *compr,
                        done: *done,
                    });
                }
                _ => {}
            }
        }
    }
    match (&native.io, native.stage) {
        (StageIo::Es { vertex_dwords, .. }, HwStage::Es) => {
            raise_es_gs_lds(p, *vertex_dwords, *vertex_dwords, false, &mut io)?;
        }
        (StageIo::Gs(gs), HwStage::Gs) => {
            raise_es_gs_lds(p, gs.ring_dwords, gs.vertex_dwords, true, &mut io)?;
        }
        _ => {}
    }
    if first_halves.is_empty() {
        return Ok(io);
    }
    // A loaded i/j pair normally becomes one host interpolation: its first
    // half need not be evaluated. A first half also read as data (or used
    // without a second half) must keep P0 + i * P10. Follow phi uses too,
    // without forcing ordinary pairs to read per-vertex attributes.
    let folded: std::collections::HashSet<_> = io
        .interps
        .iter()
        .filter_map(|(s, i)| matches!(i, Interp::Interpolated { .. }).then_some(*s))
        .collect();
    let mut used: std::collections::HashSet<_> = p
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(b, blk)| {
            blk.insts
                .iter()
                .enumerate()
                .map(move |(i, inst)| ((b, i), inst))
        })
        .filter(|(s, _)| !folded.contains(s))
        .flat_map(|(_, inst)| inst.reads.iter().map(|r| r.1))
        .collect();
    loop {
        let before = used.len();
        for phi in p.blocks.iter().flat_map(|b| &b.phis) {
            if used.contains(&phi.value) {
                used.extend(&phi.args);
            }
        }
        if used.len() == before {
            break;
        }
    }
    for (site, attribute) in first_halves {
        if !io.interps.iter().any(|(s, _)| *s == site)
            && p.blocks[site.0].insts[site.1]
                .writes
                .iter()
                .any(|w| used.contains(&w.1))
        {
            io.interps.push((site, Interp::Weighted { attribute }));
        }
    }
    Ok(io)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LdsAddr {
    /// Packed 16-bit ES->GS vertex LDS dword offsets (base_v in low half, base_v+1 in high half).
    Packed(u8),
    /// `packed << 2` (low vertex offset in bits 2..17 before masking with `0x3fffc`).
    ShiftedLow(u8),
    /// Extracted 16-bit vertex dword offset.
    VertexDword(u8),
    /// Resolved byte address inside input vertex `vertex`'s ES->GS LDS slot.
    Vertex(u8, u32),
    /// Resolved byte address inside this thread's ES->GS or GSVS output LDS slot.
    Output(u32),
}

fn ds_elements(name: &str, offset0: u8, offset1: u8) -> Option<Vec<(u32, u32)>> {
    let w = match name.rsplit('_').next() {
        Some("b64") => 2,
        Some("b96") => 3,
        Some("b128") => 4,
        Some("b32") => 1,
        _ => return None,
    };
    let rest = name
        .strip_prefix("ds_read")
        .or_else(|| name.strip_prefix("ds_write"))?;
    if rest.starts_with('2') {
        let scale = 4 * w * if name.contains("st64") { 64 } else { 1 };
        Some(vec![
            (u32::from(offset0) * scale, w),
            (u32::from(offset1) * scale, w),
        ])
    } else if matches!(rest, "_b32" | "_b64" | "_b96" | "_b128") {
        let off = u32::from(offset0) | u32::from(offset1) << 8;
        Some(vec![(off, w)])
    } else {
        None
    }
}

/// Traces ES->GS input reads (`ds_read*`), ES->GS / GSVS output writes
/// (`ds_write*`), and GS stream messages (`s_sendmsg`).
fn raise_es_gs_lds(
    p: &Program,
    stride_dwords: u32,
    vertex_dwords: u32,
    is_gs: bool,
    io: &mut Io,
) -> Result<(), ShaderError> {
    let unsupported = |offset, what| ShaderError::Unsupported { offset, what };
    let stride_bytes = stride_dwords.saturating_mul(4);
    let mut consts: std::collections::HashMap<Value, u32> = std::collections::HashMap::new();
    let mut addrs: std::collections::HashMap<Value, LdsAddr> = std::collections::HashMap::new();
    if is_gs {
        for &(reg, v) in &p.inputs {
            match reg {
                Reg::V(0) => {
                    addrs.insert(v, LdsAddr::Packed(0));
                }
                Reg::V(1) => {
                    addrs.insert(v, LdsAddr::Packed(2));
                }
                Reg::V(4) => {
                    addrs.insert(v, LdsAddr::Packed(4));
                }
                _ => {}
            }
        }
    }
    let val_of = |inst: &Inst, op: &Operand| -> Option<Value> {
        match *op {
            Operand::Vector { reg, .. } => inst.read(Reg::V(reg)),
            Operand::Scalar { code, .. } if code != crate::gfx10::NULL => inst.read(Reg::S(code)),
            _ => None,
        }
    };
    let const_of = |consts: &std::collections::HashMap<Value, u32>,
                    inst: &Inst,
                    op: &Operand|
     -> Option<u32> {
        match *op {
            Operand::Const(k) => crate::resource::inline(k),
            Operand::Literal(x) => Some(x),
            Operand::Scalar { code, .. } if code == crate::gfx10::NULL => Some(0),
            _ => val_of(inst, op).and_then(|v| consts.get(&v).copied()),
        }
    };
    let order = crate::ir::reverse_postorder(&p.blocks);
    for &b in &order {
        for (i, inst) in p.blocks[b].insts.iter().enumerate() {
            let site = (b, i);
            let at = inst.machine.offset;
            let name = inst.name();
            match &inst.fields {
                Fields::Sop {
                    dst: Some(Operand::Scalar { code, .. }),
                    src,
                    ..
                } if name == "s_mov_b32" => {
                    if let Some(w) = inst.write(Reg::S(*code))
                        && let Some(c) = src.first().and_then(|o| const_of(&consts, inst, o))
                    {
                        consts.insert(w, c);
                    }
                }
                Fields::Valu(v) => {
                    let Some(Operand::Vector { reg: dreg, .. }) = v.dst else {
                        continue;
                    };
                    let Some(w) = inst.write(Reg::V(dreg)) else {
                        continue;
                    };
                    if name == "v_mov_b32" && v.sdwa.is_none() {
                        if let Some(c) = v.src.first().and_then(|o| const_of(&consts, inst, o)) {
                            consts.insert(w, c);
                        } else if let Some(a) = v
                            .src
                            .first()
                            .and_then(|o| val_of(inst, o))
                            .and_then(|x| addrs.get(&x).copied())
                        {
                            addrs.insert(w, a);
                        }
                        continue;
                    }
                    // Output ring base: `v_mad_u32_u24 dst, stride_bytes, lane, base`
                    // (or `lane, stride_bytes, base`).
                    if name == "v_mad_u32_u24" && stride_bytes > 0 && v.src.len() >= 2 {
                        let c0 = const_of(&consts, inst, &v.src[0]);
                        let c1 = const_of(&consts, inst, &v.src[1]);
                        if c0 == Some(stride_bytes) || c1 == Some(stride_bytes) {
                            addrs.insert(w, LdsAddr::Output(0));
                            continue;
                        }
                    }
                    if !is_gs {
                        if matches!(name, "v_add_nc_u32" | "v_add_co_u32") && v.src.len() >= 2 {
                            for (a_i, c_i) in [(0, 1), (1, 0)] {
                                if let Some(LdsAddr::Output(off)) =
                                    val_of(inst, &v.src[a_i]).and_then(|x| addrs.get(&x).copied())
                                    && let Some(c) = const_of(&consts, inst, &v.src[c_i])
                                {
                                    addrs.insert(w, LdsAddr::Output(off.wrapping_add(c)));
                                }
                            }
                        }
                        continue;
                    }
                    // GS ES->GS input vertex offset arithmetic.
                    match name {
                        "v_lshlrev_b32" if v.src.len() >= 2 => {
                            if const_of(&consts, inst, &v.src[0]) == Some(2)
                                && let Some(src_a) =
                                    val_of(inst, &v.src[1]).and_then(|x| addrs.get(&x).copied())
                            {
                                let sel = v.sdwa.and_then(|s| s.src_sel.get(1).copied());
                                let out_a = match (src_a, sel) {
                                    (LdsAddr::Packed(base_v), Some(4)) => {
                                        Some(LdsAddr::Vertex(base_v, 0))
                                    }
                                    (LdsAddr::Packed(base_v), Some(5)) => {
                                        Some(LdsAddr::Vertex(base_v + 1, 0))
                                    }
                                    (LdsAddr::Packed(base_v), None | Some(6)) => {
                                        Some(LdsAddr::ShiftedLow(base_v))
                                    }
                                    (LdsAddr::VertexDword(vtx), None | Some(6)) => {
                                        Some(LdsAddr::Vertex(vtx, 0))
                                    }
                                    _ => None,
                                };
                                if let Some(a) = out_a {
                                    addrs.insert(w, a);
                                }
                            }
                        }
                        "v_and_b32" if v.src.len() >= 2 => {
                            for (a_i, c_i) in [(0, 1), (1, 0)] {
                                let src_a =
                                    val_of(inst, &v.src[a_i]).and_then(|x| addrs.get(&x).copied());
                                let mask = const_of(&consts, inst, &v.src[c_i]);
                                match (src_a, mask) {
                                    (Some(LdsAddr::ShiftedLow(base_v)), Some(0x3_fffc)) => {
                                        addrs.insert(w, LdsAddr::Vertex(base_v, 0));
                                    }
                                    (Some(LdsAddr::Packed(base_v)), Some(0xffff)) => {
                                        addrs.insert(w, LdsAddr::VertexDword(base_v));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "v_lshrrev_b32" if v.src.len() >= 2 => {
                            if const_of(&consts, inst, &v.src[0]) == Some(16)
                                && let Some(LdsAddr::Packed(base_v)) =
                                    val_of(inst, &v.src[1]).and_then(|x| addrs.get(&x).copied())
                            {
                                addrs.insert(w, LdsAddr::VertexDword(base_v + 1));
                            }
                        }
                        "v_bfe_u32" if v.src.len() >= 3 => {
                            if let Some(LdsAddr::Packed(base_v)) =
                                val_of(inst, &v.src[0]).and_then(|x| addrs.get(&x).copied())
                                && const_of(&consts, inst, &v.src[2]) == Some(16)
                            {
                                match const_of(&consts, inst, &v.src[1]) {
                                    Some(0) => {
                                        addrs.insert(w, LdsAddr::VertexDword(base_v));
                                    }
                                    Some(16) => {
                                        addrs.insert(w, LdsAddr::VertexDword(base_v + 1));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "v_lshl_add_u32" if v.src.len() >= 3 => {
                            if const_of(&consts, inst, &v.src[1]) == Some(2)
                                && let Some(LdsAddr::VertexDword(vtx)) =
                                    val_of(inst, &v.src[0]).and_then(|x| addrs.get(&x).copied())
                                && let Some(c) = const_of(&consts, inst, &v.src[2])
                            {
                                addrs.insert(w, LdsAddr::Vertex(vtx, c));
                            }
                        }
                        "v_add_nc_u32" | "v_add_co_u32" if v.src.len() >= 2 => {
                            for (a_i, c_i) in [(0, 1), (1, 0)] {
                                let src_a =
                                    val_of(inst, &v.src[a_i]).and_then(|x| addrs.get(&x).copied());
                                let Some(c) = const_of(&consts, inst, &v.src[c_i]) else {
                                    continue;
                                };
                                match src_a {
                                    Some(LdsAddr::Vertex(vtx, off)) => {
                                        addrs.insert(w, LdsAddr::Vertex(vtx, off.wrapping_add(c)));
                                    }
                                    Some(LdsAddr::Output(off)) => {
                                        addrs.insert(w, LdsAddr::Output(off.wrapping_add(c)));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Fields::Ds {
                    addr,
                    data,
                    offset0,
                    offset1,
                    gds,
                    ..
                } => {
                    if *gds {
                        return Err(unsupported(at, "GDS in a merged geometry stage"));
                    }
                    let Some(a_val) = addr.as_ref().and_then(|o| val_of(inst, o)) else {
                        continue;
                    };
                    if name.starts_with("ds_read") && is_gs {
                        let Some(LdsAddr::Vertex(vertex, base_off)) = addrs.get(&a_val).copied()
                        else {
                            return Err(unsupported(
                                at,
                                "a geometry LDS read not of an input vertex",
                            ));
                        };
                        let elems = ds_elements(name, *offset0, *offset1).ok_or_else(|| {
                            unsupported(at, "a geometry LDS read of bytes or halves")
                        })?;
                        let mut reads = Vec::new();
                        let mut dst_off = 0u8;
                        for (off, w) in elems {
                            for k in 0..w {
                                let byte = base_off.wrapping_add(off).wrapping_add(4 * k);
                                if byte % 4 != 0 {
                                    return Err(unsupported(
                                        at,
                                        "an unaligned geometry input read",
                                    ));
                                }
                                let dword = byte / 4;
                                reads.push((
                                    dst_off,
                                    GsInput {
                                        vertex,
                                        row: (dword / 4) as u8,
                                        channel: (dword % 4) as u8,
                                    },
                                ));
                                dst_off += 1;
                            }
                        }
                        io.gs_reads.push((site, reads));
                    } else if name.starts_with("ds_write") {
                        let Some(LdsAddr::Output(base_off)) = addrs.get(&a_val).copied() else {
                            return Err(unsupported(
                                at,
                                "an LDS write of a merged geometry stage not to its outputs",
                            ));
                        };
                        if vertex_dwords == 0 {
                            return Err(unsupported(
                                at,
                                "a merged geometry stage without a vertex size",
                            ));
                        }
                        let elems = ds_elements(name, *offset0, *offset1)
                            .ok_or_else(|| unsupported(at, "an output write of bytes or halves"))?;
                        let mut writes = Vec::new();
                        for (e_i, (off, w)) in elems.into_iter().enumerate() {
                            let Some(&Operand::Vector { reg, .. }) = data.get(e_i) else {
                                return Err(unsupported(at, "an output write without its data"));
                            };
                            for k in 0..w {
                                let value = inst.read(Reg::V(reg + k as u8)).ok_or_else(|| {
                                    unsupported(at, "an output write without its data")
                                })?;
                                let byte = base_off.wrapping_add(off).wrapping_add(4 * k);
                                if byte % 4 != 0 {
                                    return Err(unsupported(at, "an unaligned output write"));
                                }
                                let dword = byte / 4;
                                let (vertex, within) = if is_gs {
                                    (dword / vertex_dwords, dword % vertex_dwords)
                                } else {
                                    (0, dword)
                                };
                                writes.push(LdsOutput {
                                    vertex,
                                    row: (within / 4) as u8,
                                    channel: (within % 4) as u8,
                                    value,
                                });
                            }
                        }
                        io.lds_outputs.push((site, writes));
                    }
                }
                Fields::Sopp { imm } if name == "s_sendmsg" && is_gs => {
                    let msg_id = imm & 0xF;
                    let op = (imm >> 4) & 3;
                    let stream = ((imm >> 8) & 3) as u8;
                    match (msg_id, op) {
                        (2, 1) => io.gs_ops.push((site, GsOp::Cut(stream))),
                        (2, 2) => io.gs_ops.push((site, GsOp::Emit(stream))),
                        (2, 3) => io.gs_ops.push((site, GsOp::EmitCut(stream))),
                        (3, _) => io.gs_ops.push((site, GsOp::Done)),
                        _ => return Err(unsupported(at, "a geometry stream message")),
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// The pixel input VGPRs that `addr` enables, with their meaning.
fn ps_vgprs(addr: u32) -> Vec<(u8, Input)> {
    let mut out = Vec::new();
    let mut next = 0u8;
    for (bit, &(count, kind)) in PS_VGPRS.iter().enumerate() {
        if addr >> bit & 1 == 0 {
            continue;
        }
        for index in 0..count {
            let input = match kind {
                Some(kind) => Input::Barycentric { kind, index },
                None => ps_system(bit),
            };
            out.push((next + index, input));
        }
        next += count;
    }
    out
}

/// An interpolation before its attribute is known: `barycentric` none
/// when i or j is computed.
struct Pair {
    barycentric: Option<Barycentric>,
    first: Vec<Site>,
}

impl Pair {
    fn with(self, attribute: Attribute) -> Interp {
        match self.barycentric {
            Some(barycentric) => Interp::Interpolated {
                attribute,
                barycentric,
                first: self.first,
            },
            None => Interp::Weighted { attribute },
        }
    }
}

/// Records interpolation `pair` at `site`, and its first halves too when
/// they evaluate on their own (computed barycentrics).
fn push_pair(io: &mut Io, site: Site, pair: Pair, attribute: Attribute) {
    if pair.barycentric.is_none() {
        for &f in &pair.first {
            if !io.interps.iter().any(|(s, _)| *s == f) {
                io.interps.push((f, Interp::Weighted { attribute }));
            }
        }
    }
    io.interps.push((site, pair.with(attribute)));
}

/// Finds the first halves of interpolations.
struct Halves<'a> {
    p: &'a Program,
    defs: &'a std::collections::HashMap<Value, Site>,
    phis: &'a std::collections::HashMap<Value, (usize, usize)>,
}

impl Halves<'_> {
    /// The interpolation whose second half reads the first half's result
    /// `acc`, the barycentric `j`, and attribute `attr`: every first half
    /// must read the same attribute channel. With j and every first half's
    /// i loaded as one kind's, the pair interpolates with that kind;
    /// otherwise each half evaluates on its own (`Interp::Weighted`).
    fn pair(
        &self,
        attr: &Operand,
        acc: Option<Value>,
        j: Option<Value>,
        barycentric: &impl Fn(Option<Value>) -> Option<(Barycentric, u8)>,
    ) -> Result<Pair, &'static str> {
        let mut first = Vec::new();
        self.sites(
            acc.ok_or("an interpolation without its first half")?,
            &mut Vec::new(),
            &mut first,
        )?;
        let mut kind = match barycentric(j) {
            Some((kind, 1)) => Some(kind),
            _ => None,
        };
        for &(b, i) in &first {
            let inst = &self.p.blocks[b].insts[i];
            let (a, i) = match &inst.fields {
                Fields::Vintrp {
                    src: Operand::Vector { reg, .. },
                    attr,
                    ..
                } if inst.name() == "v_interp_p1_f32" => (*attr, inst.read(Reg::V(*reg))),
                Fields::Valu(v) if inst.name() == "v_interp_p1ll_f16" => match v.src.get(1) {
                    Some(&Operand::Vector { reg, .. }) => (v.src[0], inst.read(Reg::V(reg))),
                    _ => return Err("an interpolation without its first half"),
                },
                _ => return Err("an interpolation without its first half"),
            };
            if a != *attr {
                return Err("interpolation halves of different attributes");
            }
            if kind.is_some_and(|k| barycentric(i) != Some((k, 0))) {
                kind = None;
            }
        }
        Ok(Pair {
            barycentric: kind,
            first,
        })
    }

    /// The instructions `v` comes from, through phis (`seen`: phis on the
    /// way; a loop back to one adds nothing).
    fn sites(
        &self,
        v: Value,
        seen: &mut Vec<Value>,
        out: &mut Vec<Site>,
    ) -> Result<(), &'static str> {
        if let Some(&site) = self.defs.get(&v) {
            if !out.contains(&site) {
                out.push(site);
            }
            return Ok(());
        }
        let &(b, i) = self
            .phis
            .get(&v)
            .ok_or("an interpolation without its first half")?;
        if !seen.contains(&v) {
            seen.push(v);
            for &a in &self.p.blocks[b].phis[i].args {
                self.sites(a, seen, out)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::ExportPatch;

    const S_ENDPGM: u32 = 0xBF81_0000;

    fn program(words: &[u32]) -> Program {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        crate::ir::build(&code, false, 256).unwrap()
    }

    fn pixel(inputs: Vec<PsInput>, input_addr: u32) -> NativeProgram {
        NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 256,
            layout: vec![],
            used: vec![],
            io: StageIo::Ps {
                inputs,
                input_addr,
                db_shader_control: 0,
                patches: Vec::<ExportPatch>::new(),
            },
            code: vec![],
        }
    }

    const TEXCOORD: PsInput = PsInput {
        usage: Usage::Generic,
        row: 1,
        mask: 3,
        attr: 0,
        flat: false,
        fp16: false,
        pack2: false,
        row2: 1,
    };

    #[test]
    fn interpolation_pairs_and_color_exports() {
        // Blit's pixel shader: TEXCOORD.xy from the perspective-center i/j
        // in v0/v1, then exported unconverted to MRT0.
        let p = program(&[
            0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
            0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
            0xF800_180F,
            0x0302_0102, // exp mrt0, v2, v1, v2, v3 done vm
            S_ENDPGM,
        ]);
        let io = raise(&p, &pixel(vec![TEXCOORD], 2)).unwrap();
        let input = |reg| p.inputs.iter().find(|i| i.0 == Reg::V(reg)).unwrap().1;
        let center = |index| Input::Barycentric {
            kind: Barycentric::PerspectiveCenter,
            index,
        };
        assert!(io.inputs.contains(&(input(0), center(0))));
        assert!(io.inputs.contains(&(input(1), center(1))));
        // v2 and v3 are read before written but hold nothing.
        assert!(!io.inputs.iter().any(|i| i.0 == input(3)));
        assert_eq!(
            io.interps,
            [(
                (0, 1),
                Interp::Interpolated {
                    attribute: Attribute {
                        usage: Usage::Generic,
                        row: 1,
                        channel: 0,
                        high: false
                    },
                    barycentric: Barycentric::PerspectiveCenter,
                    first: vec![(0, 0)],
                }
            )]
        );
        let [e] = &io.exports[..] else {
            panic!("{:?}", io.exports)
        };
        assert_eq!(
            (e.target, e.done, e.compressed),
            (Target::Color(0), true, false)
        );
        assert_eq!(e.values[0], p.blocks[0].insts[1].write(Reg::V(2)));
    }

    #[test]
    fn unpaired_or_unknown_interpolation_fails() {
        // p2 alone: its old dst is no p1's result.
        let lone = program(&[0xC809_0001, S_ENDPGM]); // v_interp_p2_f32 v2, v1, attr0.x
        assert!(raise(&lone, &pixel(vec![TEXCOORD], 2)).is_err());
        // attr1 is not in the stage data.
        let other = program(&[0xC808_0400, S_ENDPGM]); // v_interp_p1_f32 v2, v0, attr1.x
        assert!(raise(&other, &pixel(vec![TEXCOORD], 2)).is_err());
    }

    #[test]
    fn computed_barycentrics_evaluate_each_half() {
        // i scaled before the first half: both halves stand on their own.
        let p = program(&[
            0x1006_00F0, // v_mul_f32 v3, 0.5, v0
            0xC808_0003, // v_interp_p1_f32 v2, v3, attr0.x
            0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
            S_ENDPGM,
        ]);
        let io = raise(&p, &pixel(vec![TEXCOORD], 2)).unwrap();
        let attribute = Attribute {
            usage: Usage::Generic,
            row: 1,
            channel: 0,
            high: false,
        };
        let mut interps = io.interps.clone();
        interps.sort_by_key(|i| i.0);
        assert_eq!(
            interps,
            [
                ((0, 1), Interp::Weighted { attribute }),
                ((0, 2), Interp::Weighted { attribute }),
            ]
        );
    }

    #[test]
    fn first_halves_read_through_a_phi_keep_their_partial_values() {
        let p = program(&[
            0xBF06_8000, // s_cmp_eq_u32 s0, 0
            0xBF85_0002, // s_cbranch_scc1 -> second first half
            0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
            0xBF82_0001, // s_branch -> export
            0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
            0xF800_180F,
            0x0202_0202, // exp mrt0, v2, v2, v2, v2 done vm
            S_ENDPGM,
        ]);
        let io = raise(&p, &pixel(vec![TEXCOORD], 2)).unwrap();
        assert_eq!(io.interps.len(), 2);
        assert!(
            io.interps
                .iter()
                .all(|(_, i)| matches!(i, Interp::Weighted { .. }))
        );
    }

    #[test]
    fn a_standalone_f16_first_half_is_weighted() {
        let p = program(&[
            0xD742_0002,
            0x0002_0000, // v_interp_p1ll_f16 v2, v0, attr0.x
            0xF800_180F,
            0x0202_0202,
            S_ENDPGM,
        ]);
        let io = raise(
            &p,
            &pixel(
                vec![PsInput {
                    fp16: true,
                    ..TEXCOORD
                }],
                2,
            ),
        )
        .unwrap();
        assert!(matches!(&io.interps[..], [(_, Interp::Weighted { .. })]));
    }

    #[test]
    fn mesh_inputs_and_primitive_export() {
        let mesh = |words: &[u32]| {
            let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            let p = crate::ir::build(&code, true, 256).unwrap();
            let native = NativeProgram {
                stage: HwStage::Ms,
                wave64: true,
                vgprs: 256,
                layout: vec![],
                used: vec![],
                io: StageIo::Ms(crate::native::MeshIo {
                    outputs: vec![],
                    clip_cntl: 0,
                    threads: 128,
                    max_vertices: 128,
                    max_primitives: 128,
                    topology: 2,
                    lds_bytes: 0,
                }),
                code,
            };
            raise(&p, &native)
        };
        let io = mesh(&[
            0x7E04_0505, // v_readfirstlane_b32 s2, v5
            0xF800_0941,
            0x0000_0009, // exp prim, v9, off, off, off done
            S_ENDPGM,
        ])
        .unwrap();
        assert!(io.inputs.iter().any(|i| i.1 == Input::MeshGroup));
        assert_eq!(io.exports[0].target, Target::Primitive);
        // The group input is uniform only as the first lane reads it.
        assert!(mesh(&[0x7E02_0305, S_ENDPGM]).is_err()); // v_mov_b32 v1, v5
        // Also as an operand of a write to v5, or after a join.
        let per_lane: [&[u32]; 5] = [
            &[0x4A0A_0305, S_ENDPGM],              // v_add_nc_u32 v5, v5, v1
            &[0x7E0A_0D05, S_ENDPGM],              // v_cvt_f32_u32 v5, v5
            &[0xDC30_8000, 0x0500_0005, S_ENDPGM], // global_load_dword v5, v5, s[0:1]
            &[0x7E0A_CB01, S_ENDPGM],              // v_swap_b32 v5, v1
            &[
                0xBF06_8001, // s_cmp_eq_u32 s1, 0
                0xBF85_0001, // s_cbranch_scc1 -> 0xc
                0x7E0A_0280, // v_mov_b32 v5, 0
                0x7E02_0305, // 0xc: v_mov_b32 v1, v5
                S_ENDPGM,
            ],
        ];
        let accepted: Vec<_> = per_lane.iter().filter(|w| mesh(w).is_ok()).collect();
        assert!(accepted.is_empty(), "accepted: {accepted:08X?}");
        // Overwriting v5 reads the group input only for lanes outside EXEC:
        // v_mov_b32 v5, 0; v_mov_b32 v1, v5.
        assert!(mesh(&[0x7E0A_0280, 0x7E02_0305, S_ENDPGM]).is_ok());
        // Skipping that write with s_cbranch_execz is the same case: under the
        // same mask, the read sees only the 0.
        let reuse = [
            0x7E04_0505, // v_readfirstlane_b32 s2, v5
            0x7D88_0084, // v_cmp_gt_u32 vcc, 4, v0
            0xBE86_246A, // s_and_saveexec_b64 s[6:7], vcc
            0xBF88_0001, // s_cbranch_execz -> 0x14
            0x7E0A_0280, // v_mov_b32 v5, 0
            0x88FE_067E, // 0x14: s_or_b64 exec, exec, s[6:7]
            0xBE86_246A, // s_and_saveexec_b64 s[6:7], vcc
            0xBF88_0001, // s_cbranch_execz -> 0x24
            0x7E02_0305, // v_mov_b32 v1, v5
            0x88FE_067E, // 0x24: s_or_b64 exec, exec, s[6:7]
            S_ENDPGM,
        ];
        assert_eq!(mesh(&reuse).map(|_| ()), Ok(()));
    }

    #[test]
    fn gs_lds_inputs_outputs_and_stream_ops() {
        // Synthetic merged GS: unpack vertex 0/1/2 ES->GS LDS offsets from v0/v1,
        // read SV_Position.xy for vertices 0..2, write vertex 0's SV_Position.xy
        // into GSVS LDS (stride 48 bytes = 12 dwords = 3 verts * 4 dwords), emit, cut, done.
        let words: &[u32] = &[
            0x3406_0082, // v_lshlrev_b32 v3, 2, v0
            0x3402_0282, // v_lshlrev_b32 v1, 2, v1
            0xBE81_0382, // s_mov_b32 s1, 2
            0x3410_00F9,
            0x0586_0601, // v_lshlrev_b32 v8, s1, v0 src1_sel:WORD_1
            0x3614_06FF,
            0x0003_FFFC, // v_and_b32 v10, 0x3fffc, v3   (vertex 0)
            0x360C_02FF,
            0x0003_FFFC, // v_and_b32 v6, 0x3fffc, v1    (vertex 2)
            0xD8DC_0100,
            0x0000_000A, // ds_read2_b32 v[0:1], v10 offset1:1  (vtx 0 pos.xy)
            0xD8DC_0100,
            0x0200_0008, // ds_read2_b32 v[2:3], v8 offset1:1   (vtx 1 pos.xy)
            0xD8DC_0100,
            0x0400_0006, // ds_read2_b32 v[4:5], v6 offset1:1   (vtx 2 pos.xy)
            0xD543_000C,
            0x0002_04FF,
            0x0000_0030, // v_mad_u32_u24 v12, 48, v2, s0
            0xD838_0100,
            0x0001_000C, // ds_write2_b32 v12, v0, v1 offset1:1
            0xBF90_0022, // s_sendmsg sendmsg(MSG_GS, GS_OP_EMIT, 0)
            0xBF90_0012, // s_sendmsg sendmsg(MSG_GS, GS_OP_CUT, 0)
            0xBF90_0003, // s_sendmsg sendmsg(MSG_GS_DONE, GS_OP_NOP)
            S_ENDPGM,
        ];
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = crate::ir::build(&code, true, 16).unwrap();
        let native = NativeProgram {
            stage: HwStage::Gs,
            wave64: true,
            vgprs: 16,
            layout: vec![],
            used: vec![],
            io: StageIo::Gs(crate::native::GsIo {
                outputs: vec![],
                ring_dwords: 12,
                invocations: 1,
                max_vertices: 3,
                vertex_dwords: 4,
            }),
            code,
        };
        let io = raise(&p, &native).unwrap();
        assert_eq!(io.gs_reads.len(), 3);
        assert_eq!(
            io.gs_reads[0].1,
            vec![
                (
                    0,
                    GsInput {
                        vertex: 0,
                        row: 0,
                        channel: 0
                    }
                ),
                (
                    1,
                    GsInput {
                        vertex: 0,
                        row: 0,
                        channel: 1
                    }
                ),
            ]
        );
        assert_eq!(io.gs_reads[1].1[0].1.vertex, 1);
        assert_eq!(io.gs_reads[2].1[0].1.vertex, 2);
        assert_eq!(io.lds_outputs.len(), 1);
        assert_eq!(io.lds_outputs[0].1.len(), 2);
        assert_eq!(
            (
                io.lds_outputs[0].1[0].vertex,
                io.lds_outputs[0].1[0].row,
                io.lds_outputs[0].1[0].channel
            ),
            (0, 0, 0)
        );
        assert_eq!(
            (
                io.lds_outputs[0].1[1].vertex,
                io.lds_outputs[0].1[1].row,
                io.lds_outputs[0].1[1].channel
            ),
            (0, 0, 1)
        );
        let ops: Vec<GsOp> = io.gs_ops.iter().map(|(_, op)| *op).collect();
        assert_eq!(ops, vec![GsOp::Emit(0), GsOp::Cut(0), GsOp::Done]);
    }

    #[test]
    fn pixel_vgprs_follow_the_input_address() {
        // Linear center (bit 5), then position x and y (bits 8, 9).
        let v = ps_vgprs(1 << 5 | 1 << 8 | 1 << 9);
        assert_eq!(
            v,
            [
                (
                    0,
                    Input::Barycentric {
                        kind: Barycentric::LinearCenter,
                        index: 0
                    }
                ),
                (
                    1,
                    Input::Barycentric {
                        kind: Barycentric::LinearCenter,
                        index: 1
                    }
                ),
                (2, Input::Position(0)),
                (3, Input::Position(1)),
            ]
        );
    }
}
