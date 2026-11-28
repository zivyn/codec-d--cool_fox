//! What a program may write through its buffer views: a static "store
//! footprint" for the host's write sets.
//!
//! The host declares as written the whole view of every unordered-access
//! buffer a shader that stores is bound to. This module reads the program's
//! code instead: for each buffer store or atomic, the descriptor it goes
//! through ([`Target`]) and the byte offset it can reach, as an expression
//! ([`Expr`]) over thread ids, group ids and root constants. The host
//! evaluates the expressions at record time, with the dispatch's extents and
//! the root constants it has ([`Footprint::ranges`]); where nothing can be
//! said, the answer is `None`: anywhere in the view, as before.
//!
//! The analysis is a backward walk over the SSA form of [`crate::ir`], an
//! interval over 32-bit values at evaluation, and sound by construction:
//! anything not understood (a loop-carried value, a loaded value, an
//! operation not listed, a modifier) is [`Expr::Top`], and an interval that
//! could wrap past 2^32 is unknown too. It is never called by the
//! translation: the emitted shaders do not depend on it.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use crate::gfx10::{Encoding, Fields, NULL, Operand};
use crate::ir::{Exit, Inst, Program, Reg, Value};
use crate::native::{NativeProgram, ResourceType, RootType, StageIo, UsedResource};
use crate::resource::{Address, Resources, Source};

/// Nodes an expression may have before it is given up as unknown.
const MAX_NODES: usize = 48;
/// Values followed one after the other.
const MAX_DEPTH: u32 = 96;
/// Ranges [`Footprint::ranges`] returns at most.
const MAX_RANGES: usize = 4;

/// What a program stores to buffers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Footprint {
    pub stores: Vec<Store>,
    /// Some store could not be attributed to a descriptor (or the code has an
    /// indirect jump, a scalar-memory store): every view may be written
    /// anywhere.
    pub unbounded: bool,
    /// A flat or global store: title memory by address, never through views.
    pub by_address: bool,
    /// The loads (and the atomics, which are both): the same walk over what the program reads
    /// through buffer descriptors. `bytes` is the widest read.
    pub loads: Vec<Store>,
    /// Some load could not be attributed to a descriptor (an indirect jump, an unknown
    /// instruction): every view may be read anywhere.
    pub unbounded_loads: bool,
    /// A flat or global load (or a scalar load by an address no root view gives): memory by
    /// address, never through views.
    pub by_address_loads: bool,
    /// Textures (images) the program reads and writes, by the descriptor they are loaded
    /// through: the host says a texture is read or written wholly. Atomics are in both.
    pub image_reads: Vec<Target>,
    pub image_writes: Vec<Target>,
    /// An image access whose descriptor could not be attributed: any texture may be read and
    /// written.
    pub images_unknown: bool,
}

/// The descriptor a store goes through, as the host binds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// The used-resource entry of a descriptor table: its `user_data` and its
    /// `table_offset_dwords`.
    Table { user_data: u8, dword: u32 },
    /// A descriptor of that table the code picks by a computed index, or by
    /// control flow: any entry of it.
    TableAny { user_data: u8 },
    /// A root descriptor, by root parameter.
    Root { slot: u8 },
}

/// One store or atomic instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    pub target: Target,
    /// The byte offset, from the start of the view, of the first byte written,
    /// over all lanes.
    pub offset: Expr,
    /// The widest single access (bytes from `offset`).
    pub bytes: u32,
}

/// A 32-bit value as the program computes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Const(u32),
    /// The thread id in the group, component 0 to 2.
    Thread(u8),
    /// The thread group id, component 0 to 2.
    Group(u8),
    /// A dword of a root parameter that holds constants.
    Root { slot: u8, dword: u32 },
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    /// Shifts take the amount modulo 32, as the hardware does.
    Shl(Box<Expr>, Box<Expr>),
    Shr(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Min(Box<Expr>, Box<Expr>),
    Max(Box<Expr>, Box<Expr>),
    /// Bits `offset .. offset + width` of the value, from the bit `offset`.
    Bfe(Box<Expr>, Box<Expr>, Box<Expr>),
    /// One of the two (lanes that took different paths).
    Select(Box<Expr>, Box<Expr>),
    /// Not known.
    Top,
}

/// Why an expression is [`Expr::Top`] (the first reason met).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cause {
    /// A value loaded from memory.
    Loaded,
    /// A value that depends on itself through a loop.
    Cyclic,
    /// An operation, or a modifier of one, that is not followed.
    Op,
    /// A typed or formatted access.
    Typed,
    /// A register the hardware sets that is not a thread or group id or a
    /// root constant.
    Input,
    /// Too big an expression or too long a chain of values.
    Size,
}

/// What the host knows when it evaluates a footprint.
pub struct Bind<'a> {
    /// Threads per group (the program's own).
    pub threads: [u32; 3],
    /// Thread groups of the dispatch; `None` if not known (indirect).
    pub groups: Option<[u32; 3]>,
    /// A dword of root constants of root parameter `slot`, as the title set it.
    pub constants: &'a dyn Fn(u8, u32) -> Option<u32>,
}

type Iv = (u64, u64);
const MAX: u64 = u32::MAX as u64;

fn point(x: u32) -> Iv {
    (u64::from(x), u64::from(x))
}

