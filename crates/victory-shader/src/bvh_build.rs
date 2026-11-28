//! A reference builder, walker and checker for the console's ray-tracing
//! structures ([`crate::bvh`] has the node test itself).
//!
//! The title's own builder (53 compute programs) makes the structures its
//! trace programs walk. Nothing of the title may be in a test, so this
//! module is the stand-in used to prove, without the title, that the part of
//! the fork the builder depends on is right: the node formats and the
//! instruction that tests one node ([`crate::bvh::intersect`], and its
//! emitted form, which `emit::intersect_tests` runs on the card and WARP over
//! a whole tree made here). It has three parts:
//!
//! * [`build`]: triangles to a structure in the console's node format: a
//!   64-byte header, then box nodes (16-bit bounds rounded outward, or
//!   128-byte nodes with 32-bit bounds) of up to four children and triangle
//!   nodes. A node pointer is `node index << 3 | kind` counted from the
//!   structure's start (the header is node 0), which is how the title's
//!   32-bit form of the instruction reads them (the descriptor holds the
//!   structure's address).
//! * [`Walker`]: the closest hit of one ray, as a state machine that asks
//!   for one node test at a time, so the same walk runs with the CPU test
//!   ([`walk`]) and with the card testing a whole batch of rays' nodes per
//!   dispatch.
//! * [`check`]: the invariants of any structure in memory (pointers inside
//!   it, no node reached twice, boxes holding what hangs under them, what is
//!   not reached), so a structure the title's builder made can be told from
//!   garbage, and one that overlaps another from one that does not.
//!
//! Header: only what is known from dumps is written ([`Header`]). Dword 0
//! holds the version (6) in the low half and marks in the high one, dword 1
//! the index of the last 64-byte node (the size is one more: the header is
//! counted), dword 3 the number of elements, dwords 4-9 the bounds, dword 12
//! the root's pointer. The offsets of dwords 10, 13 and 14 are not written:
//! this builder keeps no parent table and mixes its box and triangle nodes.
//!
//! A triangle node made here holds one triangle as vertices 0, 1, 2 (the
//! first being the one neither barycentric weighs, I's the second, J's the
//! third, so the node's vertices are the input's in order) and the input
//! triangle's number in the dword of vertex 4, which the instruction does
//! not read.

use crate::bvh::{self, NONE, Ray};

/// Dwords of a node.
const NODE: usize = 16;
/// Dwords of the header.
const HEADER: usize = NODE;
/// The id dword of a node with one triangle: I weighs vertex 1, J vertex 2.
const ID: u32 = 1 | 2 << 2;
/// The dword that holds a triangle's number (vertex 4's x).
const NUMBER: usize = 12;
/// Kind of the pointer of a 16-bit box node.
pub const BOX16: u32 = 4;
/// Kind of the pointer of a 32-bit box node.
pub const BOX32: u32 = 5;
/// The version in the header.
const VERSION: u32 = 6;

/// A triangle: three vertices.
pub type Triangle = [[f32; 3]; 3];

/// A box: minimum x y z, maximum x y z.
pub type Bounds = [f32; 6];

/// How a structure is built.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Box nodes of 128 bytes with 32-bit bounds (kind 5) instead of 64
    /// bytes with 16-bit ones (kind 4).
    pub wide_boxes: bool,
}

/// A structure and where its walk starts.
#[derive(Clone, Debug)]
pub struct Built {
    /// The nodes' dwords from the header on.
    pub words: Vec<u32>,
    /// The root's pointer; [`NONE`] when no triangle went in.
    pub root: u32,
    /// Triangles in the structure.
    pub triangles: usize,
    /// Input triangles left out for a vertex that is not finite.
    pub skipped: usize,
}

impl Built {
    /// The structure's size in 64-byte nodes.
    pub fn nodes(&self) -> usize {
        self.words.len() / NODE
    }
}

/// What the header of a structure says (the fields that are known).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Header {
    /// The marks in the high half of dword 0: 0x8000 a bottom level, 0x4
    /// compacted, 0x2 rehydrated, 0x100 still packed.
    pub marks: u32,
    /// The structure's size in 64-byte nodes, the header counted.
    pub nodes: usize,
    pub elements: u32,
    pub bounds: Bounds,
    pub root: u32,
}

impl Header {
    pub const BOTTOM: u32 = 0x8000;
    pub const PACKED: u32 = 0x100;

    /// The header at the start of `words`, if they are one: version 6 and a
    /// size of at least the header and one node, at most 2^21 nodes (128 MiB).
    pub fn read(words: &[u32]) -> Option<Self> {
        let words: &[u32; HEADER] = words.get(..HEADER)?.try_into().ok()?;
        let nodes = words[1] as usize + 1;
        (words[0] & 0xffff == VERSION && (2..=1 << 21).contains(&nodes)).then(|| Self {
            marks: words[0] >> 16,
            nodes,
            elements: words[3],
            bounds: std::array::from_fn(|k| f32::from_bits(words[4 + k])),
            root: words[12],
        })
    }

