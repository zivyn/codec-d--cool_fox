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
//! and primitive n (`exp prim`: 10-bit vertex indices at bits 0, 10, 20,
//! bit 31 culls); `param` entries with bit 7 are per-primitive outputs. One
//! lane sends the output counts (`s_sendmsg` GS_ALLOC_REQ, m0 = vertices |
//! primitives << 12).

use std::rc::Rc;

use crate::ShaderError;
use crate::gfx10::{Fields, Operand};
use crate::ir::{EXEC, Inst, Program, Reg, Value};
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Io {
    /// Input values the hardware loads, with their meaning; other VGPRs
    /// read before written hold undefined values.
    pub inputs: Vec<(Value, Input)>,
    pub interps: Vec<(Site, Interp)>,
    pub exports: Vec<Export>,
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
            let group = p.inputs.iter().find(|i| i.0 == Reg::V(5)).map(|i| i.1);
            // The group input holds the same value in every lane only as the
            // first lane reads it; a VGPR write reads its old value too.
            let per_lane = p.blocks.iter().flat_map(|b| &b.insts).find(|i| {
                group.is_some_and(|g| i.reads.contains(&(Reg::V(5), g)))
                    && i.name() != "v_readfirstlane_b32"
                    && i.write(Reg::V(5)).is_none()
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
                        ("v_interp_p1_f32", _) => {}
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
                                    inst,
                                    &barycentric,
                                )
                                .map_err(|what| unsupported(at, what))?;
                            io.interps.push((site, interp.with(attribute)));
                        }
                        _ => return Err(unsupported(at, "an interpolation form")),
                    }
                }
                Fields::Valu(v) if name.starts_with("v_interp_") => match name {
                    "v_interp_p1ll_f16" => {}
                    "v_interp_p2_f16" => {
                        let attribute = attribute(inst, &v.src[0])?;
                        let read = |k: usize| match v.src.get(k) {
                            Some(&Operand::Vector { reg, .. }) => inst.read(Reg::V(reg)),
                            _ => None,
                        };
                        let interp = halves
                            .pair(&v.src[0], read(2), read(1), inst, &barycentric)
                            .map_err(|what| unsupported(at, what))?;
                        io.interps.push((site, interp.with(attribute)));
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
    Ok(io)
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

/// Mask definitions one [`Walk`] looks into before it gives up.
const MASK_STEPS: u32 = 4096;

/// How a lane mask (one dword of it) is made from others.
enum Mask<'a> {
    /// The lanes in both.
    And(Value, Value),
    /// The lanes of the first that are not in the second.
    AndNot(Value, Value),
    /// The same lanes as another mask.
    Copy(Value),
    /// Some of the lanes of another mask: EXEC narrowed by a compare.
    Within(Value),
    /// One of these (one per predecessor), by the path taken to the block.
    Phi(usize, &'a [Value]),
    Unknown,
}

/// What is known of a lane mask: masks it is within, and masks that share
/// no lane with it.
struct Facts {
    within: Vec<Value>,
    apart: Vec<Value>,
}

/// Reasoning about the lane masks of two instructions, the first of which
/// is in block `from`: whatever it does reaches the second along a path
/// that leaves `from`, so a phi in a block ahead of it is one of the
/// arguments from the blocks ahead too.
struct Walk<'a> {
    halves: &'a Halves<'a>,
    from: usize,
    /// The blocks a path from `from` enters.
    ahead: Vec<bool>,
    steps: u32,
    known: std::collections::HashMap<Value, Rc<Facts>>,
}

impl<'a> Walk<'a> {
    fn new(halves: &'a Halves<'a>, from: usize) -> Self {
        let blocks = &halves.p.blocks;
        let mut ahead = vec![false; blocks.len()];
        let mut work = blocks[from].exit.successors();
        while let Some(b) = work.pop() {
            if !std::mem::replace(&mut ahead[b], true) {
                work.extend(blocks[b].exit.successors());
            }
        }
        Self {
            halves,
            from,
            ahead,
            steps: MASK_STEPS,
            known: std::collections::HashMap::new(),
        }
    }

    /// The arguments of the phi of block `b` that a path from `from` can
    /// bring.
    fn arms<'s>(&'s self, b: usize, args: &'s [Value]) -> impl Iterator<Item = Value> + 's {
        let preds = &self.halves.p.blocks[b].preds;
        args.iter()
            .zip(preds)
            .filter(move |&(_, &p)| !self.ahead[b] || self.ahead[p] || p == self.from)
            .map(|(&a, _)| a)
    }

    /// The mask `v` is a copy of, through copies.
    fn root(&self, mut v: Value) -> Value {
        while let Mask::Copy(x) = self.halves.mask(v) {
            v = x;
        }
        v
    }

    /// What the way lane mask `v` is made says of it.
    fn facts(&mut self, v: Value) -> Rc<Facts> {
        let v = self.root(v);
        if let Some(f) = self.known.get(&v) {
            return Rc::clone(f);
        }
        let mut f = Facts {
            within: vec![v],
            apart: vec![],
        };
        if self.steps > 0 {
            self.steps -= 1;
            let halves = self.halves;
            match halves.mask(v) {
                Mask::And(x, y) => {
                    let (x, y) = (self.facts(x), self.facts(y));
                    f.within.extend(x.within.iter().chain(&y.within));
                    f.apart.extend(x.apart.iter().chain(&y.apart));
                }
                Mask::AndNot(x, y) => {
                    let x = self.facts(x);
                    f.within.extend(&x.within);
                    f.apart.extend(&x.apart);
                    f.apart.push(self.root(y));
                }
                Mask::Within(x) | Mask::Copy(x) => {
                    let x = self.facts(x);
                    f.within.extend(&x.within);
                    f.apart.extend(&x.apart);
                }
                Mask::Phi(b, args) => {
                    // What holds whichever way the phi was entered.
                    let args: Vec<Value> = self.arms(b, args).collect();
                    let mut args = args.into_iter().map(|x| self.facts(x));
                    if let Some(first) = args.next() {
                        let (mut within, mut apart) = (first.within.clone(), first.apart.clone());
                        for x in args {
                            within.retain(|m| x.within.contains(m));
                            apart.retain(|m| x.apart.contains(m));
                        }
                        f.within.extend(within);
                        f.apart = apart;
                    }
                }
                Mask::Unknown => {}
            }
        }
        let f = Rc::new(f);
        self.known.insert(v, Rc::clone(&f));
        f
    }

    /// Whether `write` runs in every lane `at` runs in (`covers`), or in
    /// none of them.
    fn lanes(&mut self, write: &Inst, at: &Inst, covers: bool) -> bool {
        (0..1 + u8::from(self.halves.p.wave64)).all(|k| {
            match (write.read(Reg::S(EXEC + k)), at.read(Reg::S(EXEC + k))) {
                (Some(w), Some(a)) if covers => {
                    let a = self.facts(a);
                    self.within(&a.within, w, 4)
                }
                (Some(w), Some(a)) => self.disjoint(w, a),
                _ => false,
            }
        })
    }

    /// Whether lane masks `a` and `b` share no lane: one is within a mask
    /// (or a phi of masks) that the other has no lane in.
    fn disjoint(&mut self, a: Value, b: Value) -> bool {
        let (a, b) = (self.facts(a), self.facts(b));
        let held =
            |within: &[Value], apart: &[Value]| apart.iter().any(|&m| self.within(within, m, 4));
        held(&b.within, &a.apart) || held(&a.within, &b.apart)
    }

    /// Whether a lane mask known to be within all of `within` is within
    /// `m`: one of them, or a phi of such.
    fn within(&self, within: &[Value], m: Value, depth: u8) -> bool {
        let m = self.root(m);
        within.contains(&m)
            || depth > 0
                && matches!(self.halves.mask(m), Mask::Phi(b, args)
                    if self.arms(b, args).all(|x| self.within(within, x, depth - 1)))
    }
}

/// An interpolation before its attribute is known.
struct Pair {
    barycentric: Barycentric,
    first: Vec<Site>,
}

impl Pair {
    fn with(self, attribute: Attribute) -> Interp {
        Interp::Interpolated {
            attribute,
            barycentric: self.barycentric,
            first: self.first,
        }
    }
}

/// Finds the first halves of interpolations.
struct Halves<'a> {
    p: &'a Program,
    defs: &'a std::collections::HashMap<Value, Site>,
    phis: &'a std::collections::HashMap<Value, (usize, usize)>,
}