/// A shift amount's interval, modulo 32.
fn amount((lo, hi): Iv) -> Iv {
    if hi <= 31 {
        (lo, hi)
    } else if lo == hi {
        (lo & 31, lo & 31)
    } else {
        (0, 31)
    }
}

fn mask(width: u64) -> u64 {
    if width >= 32 { MAX } else { (1 << width) - 1 }
}

impl Expr {
    /// Nodes in the expression.
    pub fn size(&self) -> usize {
        match self {
            Expr::Add(a, b)
            | Expr::Sub(a, b)
            | Expr::Mul(a, b)
            | Expr::Shl(a, b)
            | Expr::Shr(a, b)
            | Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::Min(a, b)
            | Expr::Max(a, b)
            | Expr::Select(a, b) => 1 + a.size() + b.size(),
            Expr::Bfe(a, b, c) => 1 + a.size() + b.size() + c.size(),
            _ => 1,
        }
    }

    /// Whether [`Expr::Top`] occurs in it.
    pub fn has_top(&self) -> bool {
        match self {
            Expr::Top => true,
            Expr::Add(a, b)
            | Expr::Sub(a, b)
            | Expr::Mul(a, b)
            | Expr::Shl(a, b)
            | Expr::Shr(a, b)
            | Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::Min(a, b)
            | Expr::Max(a, b)
            | Expr::Select(a, b) => a.has_top() || b.has_top(),
            Expr::Bfe(a, b, c) => a.has_top() || b.has_top() || c.has_top(),
            _ => false,
        }
    }

    /// The lowest and highest value it can have, or `None` if not known (or
    /// if some step could wrap past 2^32).
    pub fn eval(&self, bind: &Bind) -> Option<(u64, u64)> {
        let full = (0, MAX);
        match self {
            Expr::Const(x) => Some(point(*x)),
            Expr::Thread(c) => Some((0, u64::from(bind.threads.get(usize::from(*c))?.max(&1) - 1))),
            Expr::Group(c) => {
                let g = bind.groups?;
                Some((0, u64::from(g.get(usize::from(*c))?.max(&1) - 1)))
            }
            Expr::Root { slot, dword } => (bind.constants)(*slot, *dword).map(point),
            Expr::Add(a, b) => {
                let (x, y) = (a.eval(bind)?, b.eval(bind)?);
                let (lo, hi) = (x.0 + y.0, x.1 + y.1);
                (hi <= MAX).then_some((lo, hi))
            }
            Expr::Sub(a, b) => {
                let (x, y) = (a.eval(bind)?, b.eval(bind)?);
                (x.0 >= y.1).then(|| (x.0 - y.1, x.1 - y.0))
            }
            Expr::Mul(a, b) => {
                let (x, y) = (a.eval(bind)?, b.eval(bind)?);
                let hi = x.1.checked_mul(y.1)?;
                (hi <= MAX).then_some((x.0 * y.0, hi))
            }
            Expr::Shl(a, s) => {
                let (x, s) = (a.eval(bind)?, amount(s.eval(bind).unwrap_or((0, 31))));
                let hi = x.1 << s.1;
                (hi <= MAX).then_some((x.0 << s.0, hi))
            }
            Expr::Shr(a, s) => {
                let (x, s) = (a.eval(bind).unwrap_or(full), amount(s.eval(bind).unwrap_or((0, 31))));
                Some((x.0 >> s.1, x.1 >> s.0))
            }
            Expr::And(a, b) => match (a.eval(bind), b.eval(bind)) {
                (Some(x), Some(y)) if x.0 == x.1 && y.0 == y.1 => Some((x.0 & y.0, x.0 & y.0)),
                (Some(x), Some(y)) => Some((0, x.1.min(y.1))),
                (Some(x), None) | (None, Some(x)) => Some((0, x.1)),
                (None, None) => None,
            },
            Expr::Or(a, b) => {
                let (x, y) = (a.eval(bind)?, b.eval(bind)?);
                if x.0 == x.1 && y.0 == y.1 {
                    return Some((x.0 | y.0, x.0 | y.0));
                }
                let top = x.1.max(y.1);
                let hi = if top == 0 { 0 } else { (1u64 << (64 - top.leading_zeros())) - 1 };
                Some((x.0.max(y.0), hi.min(MAX)))
            }
            Expr::Min(a, b) => match (a.eval(bind), b.eval(bind)) {
                (Some(x), Some(y)) => Some((x.0.min(y.0), x.1.min(y.1))),
                (Some(x), None) | (None, Some(x)) => Some((0, x.1)),
                (None, None) => None,
            },
            Expr::Max(a, b) => {
                let (x, y) = (a.eval(bind)?, b.eval(bind)?);
                Some((x.0.max(y.0), x.1.max(y.1)))
            }
            Expr::Bfe(a, o, w) => {
                let x = a.eval(bind).unwrap_or(full);
                let o = amount(o.eval(bind).unwrap_or((0, 31)));
                let w = w.eval(bind).unwrap_or((0, 32));
                if x.0 == x.1 && o.0 == o.1 && w.0 == w.1 {
                    let v = (x.0 >> o.0) & mask(w.0);
                    return Some((v, v));
                }
                Some((0, mask(w.1).min(x.1 >> o.0)))
            }
            Expr::Select(a, b) => {
                let (x, y) = (a.eval(bind)?, b.eval(bind)?);
                Some((x.0.min(y.0), x.1.max(y.1)))
            }
            Expr::Top => None,
        }
    }
}