    pub fn bottom(&self) -> bool {
        self.marks & Self::BOTTOM != 0
    }

    pub fn packed(&self) -> bool {
        self.marks & Self::PACKED != 0
    }
}

/// The 28 dwords the node test reads at `pointer` (zeros past the end).
pub fn node(words: &[u32], pointer: u32) -> [u32; 28] {
    let at = (pointer >> 3) as usize * NODE;
    std::array::from_fn(|k| words.get(at + k).copied().unwrap_or(0))
}

/// The number a triangle node of this builder holds for its triangle.
pub fn number(words: &[u32], pointer: u32) -> u32 {
    words
        .get((pointer >> 3) as usize * NODE + NUMBER)
        .copied()
        .unwrap_or(NONE)
}

/// The magnitude of `x` (its bits with the sign cleared) as a 16-bit float's
/// bits rounded toward zero, and whether that was exact.
fn half_magnitude(mag: u32) -> (u16, bool) {
    let exp = (mag >> 23) as i32;
    if exp == 0xff {
        return (0x7c00, mag & 0x7f_ffff == 0);
    }
    let e = exp - 127;
    if e > 15 {
        return (0x7bff, false);
    }
    if e >= -14 {
        let half = (((e + 15) as u32) << 10 | (mag & 0x7f_ffff) >> 13) as u16;
        return (half, mag & 0x1fff == 0);
    }
    if exp == 0 {
        return (0, mag == 0);
    }
    let shift = (-1 - e) as u32;
    if shift >= 32 {
        return (0, false);
    }
    let full = 1u32 << 23 | (mag & 0x7f_ffff);
    ((full >> shift) as u16, full & ((1u32 << shift) - 1) == 0)
}

/// The largest 16-bit float not above `x` (a NaN is the 16-bit NaN).
pub fn half_down(x: f32) -> u16 {
    if x.is_nan() {
        return 0x7e00;
    }
    let (m, exact) = half_magnitude(x.to_bits() & 0x7fff_ffff);
    match (x.is_sign_negative(), exact) {
        (false, _) => m,
        (true, true) => 0x8000 | m,
        (true, false) => 0x8000 | (m + 1),
    }
}

/// The smallest 16-bit float not below `x` (a NaN is the 16-bit NaN).
pub fn half_up(x: f32) -> u16 {
    if x.is_nan() {
        return 0x7e00;
    }
    let (m, exact) = half_magnitude(x.to_bits() & 0x7fff_ffff);
    match (x.is_sign_negative(), exact) {
        (true, _) => 0x8000 | m,
        (false, true) => m,
        (false, false) => m + 1,
    }
}

/// The bounds nothing is inside of: what an empty slot holds.
const EMPTY: Bounds = [
    f32::INFINITY,
    f32::INFINITY,
    f32::INFINITY,
    f32::NEG_INFINITY,
    f32::NEG_INFINITY,
    f32::NEG_INFINITY,
];

fn union(a: &Bounds, b: &Bounds) -> Bounds {
    std::array::from_fn(|k| {
        if k < 3 {
            a[k].min(b[k])
        } else {
            a[k].max(b[k])
        }
    })
}

fn contains(outer: &Bounds, inner: &Bounds) -> bool {
    (0..3).all(|k| outer[k] <= inner[k] && inner[k + 3] <= outer[k + 3])
}

fn triangle_bounds(t: &Triangle) -> Bounds {
    let mut b = EMPTY;
    for v in t {
        b = union(&b, &[v[0], v[1], v[2], v[0], v[1], v[2]]);
    }
    b
}

/// What a box slot of this width holds for `b`: the bounds rounded outward
/// to 16 bits, or as they are.
fn stored(b: &Bounds, wide: bool) -> Bounds {
    if wide {
        *b
    } else {
        std::array::from_fn(|k| {
            let h = if k < 3 {
                half_down(b[k])
            } else {
                half_up(b[k])
            };
            bvh::half_to_f32(h)
        })
    }
}

struct Builder<'a> {
    triangles: &'a [Triangle],
    wide: bool,
    words: Vec<u32>,
}