impl Halves<'_> {
    /// The interpolation whose second half `at` reads the first half's
    /// result `acc`, the barycentric `j`, and attribute `attr`: every first
    /// half must read the same attribute channel and the same kind's i.
    fn pair(
        &self,
        attr: &Operand,
        acc: Option<Value>,
        j: Option<Value>,
        at: &Inst,
        barycentric: &impl Fn(Option<Value>) -> Option<(Barycentric, u8)>,
    ) -> Result<Pair, &'static str> {
        let mut first = Vec::new();
        self.sites(
            acc.ok_or("an interpolation without its first half")?,
            &mut Vec::new(),
            &mut first,
        )?;
        let Some((kind, 1)) = self.lanes_barycentric(j, at, barycentric) else {
            return Err("an interpolation with computed barycentrics");
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
            if self.lanes_barycentric(i, inst, barycentric) != Some((kind, 0)) {
                return Err("an interpolation with computed barycentrics");
            }
        }
        Ok(Pair {
            barycentric: kind,
            first,
        })
    }

    /// The barycentric kind and index `v` holds in every lane `at` runs in:
    /// as loaded, or after vector writes that run in none of those lanes.
    /// (The compiler gives the lanes a discard kills other values in i and
    /// j, then runs the rest of the program in the lanes that survive; the
    /// interpolation reads i and j through a phi of the two.)
    fn lanes_barycentric(
        &self,
        v: Option<Value>,
        at: &Inst,
        loaded: &impl Fn(Option<Value>) -> Option<(Barycentric, u8)>,
    ) -> Option<(Barycentric, u8)> {
        let v = v?;
        if let Some(b) = loaded(Some(v)) {
            return Some(b);
        }
        let mut found = None;
        let ok = self.same_lanes(v, at, loaded, &mut Vec::new(), &mut found);
        found.filter(|_| ok)
    }

    /// Whether `v` is one barycentric (`found`) in the lanes `at` runs in.
    fn same_lanes(
        &self,
        v: Value,
        at: &Inst,
        loaded: &impl Fn(Option<Value>) -> Option<(Barycentric, u8)>,
        seen: &mut Vec<Value>,
        found: &mut Option<(Barycentric, u8)>,
    ) -> bool {
        if let Some(b) = loaded(Some(v)) {
            return *found.get_or_insert(b) == b;
        }
        if seen.contains(&v) {
            return true;
        }
        seen.push(v);
        if let Some(&(b, i)) = self.phis.get(&v) {
            return !self.loops_back(b)
                && self.p.blocks[b].phis[i]
                    .args
                    .iter()
                    .all(|&a| self.same_lanes(a, at, loaded, seen, found));
        }
        // A vector write keeps the old value in the lanes outside EXEC (but
        // a lane write goes to its lane whatever EXEC says).
        let Some(&(b, i)) = self.defs.get(&v) else {
            return false;
        };
        let write = &self.p.blocks[b].insts[i];
        let Some(&(reg @ Reg::V(_), _)) = write.writes.iter().find(|w| w.1 == v) else {
            return false;
        };
        let Some(old) = write
            .read(reg)
            .filter(|_| write.name() != "v_writelane_b32")
        else {
            return false;
        };
        let mut walk = Walk::new(self, b);
        // A copy of another register in all of the lanes `at` runs in.
        let source = match &write.fields {
            Fields::Valu(v)
                if write.name() == "v_mov_b32"
                    && (v.neg, v.abs, v.omod, v.opsel) == (0, 0, 0, 0)
                    && !v.clamp
                    && v.sdwa.is_none()
                    && v.dpp.is_none() =>
            {
                match v.src.first() {
                    Some(&Operand::Vector { reg, .. }) => write.read(Reg::V(reg)),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(source) = source.filter(|_| walk.lanes(write, at, true)) {
            return self.same_lanes(source, at, loaded, seen, found);
        }
        walk.lanes(write, at, false) && self.same_lanes(old, at, loaded, seen, found)
    }

    /// Whether block `b` is entered from a later one: its phis merge values
    /// of different iterations, which no mask below is about.
    fn loops_back(&self, b: usize) -> bool {
        self.p.blocks[b].preds.iter().any(|&p| p >= b)
    }

    /// How the lane mask `v` (one dword) is made from others.
    fn mask(&self, v: Value) -> Mask<'_> {
        if let Some(&(b, i)) = self.phis.get(&v) {
            return if self.loops_back(b) {
                Mask::Unknown
            } else {
                Mask::Phi(b, &self.p.blocks[b].phis[i].args)
            };
        }
        let Some(&(b, i)) = self.defs.get(&v) else {
            return Mask::Unknown;
        };
        let inst = &self.p.blocks[b].insts[i];
        let Some(&(reg @ Reg::S(r), _)) = inst.writes.iter().find(|w| w.1 == v) else {
            return Mask::Unknown;
        };
        let name = inst.name();
        if name.starts_with("v_cmpx") {
            // EXEC where it was and the compare holds.
            return inst.read(reg).map_or(Mask::Unknown, Mask::Within);
        }
        let Fields::Sop {
            dst: Some(Operand::Scalar { code, .. }),
            src,
            ..
        } = &inst.fields
        else {
            return Mask::Unknown;
        };
        let read = |op: Option<&Operand>, k: u8| match op {
            Some(&Operand::Scalar { code, .. }) => inst.read(Reg::S(code + k)),
            _ => None,
        };
        if name.contains("saveexec") {
            // The destination gets EXEC; EXEC gets EXEC and the source.
            if r < EXEC {
                let old = inst.read(Reg::S(EXEC + r.wrapping_sub(*code)));
                return old.map_or(Mask::Unknown, Mask::Copy);
            }
            return match (inst.read(reg), read(src.first(), r - EXEC)) {
                (Some(old), Some(s)) => Mask::And(old, s),
                _ => Mask::Unknown,
            };
        }
        let k = r.wrapping_sub(*code);
        match (name, read(src.first(), k), read(src.get(1), k)) {
            ("s_and_b32" | "s_and_b64", Some(a), Some(b)) => Mask::And(a, b),
            ("s_andn2_b32" | "s_andn2_b64", Some(a), Some(b)) => Mask::AndNot(a, b),
            ("s_mov_b32" | "s_mov_b64", Some(a), _) => Mask::Copy(a),
            _ => Mask::Unknown,
        }
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

    /// A pixel program that kills the lanes where a bit test fails and
    /// interpolates in the others. The lanes it kills get 1.0 in i and j
    /// first (as the compiler does, to keep them harmless). With `restore`
    /// the interpolation runs in all lanes again, those included.
    fn kills_then_interpolates(wave64: bool, restore: bool) -> Program {
        let w = |lo: u32, hi: u32| if wave64 { hi } else { lo };
        let code: Vec<u32> = if restore {
            vec![
                w(0xBE84_037E, 0xBE84_047E), // s_mov_b32 s4, exec_lo (b64: s[4:5], exec)
                0x7DA4_0A80,                 // v_cmpx_eq_u32 exec, 0, v5
                0xBF88_0002,                 // s_cbranch_execz to the join
                0x7E00_02F2,                 // v_mov_b32 v0, 1.0
                0x7E02_02F2,                 // v_mov_b32 v1, 1.0
                w(0xBEFE_0304, 0xBEFE_0404), // join: s_mov_b32 exec_lo, s4 (b64: exec, s[4:5])
            ]
        } else {
            vec![
                w(0xBE82_037E, 0xBE82_047E), // s_mov_b32 s2, exec_lo: the lanes still alive
                w(0xBE84_037E, 0xBE84_047E), // s_mov_b32 s4, exec_lo: all of them
                0x7DA4_0A80,                 // v_cmpx_eq_u32 exec, 0, v5: the lanes to kill
                0xBF88_0005,                 // s_cbranch_execz to the join
                w(0x8A02_7E02, 0x8A82_7E02), // s_andn2_b32 s2, s2, exec_lo
                0xBF84_0009,                 // s_cbranch_scc0 to the end: all killed
                w(0x877E_027E, 0x87FE_027E), // s_and_b32 exec_lo, exec_lo, s2
                0x7E00_02F2,                 // v_mov_b32 v0, 1.0
                0x7E02_02F2,                 // v_mov_b32 v1, 1.0
                // join: the lanes that were not in the kill's branch
                w(0x8A7E_7E04, 0x8AFE_7E04), // s_andn2_b32 exec_lo, s4, exec_lo
                w(0x877E_027E, 0x87FE_027E), // s_and_b32 exec_lo, exec_lo, s2
            ]
        };
        let mut words = code;
        words.extend([
            0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
            0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
            0xF800_180F,
            0x0302_0102, // exp mrt0, v2, v1, v2, v3 done vm
            S_ENDPGM,
        ]);
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        crate::ir::build(&code, wave64, 256).unwrap()
    }

    #[test]
    fn killed_lanes_may_hold_other_barycentrics() {
        for wave64 in [false, true] {
            // v0 and v1 are phis of the loaded values and 1.0, but 1.0 is
            // only in lanes the interpolation does not run in.
            let p = kills_then_interpolates(wave64, false);
            let io = raise(&p, &pixel(vec![TEXCOORD], 2)).unwrap();
            let [(site, interp)] = &io.interps[..] else {
                panic!("{:?}", io.interps)
            };
            assert_eq!(*site, (3, 3));
            let Interp::Interpolated {
                barycentric, first, ..
            } = interp
            else {
                panic!("{interp:?}")
            };
            assert_eq!(*barycentric, Barycentric::PerspectiveCenter);
            assert_eq!(first, &[(3, 2)]);

            // Restoring the whole mask runs it in the lanes that got 1.0
            // too: those barycentrics are computed.
            let p = kills_then_interpolates(wave64, true);
            assert_eq!(
                raise(&p, &pixel(vec![TEXCOORD], 2)),
                Err(ShaderError::Unsupported {
                    offset: 28,
                    what: "an interpolation with computed barycentrics",
                })
            );
        }
    }

    #[test]
    fn interpolation_may_read_a_copy_of_j() {
        let p = program(&[
            0x7E0C_0301, // v_mov_b32 v6, v1
            0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
            0xC809_0006, // v_interp_p2_f32 v2, v6, attr0.x
            S_ENDPGM,
        ]);
        let io = raise(&p, &pixel(vec![TEXCOORD], 2)).unwrap();
        let [((0, 2), Interp::Interpolated { first, .. })] = &io.interps[..] else {
            panic!("{:?}", io.interps)
        };
        assert_eq!(first, &[(0, 1)]);
        // A copy that ran in fewer lanes than the interpolation does.
        let p = program(&[
            0xBE84_037E, // s_mov_b32 s4, exec_lo
            0x7DA4_0A80, // v_cmpx_eq_u32 exec, 0, v5
            0x7E0C_0301, // v_mov_b32 v6, v1
            0xBEFE_0304, // s_mov_b32 exec_lo, s4
            0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
            0xC809_0006, // v_interp_p2_f32 v2, v6, attr0.x
            S_ENDPGM,
        ]);
        assert_eq!(
            raise(&p, &pixel(vec![TEXCOORD], 2)),
            Err(ShaderError::Unsupported {
                offset: 20,
                what: "an interpolation with computed barycentrics",
            })
        );
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