impl Footprint {
    /// The byte ranges `[lo, hi)`, from the start of the view, that the
    /// stores through `target` may write: sorted, joined, at most four.
    /// `None`: anywhere in the view (the program is unbounded, or a store
    /// through it has an offset that is not known). An empty list: nothing
    /// stores through it.
    pub fn ranges(&self, target: Target, bind: &Bind) -> Option<Vec<(u64, u64)>> {
        gather(&self.stores, self.unbounded, target, bind)
    }

    /// [`Footprint::ranges`] for what the program reads: the loads and
    /// atomics through the view `target` (an entry of any kind, or a root
    /// view of any kind), `None` for anywhere in it.
    pub fn load_ranges(&self, target: Target, bind: &Bind) -> Option<Vec<(u64, u64)>> {
        gather(&self.loads, self.unbounded_loads, target, bind)
    }
}

fn gather(list: &[Store], unbounded: bool, target: Target, bind: &Bind) -> Option<Vec<(u64, u64)>> {
    if unbounded {
        return None;
    }
    let mut out: Vec<(u64, u64)> = Vec::new();
    for store in list {
        let hit = store.target == target
            || match (store.target, target) {
                (Target::TableAny { user_data: a }, Target::Table { user_data: b, .. })
                | (Target::TableAny { user_data: a }, Target::TableAny { user_data: b }) => a == b,
                (Target::Table { user_data: a, .. }, Target::TableAny { user_data: b }) => a == b,
                _ => false,
            };
        if !hit {
            continue;
        }
        let (lo, hi) = store.offset.eval(bind)?;
        out.push((lo, hi + u64::from(store.bytes)));
    }
    out.sort_unstable();
    let mut joined: Vec<(u64, u64)> = Vec::new();
    for (lo, hi) in out {
        match joined.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => joined.push((lo, hi)),
        }
    }
    while joined.len() > MAX_RANGES {
        // The two nearest ranges become one.
        let at = (1..joined.len()).min_by_key(|&i| joined[i].0 - joined[i - 1].1).unwrap_or(1);
        let next = joined.remove(at);
        joined[at - 1].1 = next.1;
    }
    Some(joined)
}

/// The program's footprint, or `None` if its front half does not run (the
/// translation would fail the same way). Never panics.
pub fn analyze(native: &NativeProgram) -> Option<Footprint> {
    analyze_causes(native).map(|a| a.0)
}

/// [`analyze`], with the cause of the first [`Expr::Top`] of each store (or
/// `None` where there is none), in the order of [`Footprint::stores`].
pub fn analyze_causes(native: &NativeProgram) -> Option<(Footprint, Vec<Option<Cause>>)> {
    analyze_all(native).map(|(f, c)| (f, c.stores))
}

/// The causes of the [`Expr::Top`] of each store and each load (`None` where there is none), in the
/// order of [`Footprint::stores`] and [`Footprint::loads`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Causes {
    pub stores: Vec<Option<Cause>>,
    pub loads: Vec<Option<Cause>>,
}

/// [`analyze`] with the causes of the unknown offsets of stores and loads.
pub fn analyze_all(native: &NativeProgram) -> Option<(Footprint, Causes)> {
    let p = crate::spill::build(native).ok()?;
    let st = crate::structure::structure(&p);
    let res = crate::resource::track(&p, &native.layout, native.stage, st.as_ref().ok()).ok()?;
    Some(Analysis::new(&p, native, &res).scan())
}

#[derive(Clone, Copy)]
enum Def {
    Input(Reg),
    Phi(usize, usize),
    Inst(usize, usize),
}

#[derive(Clone, Copy)]
enum Op {
    Add,
    Sub,
    Mul,
    Shl,
    Shr,
    And,
    Or,
    Min,
    Max,
}

impl Op {
    fn apply(self, x: u32, y: u32) -> u32 {
        match self {
            Op::Add => x.wrapping_add(y),
            Op::Sub => x.wrapping_sub(y),
            Op::Mul => x.wrapping_mul(y),
            Op::Shl => x << (y & 31),
            Op::Shr => x >> (y & 31),
            Op::And => x & y,
            Op::Or => x | y,
            Op::Min => x.min(y),
            Op::Max => x.max(y),
        }
    }
}

struct Analysis<'a> {
    p: &'a Program,
    native: &'a NativeProgram,
    res: &'a Resources,
    defs: Vec<Option<Def>>,
    /// Entry values that an instruction reads as an operand (not only as the
    /// old value of a register it writes).
    real: HashSet<u32>,
    params: HashMap<u32, (usize, u32)>,
    /// Compute: thread id VGPRs, the SGPR of each group id.
    cs: Option<(u8, [Option<u8>; 3])>,
    scalar: HashMap<(usize, usize), Address>,
    memo: RefCell<HashMap<(u32, u32, u32), (Option<Expr>, Option<Cause>)>>,
    busy: RefCell<HashSet<(u32, u32, u32)>>,
    cause: Cell<Option<Cause>>,
    depth: Cell<u32>,
}