impl Builder<'_> {
    /// Room for a node of `count` nodes' worth of dwords; its index.
    fn alloc(&mut self, nodes: usize) -> usize {
        let at = self.words.len() / NODE;
        self.words.resize(self.words.len() + nodes * NODE, 0);
        at
    }

    /// The node for `prims` (indices of input triangles, at least one): its
    /// pointer and the bounds as stored in its parent's slot.
    fn node(&mut self, prims: &mut [u32]) -> (u32, Bounds) {
        if prims.len() == 1 {
            let one = prims[0];
            return self.leaf(one);
        }
        let groups = Self::groups(self.triangles, prims);
        let at = self.alloc(if self.wide { 2 } else { 1 });
        let kind = if self.wide { BOX32 } else { BOX16 };
        let mut children = [NONE; 4];
        let mut boxes = [EMPTY; 4];
        let mut all = EMPTY;
        let mut rest = prims;
        for (n, size) in groups.iter().enumerate() {
            let (these, after) = std::mem::take(&mut rest).split_at_mut(*size);
            rest = after;
            let (pointer, bounds) = self.node(these);
            let slot = stored(&bounds, self.wide);
            children[n] = pointer;
            boxes[n] = slot;
            all = union(&all, &slot);
        }
        let base = at * NODE;
        self.words[base..base + 4].copy_from_slice(&children);
        for (b, slot) in boxes.iter().enumerate() {
            if self.wide {
                for (k, x) in slot.iter().enumerate() {
                    self.words[base + 4 + 6 * b + k] = x.to_bits();
                }
            } else {
                for k in 0..6 {
                    let half = 6 * b + k;
                    let h = if k < 3 {
                        half_down(slot[k])
                    } else {
                        half_up(slot[k])
                    };
                    self.words[base + 4 + half / 2] |= u32::from(h) << (16 * (half % 2));
                }
            }
        }
        ((at as u32) << 3 | kind, all)
    }

    fn leaf(&mut self, prim: u32) -> (u32, Bounds) {
        let t = self.triangles[prim as usize];
        let at = self.alloc(1) * NODE;
        for (n, v) in t.iter().enumerate() {
            for (c, x) in v.iter().enumerate() {
                self.words[at + 3 * n + c] = x.to_bits();
            }
        }
        self.words[at + NUMBER] = prim;
        self.words[at + 15] = ID;
        (((at / NODE) as u32) << 3, triangle_bounds(&t))
    }

    /// Splits `prims` (sorted in place) into up to four groups by the
    /// median of their centres on the longest axis, twice; the groups' sizes.
    fn groups(triangles: &[Triangle], prims: &mut [u32]) -> Vec<usize> {
        let mut sizes = Vec::with_capacity(4);
        let (low, high) = Self::halve(triangles, prims);
        for (start, len) in [(0, low), (low, high)] {
            let half = &mut prims[start..start + len];
            if len >= 2 {
                let (a, b) = Self::halve(triangles, half);
                sizes.extend([a, b]);
            } else if len == 1 {
                sizes.push(1);
            }
        }
        sizes
    }

    /// Sorts `prims` by the centre on the longest axis of their centres and
    /// returns the sizes of the two halves.
    fn halve(triangles: &[Triangle], prims: &mut [u32]) -> (usize, usize) {
        let centre = |p: u32| {
            let b = triangle_bounds(&triangles[p as usize]);
            [
                (b[0] + b[3]) * 0.5,
                (b[1] + b[4]) * 0.5,
                (b[2] + b[5]) * 0.5,
            ]
        };
        let mut c = EMPTY;
        for &p in prims.iter() {
            let m = centre(p);
            c = union(&c, &[m[0], m[1], m[2], m[0], m[1], m[2]]);
        }
        let spread = |k: usize| c[k + 3] - c[k];
        let axis = (0..3).fold(0, |best, k| if spread(k) > spread(best) { k } else { best });
        prims.sort_by(|&a, &b| centre(a)[axis].total_cmp(&centre(b)[axis]).then(a.cmp(&b)));
        let low = prims.len() / 2;
        (low, prims.len() - low)
    }
}

/// Builds a structure over `triangles` (numbered by their place in it).
pub fn build(triangles: &[Triangle], options: Options) -> Built {
    let mut prims: Vec<u32> = (0..triangles.len() as u32)
        .filter(|&p| {
            triangles[p as usize]
                .iter()
                .flatten()
                .all(|x| x.is_finite())
        })
        .collect();
    let skipped = triangles.len() - prims.len();
    let mut b = Builder {
        triangles,
        wide: options.wide_boxes,
        words: vec![0; HEADER],
    };
    let (root, bounds) = if prims.is_empty() {
        (NONE, [0.0; 6])
    } else {
        b.node(&mut prims)
    };
    let nodes = (b.words.len() / NODE) as u32;
    b.words[0] = VERSION;
    b.words[1] = nodes - 1;
    b.words[3] = prims.len() as u32;
    for (k, x) in bounds.iter().enumerate() {
        b.words[4 + k] = x.to_bits();
    }
    b.words[12] = root;
    Built {
        words: b.words,
        root,
        triangles: prims.len(),
        skipped,
    }
}

/// What the closest hit of a ray is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    /// The distance in the ray's units.
    pub t: f32,
    /// The weights of vertices 1 and 2.
    pub i: f32,
    pub j: f32,
    /// The triangle was seen clockwise from the ray's origin.
    pub front: bool,
    /// The triangle's pointer.
    pub pointer: u32,
}

/// The walk of one ray to its closest hit, one node test at a time:
/// [`Walker::ask`] names the node to test, [`Walker::give`] takes what the
/// instruction returned for it. Box nodes are entered nearest child first
/// when the test sorts; a hit shortens the ray.
#[derive(Clone, Debug)]
pub struct Walker {
    ray: Ray,
    stack: Vec<u32>,
    asked: Option<u32>,
    /// The closest hit so far.
    pub best: Option<Hit>,
    /// Node tests given so far.
    pub tests: u32,
}