impl<'a> Analysis<'a> {
    fn new(p: &'a Program, native: &'a NativeProgram, res: &'a Resources) -> Self {
        let n = p
            .inputs
            .iter()
            .map(|i| i.1)
            .chain(p.blocks.iter().flat_map(|b| {
                b.phis.iter().map(|f| f.value).chain(b.insts.iter().flat_map(|i| i.writes.iter().map(|w| w.1)))
            }))
            .map(|v| v.0 as usize + 1)
            .max()
            .unwrap_or(0);
        let mut defs: Vec<Option<Def>> = vec![None; n];
        for &(reg, v) in &p.inputs {
            defs[v.0 as usize] = Some(Def::Input(reg));
        }
        for (b, block) in p.blocks.iter().enumerate() {
            for (k, f) in block.phis.iter().enumerate() {
                defs[f.value.0 as usize] = Some(Def::Phi(b, k));
            }
            for (i, inst) in block.insts.iter().enumerate() {
                for w in &inst.writes {
                    defs[w.1.0 as usize] = Some(Def::Inst(b, i));
                }
            }
        }
        let is_input = |v: Value| matches!(defs.get(v.0 as usize), Some(Some(Def::Input(_))));
        let mut real = HashSet::new();
        for block in &p.blocks {
            for f in &block.phis {
                real.extend(f.args.iter().filter(|a| is_input(**a)).map(|a| a.0));
            }
            for inst in &block.insts {
                for &(reg, v) in &inst.reads {
                    if !is_input(v) {
                        continue;
                    }
                    let own = inst.writes.iter().any(|w| w.0 == reg);
                    if !own || mentions(inst, reg) {
                        real.insert(v.0);
                    }
                }
            }
        }
        let cs = match &native.io {
            StageIo::Cs(cs) => Some((cs.thread_id_vgprs(), cs.system_sgprs().0)),
            _ => None,
        };
        Analysis {
            p,
            native,
            res,
            defs,
            real,
            params: res.params.iter().map(|&(v, param, dword)| (v.0, (param, dword))).collect(),
            cs,
            scalar: res.scalar.iter().map(|(site, a)| (*site, *a)).collect(),
            memo: RefCell::default(),
            busy: RefCell::default(),
            cause: Cell::new(None),
            depth: Cell::new(0),
        }
    }

    fn top(&self, why: Cause) -> Expr {
        if self.cause.get().is_none() {
            self.cause.set(Some(why));
        }
        Expr::Top
    }

    fn cap(&self, e: Expr) -> Expr {
        if e.size() > MAX_NODES { self.top(Cause::Size) } else { e }
    }

    fn mk(&self, op: Op, a: Expr, b: Expr) -> Expr {
        if let (Expr::Const(x), Expr::Const(y)) = (&a, &b) {
            return Expr::Const(op.apply(*x, *y));
        }
        let (a, b) = (Box::new(a), Box::new(b));
        self.cap(match op {
            Op::Add => Expr::Add(a, b),
            Op::Sub => Expr::Sub(a, b),
            Op::Mul => Expr::Mul(a, b),
            Op::Shl => Expr::Shl(a, b),
            Op::Shr => Expr::Shr(a, b),
            Op::And => Expr::And(a, b),
            Op::Or => Expr::Or(a, b),
            Op::Min => Expr::Min(a, b),
            Op::Max => Expr::Max(a, b),
        })
    }

    /// Either of two values (`None`: no value yet, a register nothing set).
    fn join(&self, a: Option<Expr>, b: Option<Expr>) -> Option<Expr> {
        match (a, b) {
            (None, x) | (x, None) => x,
            (Some(a), Some(b)) if a == b => Some(a),
            (Some(a), Some(b)) => Some(self.cap(Expr::Select(Box::new(a), Box::new(b)))),
        }
    }

    /// The value `v` holds, as an expression; `None` for a register that
    /// nothing wrote and no instruction reads as an operand.
    fn value(&self, v: Value, ctx: Ctx) -> Option<Expr> {
        let key = (v.0, ctx.0.map_or(0, |e| e.0 + 1), ctx.1.map_or(0, |e| e.0 + 1));
        let cached = self.memo.borrow().get(&key).cloned();
        if let Some((e, why)) = cached {
            if let Some(why) = why {
                self.top_with(why);
            }
            return e;
        }
        if self.depth.get() >= MAX_DEPTH {
            return Some(self.top(Cause::Size));
        }
        if !self.busy.borrow_mut().insert(key) {
            return Some(self.top(Cause::Cyclic));
        }
        self.depth.set(self.depth.get() + 1);
        let outer = self.cause.replace(None);
        let e = self.compute(v, ctx);
        let why = self.cause.get();
        self.cause.set(outer.or(why));
        self.depth.set(self.depth.get() - 1);
        self.busy.borrow_mut().remove(&key);
        self.memo.borrow_mut().insert(key, (e.clone(), why));
        e
    }

    fn top_with(&self, why: Cause) {
        if self.cause.get().is_none() {
            self.cause.set(Some(why));
        }
    }