impl Walker {
    pub fn new(root: u32, ray: Ray) -> Self {
        Self {
            ray,
            stack: if root == NONE { Vec::new() } else { vec![root] },
            asked: None,
            best: None,
            tests: 0,
        }
    }

    /// The ray as it is now (its extent is the closest hit's distance).
    pub fn ray(&self) -> &Ray {
        &self.ray
    }

    /// The pointer of the node to test next, or `None` when the walk is done.
    pub fn ask(&mut self) -> Option<u32> {
        assert!(self.asked.is_none(), "the last node's result was not given");
        let p = self.stack.pop()?;
        self.asked = Some(p);
        Some(p)
    }

    /// What the instruction returned for the node [`Walker::ask`] named.
    pub fn give(&mut self, result: [u32; 4]) {
        let pointer = self.asked.take().expect("no node was asked for");
        self.tests += 1;
        match bvh::kind(u64::from(pointer)) {
            BOX16 | BOX32 => {
                for &child in result.iter().rev() {
                    if child != NONE {
                        self.stack.push(child);
                    }
                }
            }
            0..=3 => {
                let [t, det, i, j] = result.map(f32::from_bits);
                if t != f32::INFINITY {
                    let distance = t / det;
                    if distance <= self.ray.extent && self.best.is_none_or(|b| distance < b.t) {
                        self.best = Some(Hit {
                            t: distance,
                            i: i / det,
                            j: j / det,
                            front: det > 0.0,
                            pointer,
                        });
                        self.ray.extent = distance;
                    }
                }
            }
            _ => {}
        }
    }
}

/// The closest hit of `ray` in `structure` with the CPU's node test, and
/// the number of node tests it took.
pub fn walk(structure: &Built, ray: Ray, sort: bool) -> (Option<Hit>, u32) {
    let mut w = Walker::new(structure.root, ray);
    while let Some(pointer) = w.ask() {
        let kind = bvh::kind(u64::from(pointer));
        let result = bvh::intersect(&node(&structure.words, pointer), kind, w.ray(), sort);
        w.give(result);
    }
    (w.best, w.tests)
}

/// The closest hit of `ray` over the triangles themselves: the distance, and
/// the triangle's number. The reference for [`walk`].
pub fn brute(triangles: &[Triangle], ray: &Ray) -> Option<(f32, usize)> {
    let mut best: Option<(f32, usize)> = None;
    for (n, t) in triangles.iter().enumerate() {
        let [t, det, ..] = bvh::triangle(t, ray);
        if t == f32::INFINITY {
            continue;
        }
        let distance = t / det;
        if distance <= ray.extent && best.is_none_or(|b| distance < b.0) {
            best = Some((distance, n));
        }
    }
    best
}

/// What [`check`] found in a structure that is well formed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Nodes in the structure, the header included.
    pub size: usize,
    /// 64-byte nodes the walk from the root reaches (a 128-byte box counts two).
    pub reached: usize,
    pub box16: usize,
    pub box32: usize,
    /// Triangle nodes, and the triangles of them the boxes name.
    pub triangle_nodes: usize,
    pub triangles: usize,
    /// Pointers of kind 6 and 7, which are not followed.
    pub instances: usize,
    pub procedurals: usize,
    /// The deepest node, the root being 1.
    pub depth: usize,
    /// Slots whose box does not hold what hangs under it.
    pub loose: usize,
    /// Slots that name a child but whose box is empty or switched off.
    pub dead: usize,
}

/// What is wrong with a structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Fewer dwords than a header.
    NoHeader,
    /// A pointer names a node (or the second half of a wide box) outside it.
    Range { pointer: u32 },
    /// A node is reached twice, or two nodes overlap: the structure is a
    /// graph (or another structure was written over this one).
    Shared { pointer: u32 },
    /// The walk is deeper than any structure made by a builder.
    TooDeep { pointer: u32 },
    /// A pointer into the header (node 0).
    Header { pointer: u32 },
    /// With [`check_strict`]: a box does not hold what hangs under it.
    Loose { pointer: u32 },
}

/// What a node's place is taken by.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Taken {
    Free,
    Box,
    BoxTail,
    /// A triangle node; the kinds (0-3) a pointer used.
    Triangles(u8),
}

struct Check<'a> {
    words: &'a [u32],
    taken: Vec<Taken>,
    report: Report,
}

/// The deepest walk accepted.
const MAX_DEPTH: usize = 64;