    fn compute(&self, v: Value, ctx: Ctx) -> Option<Expr> {
        match self.defs.get(v.0 as usize).copied().flatten() {
            None => Some(self.top(Cause::Input)),
            Some(Def::Input(reg)) => self.entry(v, reg),
            Some(Def::Phi(b, k)) => {
                let phi = &self.p.blocks[b].phis[k];
                let mut out = None;
                for &arg in &phi.args {
                    let e = self.value(arg, ctx);
                    out = self.join(out, e);
                }
                out.or_else(|| Some(self.top(Cause::Input)))
            }
            Some(Def::Inst(b, i)) => {
                let inst = &self.p.blocks[b].insts[i];
                let reg = inst.writes.iter().find(|w| w.1 == v)?.0;
                let own = exec_of(inst);
                let new = self.sem(inst, reg, own);
                if matches!(reg, Reg::V(_)) && !(own.0.is_some() && own == ctx) {
                    // Lanes that did not run it keep the old value (where
                    // the user runs under the same EXEC value, every lane
                    // that uses it ran it).
                    let old = inst.read(reg).and_then(|o| self.value(o, ctx));
                    self.join(Some(new), old)
                } else {
                    Some(new)
                }
            }
        }
    }

    /// The value of register `reg` on entry.
    fn entry(&self, v: Value, reg: Reg) -> Option<Expr> {
        match reg {
            Reg::V(c) if self.cs.is_some_and(|cs| c < cs.0) => Some(Expr::Thread(c)),
            Reg::V(_) => self.real.contains(&v.0).then(|| self.top(Cause::Input)),
            Reg::S(s) => {
                if let Some(cs) = &self.cs
                    && let Some(c) = cs.1.iter().position(|g| *g == Some(s))
                {
                    return Some(Expr::Group(c as u8));
                }
                if let Some(&(param, dword)) = self.params.get(&v.0)
                    && let Some(rp) = self.native.layout.get(param)
                    && rp.root == RootType::Constants
                {
                    return Some(Expr::Root { slot: rp.slot, dword });
                }
                Some(self.top(Cause::Input))
            }
            Reg::Scc => Some(self.top(Cause::Input)),
        }
    }

    fn read(&self, inst: &Inst, reg: Reg, ctx: Ctx) -> Expr {
        match inst.read(reg).and_then(|v| self.value(v, ctx)) {
            Some(e) => e,
            None => self.top(Cause::Input),
        }
    }

    fn operand(&self, inst: &Inst, op: &Operand, ctx: Ctx) -> Expr {
        match *op {
            Operand::Scalar { code: NULL, .. } => Expr::Const(0),
            Operand::Scalar { code, .. } => self.read(inst, Reg::S(code), ctx),
            Operand::Vector { reg, .. } => self.read(inst, Reg::V(reg), ctx),
            Operand::Const(c) => match crate::resource::inline(c) {
                Some(x) if (128..=208).contains(&c) => Expr::Const(x),
                _ => self.top(Cause::Op),
            },
            Operand::Literal(x) => Expr::Const(x),
            Operand::Attr { .. } => self.top(Cause::Op),
        }
    }