impl Check<'_> {
    /// Visits `pointer`; the bounds of what is under it (`None`: nothing
    /// known, an instance or a procedural node).
    fn visit(&mut self, pointer: u32, depth: usize) -> Result<Option<Bounds>, Fault> {
        if depth > MAX_DEPTH {
            return Err(Fault::TooDeep { pointer });
        }
        self.report.depth = self.report.depth.max(depth);
        let kind = pointer & 7;
        let index = (pointer >> 3) as usize;
        match kind {
            6 => {
                self.report.instances += 1;
                Ok(None)
            }
            7 => {
                self.report.procedurals += 1;
                Ok(None)
            }
            0..=3 => {
                self.claim(pointer, index, 1)?;
                let w = node(self.words, pointer);
                let v = bvh::triangle_vertices(&w, kind);
                self.report.triangles += 1;
                let mut b = EMPTY;
                for p in v {
                    b = union(&b, &[p[0], p[1], p[2], p[0], p[1], p[2]]);
                }
                Ok(Some(b))
            }
            _ => {
                let wide = kind == BOX32;
                self.claim(pointer, index, if wide { 2 } else { 1 })?;
                if wide {
                    self.report.box32 += 1;
                } else {
                    self.report.box16 += 1;
                }
                let w = node(self.words, pointer);
                let slots = if wide {
                    bvh::boxes32(&w)
                } else {
                    bvh::boxes16(&w)
                };
                let mut all = EMPTY;
                let mut any = false;
                for (c, slot) in bvh::children(&w).into_iter().zip(slots) {
                    if c == NONE {
                        continue;
                    }
                    if slot[0].is_nan() || slot[3].is_nan() || slot[0] > slot[3] {
                        self.report.dead += 1;
                    }
                    if let Some(content) = self.visit(c, depth + 1)? {
                        if !contains(&slot, &content) {
                            self.report.loose += 1;
                        }
                        all = union(&all, &content);
                        any = true;
                    }
                }
                Ok(any.then_some(all))
            }
        }
    }

    /// Takes the place of a node of `span` nodes at `index`.
    fn claim(&mut self, pointer: u32, index: usize, span: usize) -> Result<Taken, Fault> {
        if index == 0 {
            return Err(Fault::Header { pointer });
        }
        if index + span > self.taken.len() {
            return Err(Fault::Range { pointer });
        }
        let kind = pointer & 7;
        if kind <= 3 {
            let bit = 1u8 << kind;
            return match self.taken[index] {
                Taken::Free => {
                    self.taken[index] = Taken::Triangles(bit);
                    self.report.triangle_nodes += 1;
                    self.report.reached += 1;
                    Ok(self.taken[index])
                }
                Taken::Triangles(kinds) if kinds & bit == 0 => {
                    self.taken[index] = Taken::Triangles(kinds | bit);
                    Ok(self.taken[index])
                }
                _ => Err(Fault::Shared { pointer }),
            };
        }
        if self.taken[index..index + span]
            .iter()
            .any(|t| *t != Taken::Free)
        {
            return Err(Fault::Shared { pointer });
        }
        self.taken[index] = Taken::Box;
        if span == 2 {
            self.taken[index + 1] = Taken::BoxTail;
        }
        self.report.reached += span;
        Ok(Taken::Box)
    }
}

/// Walks the structure `words` (its header first, pointers counted from its
/// start) from `root` and checks what a structure made by a builder keeps:
/// every pointer names a node inside it, no node is reached twice and no two
/// overlap. Boxes that do not hold what is under them are counted, not
/// refused ([`check_strict`] refuses them).
pub fn check(words: &[u32], root: u32) -> Result<Report, Fault> {
    if words.len() < HEADER {
        return Err(Fault::NoHeader);
    }
    let mut c = Check {
        words,
        taken: vec![Taken::Free; words.len() / NODE],
        report: Report::default(),
    };
    c.taken[0] = Taken::Box;
    c.report.size = c.taken.len();
    if root != NONE {
        c.visit(root, 1)?;
    }
    Ok(c.report)
}

/// [`check`], and a box that does not hold what hangs under it is a fault.
pub fn check_strict(words: &[u32], root: u32) -> Result<Report, Fault> {
    let report = check(words, root)?;
    if report.loose > 0 {
        return Err(Fault::Loose { pointer: root });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small generator of repeatable numbers.
    struct Numbers(u64);

    impl Numbers {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }

        /// A float in `0..1`.
        fn unit(&mut self) -> f32 {
            (self.next() % 1_000_003) as f32 / 1_000_003.0
        }

        fn between(&mut self, low: f32, high: f32) -> f32 {
            low + (high - low) * self.unit()
        }

        fn point(&mut self, scale: f32) -> [f32; 3] {
            [
                self.between(-scale, scale),
                self.between(-scale, scale),
                self.between(-scale, scale),
            ]
        }
    }

    /// `n` small triangles scattered in a cube.
    fn soup(n: usize, seed: u64) -> Vec<Triangle> {
        let mut g = Numbers(seed);
        (0..n)
            .map(|_| {
                let c = g.point(10.0);
                std::array::from_fn(|_| {
                    let o = g.point(1.2);
                    [c[0] + o[0], c[1] + o[1], c[2] + o[2]]
                })
            })
            .collect()
    }

    /// Two layers of unit quads (two triangles each) facing the z axis: rays
    /// along it through the corners and edges must still hit.
    fn grid(side: usize) -> Vec<Triangle> {
        let mut t = Vec::new();
        for layer in 0..2 {
            let z = 2.0 * layer as f32;
            for y in 0..side {
                for x in 0..side {
                    let (x0, y0) = (x as f32 + 0.5 * layer as f32, y as f32);
                    t.push([[x0, y0 + 1.0, z], [x0 + 1.0, y0, z], [x0, y0, z]]);
                    t.push([
                        [x0, y0 + 1.0, z],
                        [x0 + 1.0, y0 + 1.0, z],
                        [x0 + 1.0, y0, z],
                    ]);
                }
            }
        }
        t
    }

    fn rays(n: usize, seed: u64, scale: f32) -> Vec<Ray> {
        let mut g = Numbers(seed);
        (0..n)
            .map(|_| {
                let o = g.point(scale * 1.5);
                let mut d = g.point(1.0);
                // No component of exactly zero, which the title's rays do have
                // but which this generator would hit with chance only.
                if d.iter().all(|x| x.abs() < 0.05) {
                    d = [0.3, 0.5, 0.7];
                }
                // Aimed through the cube most of the time.
                let target = g.point(scale);
                if g.next() % 4 != 0 {
                    d = [target[0] - o[0], target[1] - o[1], target[2] - o[2]];
                }
                Ray::new(o, d, 1.0e9)
            })
            .collect()
    }

    fn assert_walks_like_brute_force(
        triangles: &[Triangle],
        options: Options,
        rays: &[Ray],
    ) -> (usize, f64) {
        let built = build(triangles, options);
        let mut hits = 0;
        let mut tests = 0u64;
        for sort in [true, false] {
            for ray in rays {
                let (got, n) = walk(&built, *ray, sort);
                let want = brute(triangles, ray);
                match (got, want) {
                    (None, None) => {}
                    (Some(g), Some((t, prim))) => {
                        // The same arithmetic on the same vertices: the same
                        // distance to the bit. Two triangles at one distance
                        // (a shared edge) may come in either order.
                        assert_eq!(
                            g.t.to_bits(),
                            t.to_bits(),
                            "ray {ray:?}: hit {g:?}, wanted triangle {prim} at {t}"
                        );
                        let found = number(&built.words, g.pointer) as usize;
                        let again = bvh::triangle(&triangles[found], ray);
                        assert_eq!(
                            (again[0] / again[1]).to_bits(),
                            t.to_bits(),
                            "triangle {found} is not at {t}"
                        );
                        if sort {
                            hits += 1;
                        }
                    }
                    other => panic!("ray {ray:?}: {other:?}"),
                }
                tests += u64::from(n);
            }
        }
        (hits, tests as f64 / (2 * rays.len()) as f64)
    }

    #[test]
    fn halves_round_outward_and_by_the_least() {
        use crate::bvh::tests::f32_to_half;
        let mut g = Numbers(7);
        let mut values = vec![
            0.0, -0.0, 1.0, -1.0, 65504.0, -65504.0, 65505.0, 70000.0, -70000.0, 1.0e-8, -1.0e-8,
        ];
        values.extend([6.0e-8, 3.0e-8, 6.1e-5, 5.9e-5, 0.1, -0.1]);
        values.extend((0..60).map(|_| g.between(-40.0, 40.0)));
        values.extend((0..30).map(|_| g.between(-0.001, 0.001)));
        for x in values {
            let (down, up) = (half_down(x), half_up(x));
            assert!(bvh::half_to_f32(down) <= x, "{x}: down {down:#x}");
            assert!(bvh::half_to_f32(up) >= x, "{x}: up {up:#x}");
            // The least such value, by looking at every half.
            let (lo, hi) = (
                bvh::half_to_f32(f32_to_half(x, false)),
                bvh::half_to_f32(f32_to_half(x, true)),
            );
            // `f32_to_half` picks the closest of the halves on the right side,
            // which is the least one.
            assert_eq!(bvh::half_to_f32(down), lo, "{x}");
            assert_eq!(bvh::half_to_f32(up), hi, "{x}");
        }
        assert_eq!(
            (half_down(f32::INFINITY), half_up(f32::INFINITY)),
            (0x7c00, 0x7c00)
        );
        assert_eq!(
            (half_down(f32::NEG_INFINITY), half_up(f32::NEG_INFINITY)),
            (0xfc00, 0xfc00)
        );
        // Exact values do not move.
        assert_eq!(half_down(1.5), half_up(1.5));
        assert_eq!(bvh::half_to_f32(half_up(1.5)), 1.5);
    }

    #[test]
    fn an_empty_input_is_a_header_and_no_root() {
        let b = build(&[], Options::default());
        assert_eq!((b.root, b.nodes(), b.triangles), (NONE, 1, 0));
        assert_eq!(b.words[0], VERSION);
        let r = check(&b.words, b.root).unwrap();
        assert_eq!(r.reached, 0);
        assert_eq!(
            walk(&b, Ray::new([0.0; 3], [0.0, 0.0, 1.0], 10.0), true),
            (None, 0)
        );
        // A triangle with a vertex that is not a number is left out.
        let bad = [[0.0, 0.0, 1.0], [f32::NAN, 0.0, 1.0], [0.0, 1.0, 1.0]];
        let b = build(&[bad], Options::default());
        assert_eq!((b.triangles, b.skipped, b.root), (0, 1, NONE));
    }

    #[test]
    fn small_inputs_make_trees_of_one_to_four_children() {
        for n in 1..=9 {
            for wide_boxes in [false, true] {
                let t = soup(n, 3 + n as u64);
                let b = build(&t, Options { wide_boxes });
                let r = check_strict(&b.words, b.root).unwrap();
                assert_eq!(r.triangles, n, "{n} triangles");
                assert_eq!(r.triangle_nodes, n);
                assert_eq!(r.reached + 1, r.size, "no node is left out");
                assert_eq!(r.loose, 0);
                if n == 1 {
                    assert_eq!(bvh::kind(u64::from(b.root)), 0);
                    assert_eq!(r.box16 + r.box32, 0);
                }
            }
        }
    }

    #[test]
    fn the_closest_hit_is_the_brute_force_one_with_16_bit_boxes() {
        let t = soup(300, 11);
        let (hits, _) = assert_walks_like_brute_force(&t, Options::default(), &rays(400, 5, 10.0));
        assert!(hits > 40, "the rays must hit something: {hits}");
    }

    #[test]
    fn the_closest_hit_is_the_brute_force_one_with_32_bit_boxes() {
        let t = soup(300, 12);
        let (hits, _) =
            assert_walks_like_brute_force(&t, Options { wide_boxes: true }, &rays(400, 6, 10.0));
        assert!(hits > 40, "the rays must hit something: {hits}");
    }

    #[test]
    fn a_walk_tests_far_fewer_nodes_than_there_are_triangles() {
        let t = soup(2000, 21);
        let (hits, tests) =
            assert_walks_like_brute_force(&t, Options::default(), &rays(150, 8, 10.0));
        assert!(hits > 20, "{hits}");
        assert!(tests < 600.0, "{tests} node tests a ray, of 2000 triangles");
    }

    #[test]
    fn rays_along_an_axis_through_corners_and_edges_miss_nothing() {
        let t = grid(12);
        let mut rays = Vec::new();
        // Whole and half coordinates up to 11.5: vertices, edges and the
        // diagonals of the quads. Not 12 and 12.5, the structure's own far
        // faces: a ray parallel to an axis that lies exactly on the maximum
        // face of a box is missed by the node test (`(max - origin) * inverse`
        // is 0 times the clamped infinity: the ray leaves the slab at 0), so
        // a triangle on that face is found by `brute` and not by the walk.
        // Inside the structure a neighbour's box (its minimum face) enters.
        for y in 0..24 {
            for x in 0..24 {
                let o = [x as f32 * 0.5, y as f32 * 0.5, -3.0];
                rays.push(Ray::new(o, [0.0, 0.0, 1.0], 100.0));
                rays.push(Ray::new([o[0], o[1], 9.0], [0.0, 0.0, -1.0], 100.0));
            }
        }
        for wide_boxes in [false, true] {
            let built = build(&t, Options { wide_boxes });
            for ray in &rays {
                let (got, _) = walk(&built, *ray, true);
                let want = brute(&t, ray);
                assert_eq!(
                    got.map(|h| h.t.to_bits()),
                    want.map(|w| w.0.to_bits()),
                    "ray {ray:?}"
                );
            }
        }
    }

    #[test]
    fn triangles_that_share_one_centre_still_split_and_finish() {
        let same: Triangle = [[0.0, 1.0, 5.0], [1.0, -1.0, 5.0], [-1.0, -1.0, 5.0]];
        let mut t = vec![same; 100];
        t.push([[10.0, 1.0, 5.0], [11.0, -1.0, 5.0], [9.0, -1.0, 5.0]]);
        let built = build(&t, Options::default());
        let r = check_strict(&built.words, built.root).unwrap();
        assert_eq!(r.triangles, 101);
        assert!(r.depth < 20, "{}", r.depth);
        let ray = Ray::new([0.0, 0.0, 0.0], [0.0, 0.0, 1.0], 100.0);
        assert_eq!(walk(&built, ray, true).0.map(|h| h.t), Some(5.0));
        let ray = Ray::new([10.0, 0.0, 0.0], [0.0, 0.0, 1.0], 100.0);
        assert_eq!(
            number(&built.words, walk(&built, ray, true).0.unwrap().pointer),
            100
        );
    }

    #[test]
    fn the_extent_shortens_the_walk_and_a_hit_beyond_it_is_not_one() {
        let t = vec![
            [[0.0, 1.0, 5.0], [1.0, -1.0, 5.0], [-1.0, -1.0, 5.0]],
            [[0.0, 1.0, 9.0], [1.0, -1.0, 9.0], [-1.0, -1.0, 9.0]],
        ];
        let built = build(&t, Options::default());
        let hit = |extent| {
            walk(&built, Ray::new([0.0; 3], [0.0, 0.0, 1.0], extent), true)
                .0
                .map(|h| h.t)
        };
        assert_eq!(hit(100.0), Some(5.0));
        assert_eq!(hit(7.0), Some(5.0));
        assert_eq!(hit(3.0), None);
        // From between them, only the far one is ahead.
        let ray = Ray::new([0.0, 0.0, 6.0], [0.0, 0.0, 1.0], 100.0);
        assert_eq!(walk(&built, ray, true).0.map(|h| h.t), Some(3.0));
    }

    #[test]
    fn the_header_says_what_the_structure_is() {
        let t = soup(40, 4);
        let b = build(&t, Options::default());
        assert_eq!(b.words[0], VERSION);
        assert_eq!(b.words[1] as usize + 1, b.nodes());
        let h = Header::read(&b.words).unwrap();
        assert_eq!(
            (h.nodes, h.elements, h.root, h.packed()),
            (b.nodes(), 40, b.root, false)
        );
        assert_eq!(b.words[3], 40);
        assert_eq!(b.words[12], b.root);
        let bounds: Vec<f32> = b.words[4..10].iter().map(|&w| f32::from_bits(w)).collect();
        for tri in &t {
            for v in tri {
                for k in 0..3 {
                    assert!(
                        bounds[k] <= v[k] && v[k] <= bounds[k + 3],
                        "{v:?} is outside {bounds:?}"
                    );
                }
            }
        }
    }

    /// The first box node's pointer and the dword it starts at.
    fn root_at(b: &Built) -> usize {
        (b.root >> 3) as usize * NODE
    }

    #[test]
    fn a_pointer_out_of_the_structure_is_found() {
        let mut b = build(&soup(50, 9), Options::default());
        let at = root_at(&b);
        let end = b.nodes() as u32;
        b.words[at + 1] = end << 3 | BOX16;
        assert!(matches!(check(&b.words, b.root), Err(Fault::Range { .. })));
        // A wide box's second half past the end counts as out as well.
        let mut b = build(&soup(50, 9), Options { wide_boxes: true });
        let last = b.nodes() as u32 - 1;
        let at = root_at(&b);
        b.words[at] = last << 3 | BOX32;
        assert!(matches!(
            check(&b.words, b.root),
            Err(Fault::Range { .. } | Fault::Shared { .. })
        ));
        // Into the header.
        let mut b = build(&soup(50, 9), Options::default());
        let at = root_at(&b);
        b.words[at] = BOX16;
        assert!(matches!(check(&b.words, b.root), Err(Fault::Header { .. })));
    }

    #[test]
    fn a_node_reached_twice_and_a_loop_are_found() {
        let b = build(&soup(50, 9), Options::default());
        let at = root_at(&b);
        let mut twice = b.clone();
        twice.words[at + 1] = twice.words[at];
        assert!(matches!(
            check(&twice.words, twice.root),
            Err(Fault::Shared { .. })
        ));
        let mut round = b.clone();
        // A child that is the root: the walk comes back to it.
        round.words[at] = round.root;
        assert!(matches!(
            check(&round.words, round.root),
            Err(Fault::Shared { .. })
        ));
    }

    #[test]
    fn a_structure_written_over_another_is_found_by_where_they_meet() {
        // Two structures one after the other, the second written from a
        // node of the first: what a copy into a place that was too small
        // does. Walking the first meets the second's nodes.
        let a = build(&soup(60, 31), Options::default());
        let b = build(&soup(60, 32), Options::default());
        let mut memory = a.words.clone();
        let at = a.nodes() - 6;
        memory.truncate(at * NODE);
        memory.extend(&b.words[HEADER..]);
        let size = memory.len() / NODE;
        memory[1] = size as u32 - 1;
        // The first's root still names its nodes; some are now the second's
        // and its pointers lead to places of their own structure.
        let result = check_strict(&memory, a.root);
        assert!(
            result.is_err() || result.unwrap().reached + 1 != size,
            "the overlap is seen as a fault or as a node count that does not add up"
        );
    }

    #[test]
    fn a_box_that_is_too_small_is_loose_and_an_orphan_is_counted() {
        let b = build(&soup(50, 10), Options { wide_boxes: true });
        let at = root_at(&b);
        let mut small = b.clone();
        // Slot 0's maximum x becomes its minimum x less one: it no longer holds what is below.
        let min_x = f32::from_bits(small.words[at + 4]);
        small.words[at + 4 + 3] = (min_x - 1.0).to_bits();
        let report = check(&small.words, small.root).unwrap();
        assert!(report.loose >= 1);
        assert!(check_strict(&small.words, small.root).is_err());
        // A node nothing names: one more at the end.
        let mut more = b.clone();
        more.words.resize(more.words.len() + NODE, 0);
        more.words[1] += 1;
        let report = check(&more.words, more.root).unwrap();
        assert_eq!(report.size, report.reached + 2);
    }
}