    /// What `inst` writes to `reg`, for the lanes that run it.
    fn sem(&self, inst: &Inst, reg: Reg, ctx: Ctx) -> Expr {
        let name = inst.machine.name().unwrap_or("");
        match &inst.fields {
            Fields::Sop { dst: Some(Operand::Scalar { code, dwords: 1 }), src, imm } if Reg::S(*code) == reg => {
                let a = |k: usize| match src.get(k) {
                    Some(o) => self.operand(inst, o, ctx),
                    None => self.top(Cause::Op),
                };
                let lshl_add = |n: u32| self.mk(Op::Add, self.mk(Op::Shl, a(0), Expr::Const(n)), a(1));
                match name {
                    "s_mov_b32" => a(0),
                    "s_movk_i32" => match imm {
                        Some(i) => Expr::Const(*i as i16 as i32 as u32),
                        None => self.top(Cause::Op),
                    },
                    "s_add_u32" | "s_add_i32" => self.mk(Op::Add, a(0), a(1)),
                    "s_sub_u32" | "s_sub_i32" => self.mk(Op::Sub, a(0), a(1)),
                    "s_mul_i32" => self.mk(Op::Mul, a(0), a(1)),
                    "s_lshl_b32" => self.mk(Op::Shl, a(0), a(1)),
                    "s_lshr_b32" => self.mk(Op::Shr, a(0), a(1)),
                    "s_and_b32" => self.mk(Op::And, a(0), a(1)),
                    "s_or_b32" => self.mk(Op::Or, a(0), a(1)),
                    "s_min_u32" => self.mk(Op::Min, a(0), a(1)),
                    "s_max_u32" => self.mk(Op::Max, a(0), a(1)),
                    "s_lshl1_add_u32" => lshl_add(1),
                    "s_lshl2_add_u32" => lshl_add(2),
                    "s_lshl3_add_u32" => lshl_add(3),
                    "s_lshl4_add_u32" => lshl_add(4),
                    "s_bfe_u32" => {
                        let s = a(1);
                        let off = self.mk(Op::And, s.clone(), Expr::Const(31));
                        let width = self.mk(Op::And, self.mk(Op::Shr, s, Expr::Const(16)), Expr::Const(127));
                        self.cap(Expr::Bfe(Box::new(a(0)), Box::new(off), Box::new(width)))
                    }
                    "s_cselect_b32" => self.join(Some(a(0)), Some(a(1))).unwrap_or(Expr::Top),
                    _ => self.top(Cause::Op),
                }
            }
            Fields::Valu(v) => {
                let plain = v.abs == 0
                    && v.neg == 0
                    && v.neg_hi == 0
                    && v.omod == 0
                    && v.opsel == 0
                    && v.opsel_hi == 0
                    && !v.clamp
                    && v.sdwa.is_none()
                    && v.dpp.is_none();
                let mine = match v.dst {
                    Some(Operand::Vector { reg: r, dwords: 1 }) => Reg::V(r) == reg,
                    Some(Operand::Scalar { code, dwords: 1 }) => name == "v_readfirstlane_b32" && Reg::S(code) == reg,
                    _ => false,
                };
                if !plain || !mine {
                    return self.top(Cause::Op);
                }
                let a = |k: usize| match v.src.get(k) {
                    Some(o) => self.operand(inst, o, ctx),
                    None => self.top(Cause::Op),
                };
                let m24 = Expr::Const(0x00FF_FFFF);
                let mul24 = |x: Expr, y: Expr| {
                    self.mk(Op::Mul, self.mk(Op::And, x, m24.clone()), self.mk(Op::And, y, m24.clone()))
                };
                match name {
                    "v_mov_b32" | "v_readfirstlane_b32" => a(0),
                    "v_add_nc_u32" | "v_add_u32" | "v_add_co_u32" => self.mk(Op::Add, a(0), a(1)),
                    "v_sub_nc_u32" | "v_sub_u32" | "v_sub_co_u32" => self.mk(Op::Sub, a(0), a(1)),
                    "v_subrev_nc_u32" | "v_subrev_u32" | "v_subrev_co_u32" => self.mk(Op::Sub, a(1), a(0)),
                    "v_mul_lo_u32" => self.mk(Op::Mul, a(0), a(1)),
                    "v_mul_u32_u24" => mul24(a(0), a(1)),
                    "v_mad_u32_u24" => self.mk(Op::Add, mul24(a(0), a(1)), a(2)),
                    "v_lshlrev_b32" => self.mk(Op::Shl, a(1), a(0)),
                    "v_lshrrev_b32" => self.mk(Op::Shr, a(1), a(0)),
                    "v_and_b32" => self.mk(Op::And, a(0), a(1)),
                    "v_or_b32" => self.mk(Op::Or, a(0), a(1)),
                    "v_min_u32" => self.mk(Op::Min, a(0), a(1)),
                    "v_max_u32" => self.mk(Op::Max, a(0), a(1)),
                    "v_bfe_u32" => {
                        let off = self.mk(Op::And, a(1), Expr::Const(31));
                        let width = self.mk(Op::And, a(2), Expr::Const(31));
                        self.cap(Expr::Bfe(Box::new(a(0)), Box::new(off), Box::new(width)))
                    }
                    "v_lshl_add_u32" => self.mk(Op::Add, self.mk(Op::Shl, a(0), a(1)), a(2)),
                    "v_lshl_or_b32" => self.mk(Op::Or, self.mk(Op::Shl, a(0), a(1)), a(2)),
                    "v_add3_u32" => self.mk(Op::Add, self.mk(Op::Add, a(0), a(1)), a(2)),
                    "v_cndmask_b32" => self.join(Some(a(0)), Some(a(1))).unwrap_or(Expr::Top),
                    _ => self.top(Cause::Op),
                }
            }
            Fields::Smem { .. } | Fields::Buffer { .. } | Fields::Flat { .. } | Fields::Ds { .. } | Fields::Image { .. } => {
                self.top(Cause::Loaded)
            }
            _ => self.top(Cause::Op),
        }
    }

    /// Every store and load of the program.
    fn scan(&self) -> (Footprint, Causes) {
        let mut out = Footprint::default();
        let mut causes = Causes::default();
        let described: HashMap<(usize, usize), &crate::resource::Descriptor> = self
            .res
            .descriptors
            .iter()
            .filter(|d| d.role == crate::resource::Role::Resource)
            .map(|d| (d.site, d))
            .collect();
        // Scalar loads that fetch a descriptor: reads of the descriptor heap, not of a view.
        let mut fetches: HashSet<(usize, usize)> = HashSet::new();
        for d in &self.res.descriptors {
            for leaf in d.source.leaves() {
                if let Source::Loaded { load, .. } | Source::Fetched { load, .. } = leaf {
                    fetches.insert(*load);
                }
            }
        }
        for (b, block) in self.p.blocks.iter().enumerate() {
            if block.exit == Exit::Indirect {
                out.unbounded = true;
                out.unbounded_loads = true;
                out.images_unknown = true;
            }
            for (i, inst) in block.insts.iter().enumerate() {
                let enc = inst.machine.encoding;
                let name = inst.machine.name();
                let site = (b, i);
                let is = |words: &[&str]| name.is_some_and(|n| words.iter().any(|w| n.contains(w)));
                match enc {
                    Encoding::Mubuf | Encoding::Mtbuf => {
                        if name.is_none() {
                            out.unbounded = true;
                            out.unbounded_loads = true;
                            continue;
                        }
                        let store = is(&["store"]) || is(&["atomic"]);
                        let load = is(&["load"]) || is(&["atomic"]);
                        if !store && !load {
                            continue;
                        }
                        let Some(d) = described.get(&site) else {
                            out.unbounded |= store;
                            out.unbounded_loads |= load;
                            continue;
                        };
                        self.buffer_access(inst, &d.source, store, load, &mut out, &mut causes);
                    }
                    Encoding::Smem => match name {
                        None => {
                            out.unbounded = true;
                            out.unbounded_loads = true;
                        }
                        Some(n) if n.contains("store") || n.contains("atomic") => {
                            out.unbounded = true;
                            out.unbounded_loads = true;
                        }
                        Some(n) if n.contains("load") => {
                            if !fetches.contains(&site) {
                                let source = described.get(&site).map(|d| &d.source);
                                self.scalar_load(inst, site, n, source, &mut out, &mut causes);
                            }
                        }
                        Some(_) => {}
                    },
                    Encoding::Flat | Encoding::Global => match name {
                        None => {
                            out.unbounded = true;
                            out.unbounded_loads = true;
                        }
                        Some(n) if n.contains("scratch") => {}
                        Some(n) => {
                            out.by_address |= n.contains("store") || n.contains("atomic");
                            out.by_address_loads |= n.contains("load") || n.contains("atomic");
                        }
                    },
                    Encoding::Sop1 => {
                        if name.is_none_or(|n| matches!(n, "s_setpc_b64" | "s_swappc_b64")) {
                            out.unbounded = true;
                            out.unbounded_loads = true;
                            out.images_unknown = true;
                        }
                    }
                    Encoding::Mimg => {
                        let n = name.unwrap_or("");
                        let write = n.contains("store") || n.contains("atomic");
                        let read = !n.contains("store");
                        if n.starts_with("image_bvh") && !described.contains_key(&site) {
                            // Nodes in title memory by address.
                            out.by_address_loads = true;
                            continue;
                        }
                        let targets = described.get(&site).and_then(|d| {
                            let mut all = Vec::new();
                            for leaf in d.source.leaves() {
                                all.push(self.leaf(leaf, exec_of(inst)).map(|l| l.0)?);
                            }
                            Some(all)
                        });
                        match (name, targets) {
                            (Some(_), Some(targets)) => {
                                for t in targets {
                                    if write && !out.image_writes.contains(&t) {
                                        out.image_writes.push(t);
                                    }
                                    if read && !out.image_reads.contains(&t) {
                                        out.image_reads.push(t);
                                    }
                                }
                            }
                            _ => out.images_unknown = true,
                        }
                    }
                    _ => {}
                }
            }
        }
        (out, causes)
    }

    /// A scalar-memory load: through a buffer descriptor (`s_buffer_load_*`), or by the address a root
    /// view gives (`s_load_*`).
    fn scalar_load(
        &self,
        inst: &Inst,
        site: (usize, usize),
        name: &str,
        source: Option<&Source>,
        out: &mut Footprint,
        causes: &mut Causes,
    ) {
        let Fields::Smem { offset, soffset, .. } = &inst.fields else {
            out.unbounded_loads = true;
            return;
        };
        let bytes = match name.rsplit("dword").next() {
            Some("") => 4,
            Some("x2") => 8,
            Some("x3") => 12,
            Some("x4") => 16,
            Some("x8") => 32,
            _ => 64,
        };
        self.cause.set(None);
        let ctx = exec_of(inst);
        if name.starts_with("s_buffer_") {
            let Some(source) = source else {
                out.unbounded_loads = true;
                return;
            };
            let mut off = Expr::Const(*offset as u32);
            if !matches!(soffset, Operand::Scalar { code: NULL, .. } | Operand::Const(128)) {
                let so = self.operand(inst, soffset, ctx);
                off = self.mk(Op::Add, off, so);
            }
            let base_cause = self.cause.get();
            for leaf in source.leaves() {
                self.cause.set(base_cause);
                let Some((target, _, base, _)) = self.leaf(leaf, ctx) else {
                    out.unbounded_loads = true;
                    return;
                };
                let at = self.mk(Op::Add, off.clone(), base);
                causes.loads.push(if at.has_top() { self.cause.get() } else { None });
                out.loads.push(Store { target, offset: at, bytes });
            }
            return;
        }
        // By an address: the root parameter it comes from says what is read.
        let Some(a) = self.scalar.get(&site) else {
            out.by_address_loads = true;
            return;
        };
        match self.native.layout.get(a.param).map(|rp| (rp.root, rp.slot)) {
            // Descriptors of a table's own memory.
            Some((RootType::Table | RootType::ResourceHeap | RootType::SamplerHeap, _)) => {}
            // The program's own constants.
            None if a.param == crate::resource::CODE || a.param == crate::resource::GS_TABLE => {}
            Some((RootType::Descriptor, slot)) if !crate::spill::is_spilled(self.native) => {
                let mut at = Expr::Const(a.offset);
                if let Some(d) = a.dynamic {
                    let d = self.value(d, ctx).unwrap_or_else(|| self.top(Cause::Input));
                    at = self.mk(Op::Add, at, d);
                }
                causes.loads.push(if at.has_top() { self.cause.get() } else { None });
                out.loads.push(Store { target: Target::Root { slot }, offset: at, bytes });
            }
            _ => out.by_address_loads = true,
        }
    }

    /// The stores and loads one buffer access stands for, one per descriptor it may go through.
    fn buffer_access(&self, inst: &Inst, source: &Source, store: bool, load: bool, out: &mut Footprint, causes: &mut Causes) {
        let Fields::Buffer { addr, soffset, offset, offen, idxen, format, lds, tfe, .. } = &inst.fields else {
            out.unbounded |= store;
            out.unbounded_loads |= load;
            return;
        };
        let name = inst.machine.name().unwrap_or("");
        let typed = format.is_some() || name.contains("format") || *lds || *tfe;
        let bytes = if name.contains("atomic") {
            if name.ends_with("_x2") { 8 } else { 4 }
        } else if name.contains("dwordx4") || name.ends_with("_xyzw") {
            16
        } else if name.contains("dwordx3") || name.ends_with("_xyz") {
            12
        } else if name.contains("dwordx2") || name.ends_with("_xy") || name.contains("byte") || name.contains("short") {
            // (A byte or a short is written as an atomic on the dword that holds it, and may continue in the next.)
            8
        } else {
            4
        };
        let narrow = name.contains("byte") || name.contains("short");
        self.cause.set(None);
        let ctx = exec_of(inst);
        let (mut index, mut voffset) = (None, None);
        if let Operand::Vector { reg, .. } = *addr {
            if *idxen {
                index = Some(self.read(inst, Reg::V(reg), ctx));
            }
            if *offen {
                voffset = Some(self.read(inst, Reg::V(reg + u8::from(*idxen)), ctx));
            }
        }
        let mut off = Expr::Const(u32::from(*offset));
        if let Some(v) = voffset {
            off = self.mk(Op::Add, off, v);
        }
        if !matches!(soffset, Operand::Scalar { code: NULL, .. } | Operand::Const(128)) {
            let s = self.operand(inst, soffset, ctx);
            off = self.mk(Op::Add, off, s);
        }
        let base_cause = self.cause.get();
        for leaf in source.leaves() {
            self.cause.set(base_cause);
            let Some((target, used, base, dynamic)) = self.leaf(leaf, ctx) else {
                out.unbounded |= store;
                out.unbounded_loads |= load;
                return;
            };
            let mut at = self.mk(Op::Add, off.clone(), base);
            if let Some(index) = &index {
                at = match used {
                    _ if dynamic => self.top(Cause::Op),
                    Some(u) if u.kind != ResourceType::Cbv && u.stride >= 4 && u.stride % 4 == 0 => {
                        let scaled = self.mk(Op::Mul, index.clone(), Expr::Const(u.stride));
                        self.mk(Op::Add, scaled, at)
                    }
                    Some(u) if u.stride == 1 => self.mk(Op::Add, index.clone(), at),
                    _ => self.top(Cause::Op),
                };
            }
            if narrow {
                at = self.mk(Op::And, at, Expr::Const(!3));
            }
            if typed {
                at = self.top(Cause::Typed);
            }
            let why = if at.has_top() { self.cause.get() } else { None };
            if store {
                causes.stores.push(why);
                out.stores.push(Store { target, offset: at.clone(), bytes });
            }
            if load {
                causes.loads.push(why);
                out.loads.push(Store { target, offset: at, bytes });
            }
        }
    }

    /// Where one descriptor comes from: the target, the used-resource entry
    /// that covers it, the offset the descriptor adds, and whether the entry is
    /// picked by a computed index. `None`: not known.
    fn leaf(&self, source: &Source, ctx: Ctx) -> Option<(Target, Option<&'a UsedResource>, Expr, bool)> {
        match source {
            Source::Built { address, .. } => {
                let rp = *self.native.layout.get(address.param)?;
                if rp.root != RootType::Descriptor || crate::spill::is_spilled(self.native) {
                    return None;
                }
                let native: &'a NativeProgram = self.native;
                let used = native.used.iter().find(|u| u.user_data == rp.user_data);
                let mut base = Expr::Const(address.offset);
                if let Some(d) = address.dynamic {
                    let d = self.value(d, ctx).unwrap_or_else(|| self.top(Cause::Input));
                    base = self.mk(Op::Add, base, d);
                }
                Some((Target::Root { slot: rp.slot }, used, base, false))
            }
            Source::Loaded { load, first } => {
                let a = self.scalar.get(load)?;
                let rp = *self.native.layout.get(a.param)?;
                if rp.root != RootType::Table {
                    return None;
                }
                let dword = a.offset / 4 + u32::from(*first);
                let native: &'a NativeProgram = self.native;
                let used = native
                    .used
                    .iter()
                    .filter(|u| u.root == RootType::Table && u.user_data == rp.user_data && u.table_offset_dwords <= dword)
                    .max_by_key(|u| u.table_offset_dwords);
                let dynamic = a.dynamic.is_some();
                let target = match used {
                    Some(u) if !dynamic => Target::Table { user_data: rp.user_data, dword: u.table_offset_dwords },
                    _ => Target::TableAny { user_data: rp.user_data },
                };
                Some((target, used, Expr::Const(0), dynamic))
            }
            _ => None,
        }
    }
}

/// The EXEC values an instruction runs under (low and, in wave64, high).
type Ctx = (Option<Value>, Option<Value>);

fn exec_of(inst: &Inst) -> Ctx {
    (inst.read(Reg::S(crate::ir::EXEC)), inst.read(Reg::S(crate::ir::EXEC + 1)))
}

/// Whether `inst` names `reg` among the sources of a vector operation.
fn mentions(inst: &Inst, reg: Reg) -> bool {
    let Reg::V(r) = reg else { return false };
    let Fields::Valu(v) = &inst.fields else { return false };
    v.src.iter().any(|o| matches!(*o, Operand::Vector { reg: s, dwords } if s <= r && u16::from(r) < u16::from(s) + u16::from(dwords)))
}
