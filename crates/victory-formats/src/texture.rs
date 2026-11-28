//! Texture layouts of the console GPU (RDNA2, GFX10.3): a texture's size and
//! alignment, where each mip and array slice starts, the address of each
//! element, and copies between tiled memory and linear rows.
//!
//! Units: an element is a texel, or a 4x4 block of a block-compressed
//! format. Mips of one array slice (or one depth slab of a thick 3D texture)
//! are stored together, smallest first: the mip tail (the small mips sharing
//! one block) at offset 0, mip 0 last.
//!
//! The algorithm follows AMD's address library (addrlib), as shipped in Mesa
//! under `src/amd/addrlib`: block dimensions, mip placement and mip tails
//! from `gfx10addrlib.cpp` and `addrlib2.cpp`
//! (`ComputeSurfaceInfoMacroTiled`, `ComputeThinBlockDimension`,
//! `GetMipTailDim`, `GetMaxNumMipsInTail`, `HwlComputeSurfaceInfoLinear`,
//! `ComputeSurfaceAddrFromCoordMacroTiled`), and the 256-byte micro tile
//! patterns in `MICRO` from `gfx10SwizzlePattern.h` (the RB+ tables, as on
//! RDNA2). The modes with pipe and bank XOR (`_T`, `_X`) also XOR bits of the
//! block's own coordinates and of the slice into the pipe and bank bits of the
//! address, by a pattern that depends on the GPU's pipes and packers ([`Gpu`]);
//! those patterns are in `xor_patterns.rs`. addrlib carries this notice:
//!
//! Copyright (C) 2007-2026 Advanced Micro Devices, Inc. All rights reserved.
//! SPDX-License-Identifier: MIT
//!
//! Permission is hereby granted, free of charge, to any person obtaining a
//! copy of this software and associated documentation files (the
//! "Software"), to deal in the Software without restriction, including
//! without limitation the rights to use, copy, modify, merge, publish,
//! distribute, sublicense, and/or sell copies of the Software, and to permit
//! persons to whom the Software is furnished to do so, subject to the
//! following conditions: the above copyright notice and this permission
//! notice shall be included in all copies or substantial portions of the
//! Software. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
//! EXPRESS OR IMPLIED.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::xor_patterns as xp;

/// A swizzle mode, by its hardware number (addrlib's `ADDR_SW_*` order).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Swizzle(pub u32);

/// The number and the mode's name, so a log of a description says which
/// mode it was.
impl std::fmt::Debug for Swizzle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Swizzle")
            .field(&self.0)
            .field(&format_args!("{}", self.name()))
            .finish()
    }
}

impl Swizzle {
    pub const LINEAR: Swizzle = Swizzle(0);
    pub const SW_256B_S: Swizzle = Swizzle(1);
    pub const SW_256B_D: Swizzle = Swizzle(2);
    pub const SW_4KB_S: Swizzle = Swizzle(5);
    pub const SW_4KB_D: Swizzle = Swizzle(6);
    pub const SW_64KB_S: Swizzle = Swizzle(9);
    pub const SW_64KB_D: Swizzle = Swizzle(10);
    pub const SW_64KB_S_T: Swizzle = Swizzle(17);
    pub const SW_64KB_D_T: Swizzle = Swizzle(18);
    pub const SW_4KB_S_X: Swizzle = Swizzle(21);
    pub const SW_4KB_D_X: Swizzle = Swizzle(22);
    pub const SW_4KB_R_X: Swizzle = Swizzle(23);
    pub const SW_64KB_Z_X: Swizzle = Swizzle(24);
    pub const SW_64KB_S_X: Swizzle = Swizzle(25);
    pub const SW_64KB_D_X: Swizzle = Swizzle(26);
    pub const SW_64KB_R_X: Swizzle = Swizzle(27);

    /// log2 of the block size in bytes (0 for linear); `None` for modes
    /// GFX10 does not have.
    pub fn block_log2(self) -> Option<u32> {
        Some(match self.0 {
            0 => 0,
            1 | 2 => 8,
            5 | 6 | 21..=23 => 12,
            9 | 10 | 17 | 18 | 24..=27 => 16,
            _ => return None,
        })
    }

    /// The mode's name in addrlib (`SW_64KB_R_X`), `SW_?` for a number
    /// GFX10 has no mode for.
    pub fn name(self) -> &'static str {
        match self.0 {
            0 => "SW_LINEAR",
            1 => "SW_256B_S",
            2 => "SW_256B_D",
            5 => "SW_4KB_S",
            6 => "SW_4KB_D",
            9 => "SW_64KB_S",
            10 => "SW_64KB_D",
            17 => "SW_64KB_S_T",
            18 => "SW_64KB_D_T",
            21 => "SW_4KB_S_X",
            22 => "SW_4KB_D_X",
            23 => "SW_4KB_R_X",
            24 => "SW_64KB_Z_X",
            25 => "SW_64KB_S_X",
            26 => "SW_64KB_D_X",
            27 => "SW_64KB_R_X",
            _ => "SW_?",
        }
    }

    fn standard(self) -> bool {
        matches!(self.0, 1 | 5 | 9 | 17 | 21 | 25)
    }

    fn display(self) -> bool {
        matches!(self.0, 2 | 6 | 10 | 18 | 22 | 26)
    }

    fn z_order(self) -> bool {
        self.0 == 24
    }

    /// Pipe and bank XOR modes (`_T`, `_X`): their addresses depend on the
    /// GPU's pipe configuration.
    pub fn xor(self) -> bool {
        self.0 >= 16
    }
}

/// The GPU's tile configuration: how many pipes and packers its
/// `GB_ADDR_CONFIG` names. The XOR modes' addresses depend on it (the other
/// modes' do not); addrlib has patterns for the RB+ chips' combinations
/// (`pipes` from `packers` to 4 times `packers`, up to 64 pipes).
///
/// shortcut: the console's own values are not known from a source we can
/// check. A Series X is a half of a Navi 21 (4 shader arrays, 8 RB+: 8 pipes,
/// 4 packers) and a Series S a half again (4 pipes, 2 packers), which is what
/// [`Gpu::SERIES_X`] and [`Gpu::SERIES_S`] say. A title whose own code tiles
/// by the hardware's equations would need the right ones; the XG calls the
/// title makes here and the read-backs use the same [`Layout`], so they
/// agree with each other either way. `VICTORY_TILE_GPU=pipes,packers`
/// (victory-gpu) tries another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Gpu {
    /// Index into the table of GPUs (`xor_patterns.rs`).
    index: u8,
}

impl Gpu {
    pub const SERIES_X: Gpu = Gpu { index: 7 };
    pub const SERIES_S: Gpu = Gpu { index: 4 };

    /// From log2 of the pipes and log2 of the packers; `None` when addrlib
    /// has no pattern for that pair.
    pub fn new(pipes_log2: u32, packers_log2: u32) -> Option<Gpu> {
        let index = xp::GPUS
            .iter()
            .position(|&(p, k)| u32::from(p) == pipes_log2 && u32::from(k) == packers_log2)?;
        Some(Gpu { index: index as u8 })
    }

    /// From counts (powers of two): `Gpu::from_counts(8, 4)` is a Series X's.
    pub fn from_counts(pipes: u32, packers: u32) -> Option<Gpu> {
        let log2 = |n: u32| n.is_power_of_two().then_some(n.trailing_zeros());
        Gpu::new(log2(pipes)?, log2(packers)?)
    }

    pub fn pipes_log2(self) -> u32 {
        u32::from(xp::GPUS[self.index as usize].0)
    }

    pub fn packers_log2(self) -> u32 {
        u32::from(xp::GPUS[self.index as usize].1)
    }
}

static DEFAULT_GPU: AtomicU8 = AtomicU8::new(Gpu::SERIES_X.index);

/// Sets the GPU [`Layout::new`] takes: before the title starts.
pub fn set_gpu(gpu: Gpu) {
    DEFAULT_GPU.store(gpu.index, Ordering::Relaxed);
}

/// The GPU [`Layout::new`] takes: a Series X's unless [`set_gpu`] says
/// another.
pub fn gpu() -> Gpu {
    Gpu {
        index: DEFAULT_GPU.load(Ordering::Relaxed),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Tex1d,
    Tex2d,
    Tex3d,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The description makes no texture (zero size, bad element size, a
    /// mode this dimension cannot use, too many mips).
    Invalid,
    /// A valid layout whose addressing is not implemented here.
    Unsupported,
    /// A buffer is smaller than the layout or the rows need.
    TooSmall,
}

/// One plane of a texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Desc {
    pub dimension: Dimension,
    /// In texels.
    pub width: u32,
    pub height: u32,
    /// Depth of a 3D texture, else the array size.
    pub depth: u32,
    pub mips: u32,
    /// 1, 2, 4, 8 or 16.
    pub element_bytes: u32,
    /// Texels per element side: 1, or 4 for block compression.
    pub block: u32,
    pub samples: u32,
    pub swizzle: Swizzle,
}

/// Elements along one side of mip `mip`, as data and copies see it: texels
/// halve rounding down (to at least 1), then round up to whole elements.
fn dim(texels: u32, mip: u32, block: u32) -> u32 {
    (texels >> mip).max(1).div_ceil(block)
}

/// Elements along one side of mip `mip` for placement (padding, tail,
/// offsets): addrlib halves mip 0's elements rounding up, which can be one
/// more than [`dim`] for odd sizes.
fn padded_dim(texels: u32, mip: u32, block: u32) -> u32 {
    texels.max(1).div_ceil(block).div_ceil(1 << mip)
}

/// Where one mip lives inside a slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mip {
    /// Size in elements (depth 1 unless the texture is 3D).
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    /// Padded size in elements: whole blocks, or the tail's share of one.
    pub pitch: u32,
    pub rows: u32,
    pub depth_pitch: u32,
    /// Byte offset of its first block in the slice (0 in the tail).
    pub offset: u64,
    /// In the mip tail: byte offset inside the tail block and the element
    /// position the mip starts at.
    pub tail: Option<Tail>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tail {
    pub offset: u32,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub desc: Desc,
    /// Block dimensions in elements (linear: the pitch alignment, 1, 1).
    pub block: [u32; 3],
    /// 3D blocks span `block[2]` depth slices (GFX10 3D `S` and `D` modes).
    pub thick: bool,
    /// Array slices, depth slabs of `block[2]` slices (thick), or depth
    /// slices (thin and linear 3D).
    pub slices: u32,
    /// Bytes of one slice: its whole mip chain.
    pub slice_size: u64,
    pub size: u64,
    pub alignment: u64,
    /// The first mip in the tail; `mips.len()` without one.
    pub first_tail_mip: u32,
    pub mips: Vec<Mip>,
    /// Byte offsets inside a block for each coordinate inside the block,
    /// per axis. Empty for linear and unsupported modes.
    pattern: &'static SwizzlePattern,
    /// XOR modes: for each axis, the offset bits the block's own coordinate
    /// (the coordinate divided by the block's size) flips. Empty otherwise.
    high: &'static SwizzlePattern,
    /// The GPU the XOR patterns are for.
    gpu: Gpu,
    /// The XOR of the surface's pipe and bank bits (from its base address
    /// on the hardware), already moved to its place in the block.
    pipe_bank: u32,
    /// Bytes from the start of a block that no row-dependent bit of the
    /// offset reaches: a run of columns that long is contiguous whatever
    /// the row.
    row_free: u32,
}

type SwizzlePattern = [Vec<u32>; 3];
static EMPTY_PATTERN: SwizzlePattern = [const { Vec::new() }; 3];

/// Micro tile (256 bytes) patterns by log2 of the element size: for each
/// address bit, the element coordinate bit it takes (`x0`, `y1`, `z0`...),
/// `--` for the bits inside an element. Thin `S`, thin `D`, thick `S`
/// (also the `D` 3D modes). From addrlib's `GFX10_SW_256_S_RBPLUS`,
/// `GFX10_SW_256_D_RBPLUS` and `GFX10_SW_4K_S3_RBPLUS` patterns; the larger
/// blocks add bits Y, X (thin) or Y, X, Z (thick) in turn.
const MICRO: [[&[u8; 16]; 5]; 3] = [
    [
        b"x0x1x2x3y0y1y2y3",
        b"--x0x1x2y0y1y2x3",
        b"----x0x1y0y1y2x2",
        b"------x0y0y1x1x2",
        b"--------y0y1x0x1",
    ],
    [
        b"x0x1x2y1y0y2x3y3",
        b"--x0x1x2y0y1y2x3",
        b"----x0x1y0y1x2y2",
        b"------x0y0x1x2y1",
        b"--------x0y0x1y1",
    ],
    [
        b"x0x1z0y0z1y1x2z2",
        b"--x0z0y0z1y1x1z2",
        b"----x0y0z0y1x1z1",
        b"------x0z0y0x1z1",
        b"--------z0y0x0z1",
    ],
];

/// For each axis, the address bit each coordinate bit lands on.
fn pattern(kind: usize, element_log2: u32, block_log2: u32) -> [Vec<u32>; 3] {
    let mut axes: [Vec<u32>; 3] = Default::default();
    let micro = MICRO[kind][element_log2 as usize];
    for bit in 0..8u32 {
        let (axis, index) = (micro[2 * bit as usize], micro[2 * bit as usize + 1]);
        let axis = match axis {
            b'x' => 0,
            b'y' => 1,
            b'z' => 2,
            _ => continue,
        };
        let index = usize::from(index - b'0');
        let to = &mut axes[axis];
        if to.len() <= index {
            to.resize(index + 1, 0);
        }
        to[index] = bit;
    }
    let order: &[usize] = if kind == 2 { &[1, 0, 2] } else { &[1, 0] };
    for (bit, axis) in (8..block_log2).zip(order.iter().cycle()) {
        axes[*axis].push(bit);
    }
    axes
}

/// Byte offsets inside a block for coordinates 0..2^bits of one axis.
fn spread(bits: &[u32]) -> Vec<u32> {
    (0..1u32 << bits.len())
        .map(|c| {
            bits.iter()
                .enumerate()
                .map(|(i, &to)| ((c >> i) & 1) << to)
                .sum()
        })
        .collect()
}

/// Tables depend only on the micro-tile kind, block size and element size.
/// Texture extents, mip placement and array slices only affect the base
/// address, so layouts with the same key can share these immutable tables.
fn shared_pattern(kind: usize, element_log2: u32, block_log2: u32) -> &'static SwizzlePattern {
    // Indexed by MICRO kind, block size (256 B, 4 KiB, 64 KiB), then element size.
    static PATTERNS: [[[OnceLock<SwizzlePattern>; 5]; 3]; 3] =
        [const { [const { [const { OnceLock::new() }; 5] }; 3] }; 3];
    let block_index = ((block_log2 - 8) / 4) as usize;
    PATTERNS[kind][block_index][element_log2 as usize]
        .get_or_init(|| pattern(kind, element_log2, block_log2).map(|bits| spread(&bits)))
}

/// The table of `xor_patterns.rs` for a XOR mode, `None` for the modes it
/// has none for (`SW_4KB_R_X`, and `D`/`D_T` in 3D, which GFX10 lacks).
fn xor_table(swizzle: Swizzle, three_d: bool) -> Option<usize> {
    Some(match (swizzle.0, three_d) {
        (21, false) => 0,
        (22, false) => 1,
        (17, false) => 2,
        (18, false) => 3,
        (25, false) => 4,
        (26, false) => 5,
        (27, _) => 6,
        (24, _) => 7,
        (21, true) => 8,
        (17, true) => 9,
        (25, true) => 10,
        (26, true) => 11,
        _ => return None,
    })
}

/// A XOR mode's offset tables: for each axis, the offset of each coordinate
/// inside the block (`low`, as for the other modes, but XOR-combined with
/// the other axes' rather than added), and the offset bits the block's own
/// coordinate flips (`high`, indexed by the coordinate divided by the block's
/// size, its upper bits ignored: no pattern reaches them).
struct XorPattern {
    low: SwizzlePattern,
    high: SwizzlePattern,
}

/// The pattern text of `table` (see `xor_patterns.rs`) for one GPU and
/// element size, as the tables of a block of `block` elements.
fn xor_pattern(gpu: Gpu, table: usize, element_log2: u32, block: [u32; 3]) -> &'static XorPattern {
    const LEN: usize = xp::GPUS.len() * xp::TABLES * 5;
    static PATTERNS: [OnceLock<XorPattern>; LEN] = [const { OnceLock::new() }; LEN];
    let slot = (gpu.index as usize * xp::TABLES + table) * 5 + element_log2 as usize;
    PATTERNS[slot].get_or_init(|| {
        let text = xp::PATTERNS
            [usize::from(xp::INDEX[gpu.index as usize][table * 5 + element_log2 as usize])];
        // For each axis and coordinate bit, the offset bits it flips.
        let mut flips = [[0u32; 16]; 3];
        for (bit, entry) in text.split(',').enumerate() {
            let mut chars = entry.bytes().peekable();
            while let Some(axis) = chars.next() {
                let axis = match axis {
                    b'x' => 0,
                    b'y' => 1,
                    b'z' => 2,
                    _ => break,
                };
                let mut coordinate = 0usize;
                while let Some(digit) = chars.next_if(u8::is_ascii_digit) {
                    coordinate = coordinate * 10 + usize::from(digit - b'0');
                }
                flips[axis][coordinate] |= 1 << bit;
            }
        }
        let mut low: SwizzlePattern = Default::default();
        let mut high: SwizzlePattern = Default::default();
        for axis in 0..3 {
            let below = block[axis].trailing_zeros() as usize;
            low[axis] = (0..1usize << below)
                .map(|c| {
                    (0..below)
                        .filter(|b| (c >> b) & 1 != 0)
                        .fold(0, |all, b| all ^ flips[axis][b])
                })
                .collect();
            let above = flips[axis]
                .iter()
                .rposition(|&f| f != 0)
                .map_or(0, |last| last + 1);
            let above = above.saturating_sub(below);
            high[axis] = (0..1usize << above)
                .map(|v| {
                    (0..above)
                        .filter(|j| (v >> j) & 1 != 0)
                        .fold(0, |all, j| all ^ flips[axis][below + j])
                })
                .collect();
        }
        XorPattern { low, high }
    })
}

const MAX_MIPS: u32 = 16;

/// addrlib's `ColumnBits` and `BankBits`: pipe-bank XOR bits above the pipes.
const COLUMN_BITS: u32 = 2;
const BANK_BITS: u32 = 4;

/// The largest width, height and depth (or array size) accepted, in texels.
/// GFX10's image descriptors hold at most 16384 texels per side, so no
/// texture the GPU can use is refused. The bound also keeps every size
/// computed below within `u64` (and its `u32` steps), which unchecked
/// values from a title would otherwise overflow.
const MAX_EXTENT: u32 = 1 << 14;

/// 256-byte micro tile dimensions by log2 element size, 2D and 3D.
const BLOCK_256_2D: [[u32; 2]; 5] = [[16, 16], [16, 8], [8, 8], [8, 4], [4, 4]];
const BLOCK_256_3D: [[u32; 3]; 5] = [[8, 4, 8], [4, 4, 8], [4, 4, 4], [4, 2, 4], [2, 2, 4]];
const BLOCK_1K_3D: [[u32; 3]; 5] = [[16, 8, 8], [8, 8, 8], [8, 8, 4], [8, 4, 4], [4, 4, 4]];

impl Layout {
    /// The layout on the GPU [`gpu`] names.
    pub fn new(desc: &Desc) -> Result<Layout, Error> {
        Layout::with_gpu(desc, gpu())
    }

    /// The layout on `gpu` (only the XOR modes differ between GPUs).
    pub fn with_gpu(desc: &Desc, gpu: Gpu) -> Result<Layout, Error> {
        let d = *desc;
        let block_log2 = d.swizzle.block_log2().ok_or(Error::Invalid)?;
        let valid = (1..=MAX_EXTENT).contains(&d.width)
            && (1..=MAX_EXTENT).contains(&d.height)
            && (1..=MAX_EXTENT).contains(&d.depth)
            && (1..=MAX_MIPS).contains(&d.mips)
            && d.element_bytes.is_power_of_two()
            && d.element_bytes <= 16
            && matches!(d.block, 1 | 4)
            && d.samples.is_power_of_two()
            && d.samples <= 8
            && (d.samples == 1
                || (d.dimension == Dimension::Tex2d && d.mips == 1 && d.swizzle != Swizzle::LINEAR))
            && (d.dimension != Dimension::Tex1d || d.height == 1)
            && (d.dimension != Dimension::Tex3d
                || matches!(d.swizzle.0, 0 | 5 | 9 | 17 | 21 | 24..=27));
        if !valid {
            return Err(Error::Invalid);
        }
        if block_log2 == 0 {
            Ok(Layout::linear(d))
        } else {
            Ok(Layout::tiled(d, block_log2, gpu))
        }
    }

    /// The layout of a surface whose base address carries the pipe and bank
    /// XOR `value` (addrlib's `pipeBankXor`): the XOR modes only, others
    /// are unchanged. Zero for the 64 KiB-aligned surfaces a title makes.
    pub fn with_pipe_bank_xor(mut self, value: u32) -> Layout {
        let block_log2 = self.alignment.trailing_zeros();
        if self.desc.swizzle.xor() && block_log2 > 8 {
            let pipes = self.gpu.pipes_log2();
            let bank_bits = block_log2
                .saturating_sub(8 + pipes + COLUMN_BITS)
                .min(BANK_BITS);
            let mask = ((1 << pipes) - 1) | (((1 << bank_bits) - 1) << (pipes + COLUMN_BITS));
            self.pipe_bank = ((value & mask) << 8) & ((1 << block_log2) - 1);
            self.row_free = self.row_free_bytes();
        }
        self
    }

    /// Bytes from the start of a block that no bit of an offset which
    /// depends on the row (or slice) reaches.
    fn row_free_bytes(&self) -> u32 {
        let rows = self.pattern[1]
            .iter()
            .chain(&self.pattern[2])
            .chain(&self.high[1])
            .chain(&self.high[2])
            .fold(self.pipe_bank, |all, &bits| all | bits);
        if rows == 0 {
            self.alignment as u32
        } else {
            1 << rows.trailing_zeros()
        }
    }

    /// `HwlComputeSurfaceInfoLinear`: rows aligned to 256 bytes, mips packed
    /// smallest first, a 3D texture's depth slices each hold the chain.
    fn linear(d: Desc) -> Layout {
        let bpe = u64::from(d.element_bytes);
        let align = 256 / d.element_bytes;
        let three_d = d.dimension == Dimension::Tex3d;
        let mut mips = vec![];
        let mut offset = 0;
        for i in (0..d.mips).rev() {
            let h = padded_dim(d.height, i, d.block);
            let pitch = padded_dim(d.width, i, d.block).next_multiple_of(align);
            mips.push(Mip {
                width: dim(d.width, i, d.block),
                height: dim(d.height, i, d.block),
                depth: if three_d { dim(d.depth, i, 1) } else { 1 },
                pitch,
                rows: h,
                depth_pitch: if three_d { d.depth } else { 1 },
                offset,
                tail: None,
            });
            offset += u64::from(pitch) * u64::from(h) * bpe;
        }
        mips.reverse();
        Layout {
            desc: d,
            block: [align, 1, 1],
            thick: false,
            slices: d.depth,
            slice_size: offset,
            size: offset * u64::from(d.depth),
            alignment: 256,
            first_tail_mip: d.mips,
            mips,
            pattern: &EMPTY_PATTERN,
            high: &EMPTY_PATTERN,
            gpu: Gpu::SERIES_X,
            pipe_bank: 0,
            row_free: 256,
        }
    }

    fn tiled(d: Desc, block_log2: u32, gpu: Gpu) -> Layout {
        let element_log2 = d.element_bytes.trailing_zeros();
        let bpe = u64::from(d.element_bytes);
        let block_bytes = 1u64 << block_log2;
        let thick = d.dimension == Dimension::Tex3d && (d.swizzle.standard() || d.swizzle.display());
        let block = if thick {
            // ComputeThickBlockDimension
            let amp = (block_log2 - 10) / 3;
            let rest = (block_log2 - 10) % 3;
            let b = BLOCK_1K_3D[element_log2 as usize];
            [b[0] << amp, b[1] << (amp + rest / 2), b[2] << (amp + u32::from(rest != 0))]
        } else {
            // ComputeThinBlockDimension
            let samples_log2 = d.samples.trailing_zeros();
            let elements_log2 = block_log2 - element_log2 - samples_log2;
            let wide = samples_log2 & 1 == 0 || block_log2 & 1 != 0;
            let w = (elements_log2 + u32::from(wide)) / 2;
            [1 << w, 1 << (elements_log2 - w), 1]
        };
        let slices = d.depth.next_multiple_of(block[2]);
        let three_d = d.dimension == Dimension::Tex3d;
        // Mip depths placed by a thick layout (thin ones store depth as
        // slices).
        let mip_depth = |i: u32| if thick { padded_dim(d.depth, i, 1) } else { 1 };
        let depth_of = |i: u32| if three_d { dim(d.depth, i, 1) } else { 1 };
        let (width, height) = (|i| dim(d.width, i, d.block), |i| dim(d.height, i, d.block));
        let padded = |i| (padded_dim(d.width, i, d.block), padded_dim(d.height, i, d.block));
        let mut mips = Vec::with_capacity(d.mips as usize);
        let mut first_tail_mip = d.mips;
        let mut slice_size = 0u64;
        if d.mips == 1 {
            mips.push(Mip {
                width: width(0),
                height: height(0),
                depth: depth_of(0),
                pitch: width(0).next_multiple_of(block[0]),
                rows: height(0).next_multiple_of(block[1]),
                depth_pitch: if three_d { slices } else { 1 },
                offset: 0,
                tail: None,
            });
            slice_size = u64::from(mips[0].pitch)
                * u64::from(mips[0].rows)
                * bpe
                * u64::from(d.samples);
        } else {
            // GetMipTailDim: the largest mip the tail takes.
            let mut tail_max = block;
            if thick {
                match block_log2 % 3 {
                    0 => tail_max[1] >>= 1,
                    1 => tail_max[0] >>= 1,
                    _ => tail_max[2] >>= 1,
                }
            } else {
                tail_max[0] >>= 1;
            }
            // GetMaxNumMipsInTail; 256-byte blocks have no tail
            // (`ComputeSurfaceInfoMicroTiled`).
            let effective = if thick {
                block_log2 - (block_log2 - 8) / 3
            } else {
                block_log2
            };
            let max_in_tail = match effective {
                8 => 0,
                9..=11 => 1 + (1 << (effective - 9)),
                _ => effective - 4,
            };
            // dsMipmapHtileFix: 8- and 16-bit depth enter the tail as 32-bit.
            let mut fixed = tail_max;
            if d.swizzle.z_order() && element_log2 <= 1 {
                let b = BLOCK_256_2D[element_log2 as usize];
                fixed[0] /= b[0] / BLOCK_256_2D[2][0];
                fixed[1] /= b[1] / BLOCK_256_2D[2][1];
            }
            let mut sizes = vec![];
            for i in 0..d.mips {
                let (w, h) = padded(i);
                if w <= fixed[0] && h <= fixed[1] && d.mips - i <= max_in_tail {
                    first_tail_mip = i;
                    slice_size += block_bytes / u64::from(block[2]);
                    break;
                }
                let pitch = w.next_multiple_of(block[0]);
                let rows = h.next_multiple_of(block[1]);
                let depth = mip_depth(i).next_multiple_of(block[2]);
                let one = u64::from(pitch) * u64::from(rows) * bpe;
                slice_size += one;
                sizes.push(one * u64::from(block[2]));
                mips.push(Mip {
                    width: width(i),
                    height: height(i),
                    depth: depth_of(i),
                    pitch,
                    rows,
                    depth_pitch: if three_d { depth } else { 1 },
                    offset: 0,
                    tail: None,
                });
            }
            // Standard mips from the smallest up, after the tail block.
            let mut offset = if first_tail_mip < d.mips {
                block_bytes
            } else {
                0
            };
            for i in (0..first_tail_mip as usize).rev() {
                mips[i].offset = offset;
                offset += sizes[i];
            }
            let micro = if thick {
                let b = BLOCK_256_3D[element_log2 as usize];
                [b[0], b[1]]
            } else {
                BLOCK_256_2D[element_log2 as usize]
            };
            let (mut pitch, mut rows) = (tail_max[0], tail_max[1]);
            for i in first_tail_mip..d.mips {
                let m = max_in_tail - 1 - (i - first_tail_mip);
                let offset = if m > 6 { 16 << m } else { m << 8 };
                // Address bits 8.. of the offset, odd ones x, even ones y.
                let bits = |from: u32| (0..6).map(|b| ((offset >> (from + 2 * b)) & 1) << b).sum();
                let (mut x, mut y): (u32, u32) = (bits(9), bits(8));
                if block_log2 & 1 != 0 {
                    std::mem::swap(&mut x, &mut y);
                    if element_log2 & 1 != 0 {
                        y = (y << 1) | (x & 1);
                        x >>= 1;
                    }
                }
                mips.push(Mip {
                    width: width(i),
                    height: height(i),
                    depth: depth_of(i),
                    pitch,
                    rows,
                    depth_pitch: if thick {
                        mip_depth(i).next_multiple_of(BLOCK_256_3D[element_log2 as usize][2])
                    } else {
                        1
                    },
                    offset: 0,
                    tail: Some(Tail {
                        offset,
                        x: x * micro[0],
                        y: y * micro[1],
                    }),
                });
                pitch = (pitch >> 1).max(micro[0]);
                rows = (rows >> 1).max(micro[1]);
            }
        }
        // Thick layouts store each depth slab's chain together.
        let slab = if thick { u64::from(block[2]) } else { 1 };
        let slice_size = slice_size * slab;
        let slices = slices / block[2];
        let (pattern, high) = if d.samples != 1 {
            (&EMPTY_PATTERN, &EMPTY_PATTERN)
        } else if !d.swizzle.xor() {
            let kind = if thick {
                2
            } else {
                usize::from(d.swizzle.display())
            };
            (
                shared_pattern(kind, element_log2, block_log2),
                &EMPTY_PATTERN,
            )
        } else if let Some(table) = xor_table(d.swizzle, three_d) {
            let xor = xor_pattern(gpu, table, element_log2, block);
            (&xor.low, &xor.high)
        } else {
            (&EMPTY_PATTERN, &EMPTY_PATTERN)
        };
        let mut layout = Layout {
            desc: d,
            block,
            thick,
            slices,
            slice_size,
            size: slice_size * u64::from(slices),
            alignment: block_bytes,
            first_tail_mip,
            mips,
            pattern,
            high,
            gpu,
            pipe_bank: 0,
            row_free: 0,
        };
        layout.row_free = layout.row_free_bytes();
        layout
    }

    /// Whether [`Layout::offset`] and the copies work for this layout:
    /// linear and every tiled mode but `SW_4KB_R_X` (addrlib has no pattern
    /// for it), single-sampled.
    ///
    /// shortcut: no addressing for MSAA (the sample bits are not in the
    /// patterns used here).
    pub fn addressable(&self) -> bool {
        self.desc.swizzle == Swizzle::LINEAR || !self.pattern[0].is_empty()
    }

    /// The slice and the depth inside it for `z` (an array slice or a 3D
    /// depth slice).
    fn split(&self, z: u32) -> (u32, u32) {
        if self.thick {
            (z / self.block[2], z % self.block[2])
        } else {
            (z, 0)
        }
    }

    /// Byte offset of element (`x`, `y`) of mip `mip` in array slice or
    /// depth slice `z`. Coordinates must lie inside the mip.
    pub fn offset(&self, mip: u32, x: u32, y: u32, z: u32) -> Result<u64, Error> {
        let m = self.mips.get(mip as usize).ok_or(Error::Invalid)?;
        let depth = if self.desc.dimension == Dimension::Tex3d {
            m.depth
        } else {
            self.slices
        };
        let inside = x < m.width && y < m.height && z < depth;
        if !inside || !self.addressable() {
            return Err(if inside {
                Error::Unsupported
            } else {
                Error::Invalid
            });
        }
        let (slice, z) = self.split(z);
        let base = u64::from(slice) * self.slice_size + m.offset;
        if self.desc.swizzle == Swizzle::LINEAR {
            let row = u64::from(y) * u64::from(m.pitch) + u64::from(x);
            return Ok(base + row * u64::from(self.desc.element_bytes));
        }
        let (row_blocks, row_bits) = self.row_part(m, y, z, slice);
        let (column_blocks, column_bits) = self.col_part(m, x);
        Ok(base + row_blocks + column_blocks + u64::from(row_bits ^ column_bits))
    }

    /// What the block's own coordinate on `axis` flips in an offset (XOR
    /// modes; nothing for the others).
    fn flips(&self, axis: usize, coordinate: u32) -> u32 {
        let table = &self.high[axis];
        if table.is_empty() {
            0
        } else {
            table[coordinate as usize & (table.len() - 1)]
        }
    }

    /// The part of an address that depends on the row, in a thick block the
    /// depth inside it, and in the XOR modes the slice: the byte offset of
    /// the row's blocks, and the offset bits inside a block (which the
    /// column's are XORed with, they overlap in the XOR modes).
    fn row_part(&self, m: &Mip, y: u32, z: u32, slice: u32) -> (u64, u32) {
        let [bw, bh, _] = self.block;
        let log2 = self.alignment.trailing_zeros();
        // In the tail the pattern of the shifted position gives the mip's
        // offset inside the tail block.
        let y = y + m.tail.map_or(0, |t| t.y);
        let row = y / bh;
        let blocks = u64::from(row) * u64::from(m.pitch / bw);
        let bits = self.pattern[1][(y % bh) as usize]
            ^ self.pattern[2][z as usize]
            ^ self.flips(1, row)
            ^ self.flips(2, slice)
            ^ self.pipe_bank;
        (blocks << log2, bits)
    }

    /// The part of an address that depends on the column, as
    /// [`Layout::row_part`].
    fn col_part(&self, m: &Mip, x: u32) -> (u64, u32) {
        let bw = self.block[0];
        let x = x + m.tail.map_or(0, |t| t.x);
        let column = x / bw;
        (
            u64::from(column) << self.alignment.trailing_zeros(),
            self.pattern[0][(x % bw) as usize] ^ self.flips(0, column),
        )
    }

    /// The row's part of an address as one number, for the modes where it
    /// does not overlap the column's.
    fn row(&self, m: &Mip, y: u32, z: u32, slice: u32) -> u64 {
        let (blocks, bits) = self.row_part(m, y, z, slice);
        blocks + u64::from(bits)
    }

    /// The column's part of an address as one number: the block's offset
    /// plus the offset bits inside it.
    fn column(&self, m: &Mip, x: u32) -> u64 {
        let (blocks, bits) = self.col_part(m, x);
        blocks + u64::from(bits)
    }

    /// Depth slices a subresource of mip `mip` covers: the mip's depth for
    /// 3D textures, else 1 (one array slice).
    pub fn subresource_depth(&self, mip: u32) -> u32 {
        self.mips.get(mip as usize).map_or(0, |m| m.depth)
    }

    /// The bytes from the texture's start that mip `first` and the smaller
    /// ones take: the smaller a mip, the nearer the start it lies, so a
    /// title that streams textures has memory for the smaller mips only and
    /// another texture right behind them. All of the layout where slices
    /// follow each other (each holds its whole chain).
    pub fn bytes_from(&self, first: u32) -> u64 {
        let whole = first == 0
            || self.slices != 1
            || self.thick
            || self.desc.dimension == Dimension::Tex3d
            || self.desc.samples != 1
            || first >= self.desc.mips;
        if whole {
            return self.size;
        }
        self.mips[first as usize..]
            .iter()
            .map(|m| match m.tail {
                Some(_) => self.alignment,
                None => m.offset + u64::from(m.pitch) * u64::from(m.rows) * u64::from(self.desc.element_bytes),
            })
            .max()
            .unwrap_or(self.size)
            .min(self.size)
    }

    /// Copies one subresource out of tiled memory (the whole texture,
    /// [`Layout::size`] bytes, or the [`Layout::bytes_from`] of the mip)
    /// into linear rows: mip `mip` of array slice
    /// `slice` (3D: every depth slice of the mip, `slice` 0). Rows are
    /// `row_pitch` bytes apart, depth slices `depth_pitch`.
    pub fn detile(
        &self,
        tiled: &[u8],
        mip: u32,
        slice: u32,
        linear: &mut [u8],
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        self.walk::<true>(tiled, linear, mip, slice, row_pitch, depth_pitch)
    }

    /// The reverse of [`Layout::detile`]: linear rows into tiled memory.
    pub fn tile(
        &self,
        tiled: &mut [u8],
        mip: u32,
        slice: u32,
        linear: &[u8],
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        self.walk::<false>(linear, tiled, mip, slice, row_pitch, depth_pitch)
    }

    /// Copies contiguous runs in one direction between the two layouts.
    #[allow(clippy::too_many_arguments)]
    fn walk<const DETILE: bool>(
        &self,
        src: &[u8],
        dst: &mut [u8],
        mip: u32,
        slice: u32,
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        let m = self.mips.get(mip as usize).ok_or(Error::Invalid)?;
        let depth = m.depth;
        let first = if self.desc.dimension == Dimension::Tex3d {
            if slice != 0 {
                return Err(Error::Invalid);
            }
            0
        } else {
            slice
        };
        if first >= self.slices {
            return Err(Error::Invalid);
        }
        if !self.addressable() {
            return Err(Error::Unsupported);
        }
        let n = self.desc.element_bytes as usize;
        let bytes = m.width as usize * n;
        let slice_bytes = (m.height as usize - 1)
            .checked_mul(row_pitch)
            .and_then(|offset| offset.checked_add(bytes))
            .ok_or(Error::TooSmall)?;

        // Multiple depth slices must not overwrite each other.
        if depth > 1 && depth_pitch < slice_bytes {
            return Err(Error::TooSmall);
        }

        let need = (depth as usize - 1)
            .checked_mul(depth_pitch)
            .and_then(|offset| offset.checked_add(slice_bytes))
            .ok_or(Error::TooSmall)?;
        let (tiled_len, linear_len) = if DETILE {
            (src.len(), dst.len())
        } else {
            (dst.len(), src.len())
        };
        let tiled_need = if DETILE { self.bytes_from(mip) } else { self.size };
        if tiled_len < tiled_need as usize || linear_len < need || row_pitch < bytes {
            return Err(Error::TooSmall);
        }
        let linear = self.desc.swizzle == Swizzle::LINEAR;
        // The row and depth contribute one common base to every column, so
        // the same contiguous runs repeat on every row, including mip tails.
        let mut runs = Vec::new();
        if !linear {
            let mut first_linear = 0;
            let mut first_tiled = self.column(m, 0) as usize;
            for x in 1..m.width {
                let offset = self.column(m, x) as usize;
                let at = x as usize * n;
                if offset != first_tiled + (at - first_linear) {
                    runs.push((first_tiled, first_linear, at - first_linear));
                    first_tiled = offset;
                    first_linear = at;
                }
            }
            runs.push((first_tiled, first_linear, bytes - first_linear));
        }
        // XOR modes: the row's offset bits are XORed into the column's, which
        // is the same as adding them for the run's bytes only when no row bit
        // is among the bits that vary inside the run (they sit above the
        // elements' columns of the micro tile in every pattern, so this
        // splits nothing but, perhaps, a long run starting off its length).
        let xor = self.desc.swizzle.xor();
        let block_mask = self.alignment as usize - 1;
        if xor {
            let free = self.row_free as usize;
            let mut split = Vec::with_capacity(runs.len());
            for &(column, at, len) in &runs {
                let span = len.next_power_of_two();
                if span <= free && (column & block_mask).is_multiple_of(span) {
                    split.push((column, at, len));
                } else {
                    split.extend(
                        (0..len)
                            .step_by(n)
                            .map(|k| (column + k, at + k, n.min(len - k))),
                    );
                }
            }
            runs = split;
        }
        // A mip can end partway through the last run. Check the other runs
        // once so each row can copy them as fixed-size chunks.
        let (body, last) = runs.split_at(runs.len().saturating_sub(1));
        let chunk_size = body.first().map(|run| run.2).filter(|&size| {
            body.iter()
                .all(|&(offset, _, len)| len == size && offset.is_multiple_of(size))
        });
        for z in 0..depth {
            let (s, inner) = self.split(first + z);
            let base = u64::from(s) * self.slice_size + m.offset;
            for y in 0..m.height {
                let linear_row = z as usize * depth_pitch + y as usize * row_pitch;
                if linear {
                    let tiled_row = base + u64::from(y) * u64::from(m.pitch) * n as u64;
                    copy_run::<DETILE>(src, dst, tiled_row as usize, linear_row, bytes);
                } else if xor {
                    let (blocks, bits) = self.row_part(m, y, inner, s);
                    let row = (base + blocks) as usize;
                    for &(column, at, len) in &runs {
                        let tiled =
                            row + (column & !block_mask) + ((column & block_mask) ^ bits as usize);
                        copy_run::<DETILE>(src, dst, tiled, linear_row + at, len);
                    }
                } else {
                    let tiled_row = (base + self.row(m, y, inner, s)) as usize;
                    match chunk_size {
                        Some(8) if tiled_row.is_multiple_of(8) => {
                            copy_runs::<8, DETILE>(src, dst, body, tiled_row, linear_row)
                        }
                        Some(16) if tiled_row.is_multiple_of(16) => {
                            copy_runs::<16, DETILE>(src, dst, body, tiled_row, linear_row)
                        }
                        Some(32) if tiled_row.is_multiple_of(32) => {
                            copy_runs::<32, DETILE>(src, dst, body, tiled_row, linear_row)
                        }
                        _ => copy_runs::<0, DETILE>(src, dst, body, tiled_row, linear_row),
                    }
                    copy_runs::<0, DETILE>(src, dst, last, tiled_row, linear_row);
                }
            }
        }
        Ok(())
    }
}

fn copy_run<const DETILE: bool>(
    src: &[u8],
    dst: &mut [u8],
    tiled: usize,
    linear: usize,
    len: usize,
) {
    let (from, to) = if DETILE {
        (tiled, linear)
    } else {
        (linear, tiled)
    };
    dst[to..][..len].copy_from_slice(&src[from..][..len]);
}

/// A row of equal runs is contiguous in linear memory. Treat it as a
/// slice of arrays so only the tiled chunk needs a bounds check per copy.
/// Use the byte ranges for irregular runs and the last run of each row.
fn copy_runs<const N: usize, const DETILE: bool>(
    src: &[u8],
    dst: &mut [u8],
    runs: &[(usize, usize, usize)],
    tiled_row: usize,
    linear_row: usize,
) {
    if N == 0 {
        for &(tiled, linear, len) in runs {
            copy_run::<DETILE>(src, dst, tiled_row + tiled, linear_row + linear, len);
        }
    } else if DETILE {
        let tiled = src.as_chunks::<N>().0;
        let linear = dst[linear_row..][..runs.len() * N].as_chunks_mut::<N>().0;
        for (out, &(offset, ..)) in linear.iter_mut().zip(runs) {
            *out = tiled[(tiled_row + offset) / N];
        }
    } else {
        let tiled = dst.as_chunks_mut::<N>().0;
        let linear = src[linear_row..][..runs.len() * N].as_chunks::<N>().0;
        for (input, &(offset, ..)) in linear.iter().zip(runs) {
            tiled[(tiled_row + offset) / N] = *input;
        }
    }
}

/// What a DXGI format stores per element: bytes, texels per element side
/// (4 for block compression), and whether its stencil is a second plane
/// (the GPU keeps depth and stencil apart).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub bytes: u32,
    pub block: u32,
    pub stencil: bool,
}

/// By DXGI format number. `None` for 96-bit, 1-bit, packed 4:2:2 and video
/// formats.
///
/// shortcut: those formats have no layout here; add them when a title uses
/// one.
pub fn format(dxgi: u32) -> Option<Format> {
    let (bytes, block, stencil) = match dxgi {
        1..=4 => (16, 1, false),
        9..=18 => (8, 1, false),
        19..=22 | 44..=47 => (4, 1, true),
        23..=43 | 67 | 87..=93 | 116 | 117 => (4, 1, false),
        48..=59 | 85 | 86 | 115 => (2, 1, false),
        118..=120 => (2, 1, true),
        60..=65 => (1, 1, false),
        70..=72 | 79..=81 => (8, 4, false),
        73..=78 | 82..=84 | 94..=99 => (16, 4, false),
        _ => return None,
    };
    Some(Format {
        bytes,
        block,
        stencil,
    })
}

/// A texture as the console allocates it: a D3D12-style description with
/// a swizzle mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureDesc {
    pub dimension: Dimension,
    /// In texels.
    pub width: u32,
    pub height: u32,
    /// Depth of a 3D texture, else the array size.
    pub depth_or_array: u32,
    /// 0: the full chain.
    pub mips: u32,
    /// DXGI format number.
    pub format: u32,
    pub samples: u32,
    pub swizzle: Swizzle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    /// From the start of the texture.
    pub offset: u64,
    pub stencil: bool,
    pub layout: Layout,
}

/// Every data plane of a texture (depth, then stencil), placed one after
/// the other.
///
/// shortcut: no compression metadata planes (HTILE, CMASK, FMASK, DCC);
/// the console adds them to render targets and depth buffers unless the
/// description denies them, so those are smaller here than on the console.
/// The host GPU renders them, so nothing reads that memory; add the planes
/// if a title sizes a pool from console sizes and runs out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Texture {
    pub planes: Vec<Plane>,
    pub size: u64,
    pub alignment: u64,
}

impl Texture {
    pub fn new(desc: &TextureDesc) -> Result<Texture, Error> {
        let f = format(desc.format).ok_or(Error::Invalid)?;
        let depth3d = if desc.dimension == Dimension::Tex3d {
            desc.depth_or_array
        } else {
            1
        };
        let mips = match desc.mips {
            0 => 32 - desc.width.max(desc.height).max(depth3d).max(1).leading_zeros(),
            n => n,
        };
        let plane = |bytes| Desc {
            dimension: desc.dimension,
            width: desc.width,
            height: desc.height,
            depth: desc.depth_or_array,
            mips,
            element_bytes: bytes,
            block: f.block,
            samples: desc.samples.max(1),
            swizzle: desc.swizzle,
        };
        let mut planes = vec![Plane {
            offset: 0,
            stencil: false,
            layout: Layout::new(&plane(f.bytes))?,
        }];
        if f.stencil {
            let layout = Layout::new(&plane(1))?;
            let offset = planes[0].layout.size.next_multiple_of(layout.alignment);
            planes.push(Plane {
                offset,
                stencil: true,
                layout,
            });
        }
        let last = &planes[planes.len() - 1];
        let size = last.offset + last.layout.size;
        let alignment = planes.iter().map(|p| p.layout.alignment).max().unwrap_or(1);
        Ok(Texture {
            planes,
            size,
            alignment,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(w: u32, h: u32, mips: u32, bpe: u32, swizzle: Swizzle) -> Desc {
        Desc {
            dimension: Dimension::Tex2d,
            width: w,
            height: h,
            depth: 1,
            mips,
            element_bytes: bpe,
            block: 1,
            samples: 1,
            swizzle,
        }
    }

    #[test]
    fn linear_layouts_require_one_sample() {
        let linear = desc(256, 256, 1, 4, Swizzle::LINEAR);
        let single = Layout::new(&linear).unwrap();
        assert!(single.addressable());
        assert_eq!(single.size, 256 * 256 * 4);

        for samples in [2, 4, 8] {
            let multisampled = Desc { samples, ..linear };
            assert_eq!(Layout::new(&multisampled), Err(Error::Invalid));

            let tiled = Layout::new(&Desc {
                swizzle: Swizzle::SW_64KB_R_X,
                ..multisampled
            })
            .unwrap();
            assert_eq!(tiled.size, single.size * u64::from(samples));
        }
    }

    /// Sizes past the hardware's 16384 texels per side are refused instead
    /// of overflowing the size arithmetic (found by random-input testing).
    #[test]
    fn extents_past_the_hardware_limit_are_invalid() {
        for swizzle in [Swizzle::LINEAR, Swizzle::SW_64KB_D_X] {
            let largest = Layout::new(&desc(MAX_EXTENT, MAX_EXTENT, 15, 16, swizzle)).unwrap();
            assert!(largest.size >= u64::from(MAX_EXTENT) * u64::from(MAX_EXTENT) * 16);
            for (w, h) in [
                (MAX_EXTENT + 1, 1),
                (1, MAX_EXTENT + 1),
                (u32::MAX, u32::MAX),
            ] {
                let result = Layout::new(&desc(w, h, 1, 16, swizzle));
                assert_eq!(result, Err(Error::Invalid), "{w}x{h}");
            }
            let mut deep = desc(1, 1, 1, 16, swizzle);
            deep.depth = u32::MAX;
            assert_eq!(Layout::new(&deep), Err(Error::Invalid));
        }
    }

    #[test]
    fn blocks_hold_their_size_in_elements() {
        for log2 in [8u32, 12, 16] {
            let swizzle = match log2 {
                8 => Swizzle::SW_256B_S,
                12 => Swizzle::SW_4KB_S,
                _ => Swizzle::SW_64KB_D,
            };
            for e in 0..5 {
                let l = Layout::new(&desc(1, 1, 1, 1 << e, swizzle)).unwrap();
                let [w, h, d] = l.block;
                assert_eq!(u64::from(w * h * d) << e, 1 << log2, "{log2} {e}");
                // The pattern covers exactly the block's coordinates.
                assert_eq!(l.pattern[0].len() as u32, w);
                assert_eq!(l.pattern[1].len() as u32, h);
            }
        }
        // 64 KiB, 16-byte elements: 64x64 (a 256x256 BC7 texture).
        let l = Layout::new(&desc(64, 64, 1, 16, Swizzle::SW_64KB_D)).unwrap();
        assert_eq!(l.block, [64, 64, 1]);
        assert_eq!((l.size, l.alignment), (65536, 65536));
    }

    /// Every element of every mip and slice lands on its own address, inside
    /// the texture, and tiling then detiling gives the rows back.
    #[test]
    fn addresses_are_a_bijection_and_copies_round_trip() {
        let cases = [
            desc(64, 64, 7, 16, Swizzle::SW_64KB_D),
            desc(100, 60, 7, 4, Swizzle::SW_64KB_D),
            desc(250, 175, 8, 8, Swizzle::SW_4KB_S),
            desc(33, 17, 6, 1, Swizzle::SW_4KB_D),
            desc(40, 9, 3, 2, Swizzle::SW_64KB_S),
            desc(30, 20, 5, 4, Swizzle::LINEAR),
            desc(83, 31, 6, 1, Swizzle::SW_256B_S),
            Desc {
                block: 4,
                ..desc(109, 71, 6, 16, Swizzle::SW_256B_D)
            },
            Desc {
                depth: 3,
                ..desc(70, 70, 4, 16, Swizzle::SW_4KB_S)
            },
            Desc {
                dimension: Dimension::Tex3d,
                depth: 16,
                ..desc(64, 32, 7, 4, Swizzle::SW_4KB_S)
            },
        ];
        for d in cases {
            check_round_trip(&Layout::new(&d).unwrap());
        }
    }

    /// Every element has its own place inside the texture, and copies of every subresource
    /// into and out of tiled memory return the rows and agree with `offset`.
    fn check_round_trip(l: &Layout) {
        let d = l.desc;
        let mut seen = vec![false; l.size as usize];
        let mut tiled = vec![0u8; l.size as usize];
        let n = d.element_bytes as usize;
        let slices = if d.dimension == Dimension::Tex3d {
            1
        } else {
            d.depth
        };
        for slice in 0..slices {
            for mip in 0..d.mips {
                let m = l.mips[mip as usize];
                let active = m.width as usize * n;
                let row = active + 13;
                let plane = row * m.height as usize + 11;
                let linear: Vec<u8> = (0..plane * m.depth as usize)
                    .map(|i| (i * 7 + mip as usize * 13 + slice as usize) as u8)
                    .collect();
                l.tile(&mut tiled, mip, slice, &linear, row, plane).unwrap();
                let mut back = vec![0xcc; linear.len()];
                l.detile(&tiled, mip, slice, &mut back, row, plane).unwrap();
                for z in 0..m.depth {
                    let base = z as usize * plane;
                    for y in 0..m.height {
                        let start = base + y as usize * row;
                        assert_eq!(back[start..start + active], linear[start..start + active]);
                        assert!(back[start + active..start + row].iter().all(|&b| b == 0xcc));
                        for x in 0..m.width {
                            let at = l.offset(mip, x, y, slice + z).unwrap() as usize;
                            let source = start + x as usize * n;
                            assert_eq!(tiled[at..at + n], linear[source..source + n]);
                            assert!(at + n <= seen.len(), "{d:?} mip {mip} outside");
                            assert!(at.is_multiple_of(n));
                            assert!(!seen[at], "{d:?} mip {mip} ({x},{y},{z}) twice");
                            seen[at] = true;
                        }
                    }
                    assert!(
                        back[base + plane - 11..base + plane]
                            .iter()
                            .all(|&b| b == 0xcc)
                    );
                }
            }
        }
    }

    const XOR_MODES: [Swizzle; 8] = [
        Swizzle::SW_64KB_S_T,
        Swizzle::SW_64KB_D_T,
        Swizzle::SW_4KB_S_X,
        Swizzle::SW_4KB_D_X,
        Swizzle::SW_64KB_Z_X,
        Swizzle::SW_64KB_S_X,
        Swizzle::SW_64KB_D_X,
        Swizzle::SW_64KB_R_X,
    ];

    /// The XOR modes 3D textures can use (`D_T` and `4KB_D_X` have no 3D pattern).
    fn has_3d(mode: Swizzle) -> bool {
        matches!(mode.0, 17 | 21 | 24 | 25 | 26 | 27)
    }

    /// A small deterministic generator for the random layouts.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u32) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) % u64::from(n)) as u32
        }
    }

    #[test]
    fn xor_modes_round_trip_on_random_layouts() {
        let mut rng = Lcg(0x5eed);
        let gpus = [Gpu::SERIES_X, Gpu::SERIES_S, Gpu::new(4, 3).unwrap()];
        for gpu in gpus {
            for mode in XOR_MODES {
                for case in 0..5 {
                    let three_d = has_3d(mode) && rng.below(3) == 0;
                    let (w, h, depth) = if three_d {
                        (1 + rng.below(40), 1 + rng.below(40), 1 + rng.below(20))
                    } else {
                        (1 + rng.below(150), 1 + rng.below(150), 1 + rng.below(4))
                    };
                    let longest = w.max(h).max(if three_d { depth } else { 1 });
                    let bpe = 1 << rng.below(5);
                    let d = Desc {
                        dimension: if three_d {
                            Dimension::Tex3d
                        } else {
                            Dimension::Tex2d
                        },
                        width: w,
                        height: h,
                        depth,
                        mips: 1 + rng.below(32 - longest.leading_zeros()),
                        element_bytes: bpe,
                        block: if bpe >= 8 && rng.below(3) == 0 { 4 } else { 1 },
                        samples: 1,
                        swizzle: mode,
                    };
                    let l = Layout::with_gpu(&d, gpu).unwrap();
                    assert!(l.addressable(), "{d:?}");
                    check_round_trip(&l);
                    if case == 1 {
                        // The base address's XOR only moves elements inside their blocks.
                        check_round_trip(&l.with_pipe_bank_xor(rng.below(1 << 12)));
                    }
                }
            }
        }
    }

    /// Every pattern of every GPU addrlib has one for, in every XOR mode and element size, places
    /// each element of a small texture (2D with slices, and 3D) in a place of its own.
    #[test]
    fn xor_patterns_of_every_gpu_are_bijections() {
        for index in 0..xp::GPUS.len() {
            let gpu = Gpu { index: index as u8 };
            for mode in XOR_MODES {
                for element_bytes in [1, 2, 4, 8, 16] {
                    let flat = Desc {
                        dimension: Dimension::Tex2d,
                        width: 90,
                        height: 38,
                        depth: 3,
                        mips: 4,
                        element_bytes,
                        block: 1,
                        samples: 1,
                        swizzle: mode,
                    };
                    check_round_trip(&Layout::with_gpu(&flat, gpu).unwrap());
                    if has_3d(mode) {
                        let volume = Desc {
                            dimension: Dimension::Tex3d,
                            width: 20,
                            height: 12,
                            depth: 6,
                            mips: 3,
                            ..flat
                        };
                        check_round_trip(&Layout::with_gpu(&volume, gpu).unwrap());
                    }
                }
            }
        }
    }

    // (mode, 3D, bytes per element, texels per element side, [width, height, depth or slices], mips,
    //  (pipes, packers), (size, first mip in the tail), [(mip, x, y, z, pipe-bank xor, address)])
    #[rustfmt::skip]
    type Known = (u32, bool, u32, u32, [u32; 3], u32, (u32, u32), (u64, u32), &'static [(u32, u32, u32, u32, u32, u64)]);
    #[rustfmt::skip]
    const ADDRLIB: &[Known] = &[
        (25, false, 4, 1, [256, 256, 1], 1, (8, 4), (262144, 1), &[(0, 255, 255, 0, 0, 260348), (0, 128, 85, 0, 0, 83024), (0, 58, 48, 0, 0, 14600), (0, 199, 184, 0, 793, 235148), (0, 183, 49, 0, 0, 80796), (0, 247, 137, 0, 747, 216988), (0, 63, 116, 0, 4037, 48332), (0, 215, 173, 0, 0, 237532)]),
        (25, false, 4, 1, [100, 60, 1], 7, (8, 4), (131072, 1), &[(0, 99, 59, 0, 0, 112188), (0, 50, 20, 0, 0, 76104), (6, 0, 0, 0, 0, 1536), (5, 0, 0, 0, 0, 3072), (2, 20, 13, 0, 0, 19920), (5, 0, 0, 0, 0, 3072), (6, 0, 0, 0, 2480, 42496), (6, 0, 0, 0, 0, 1536)]),
        (26, false, 8, 1, [300, 200, 1], 9, (8, 4), (1179648, 3), &[(0, 299, 199, 0, 0, 1123000), (0, 150, 66, 0, 3293, 707040), (8, 0, 0, 0, 3173, 25344), (7, 0, 0, 0, 3687, 27392), (5, 2, 0, 0, 0, 8480), (1, 139, 26, 0, 3680, 226472), (6, 3, 2, 0, 0, 4776), (7, 0, 0, 0, 256, 3072)]),
        (27, false, 4, 1, [1920, 1080, 1], 11, (8, 4), (12648448, 5), &[(0, 1919, 1079, 0, 2191, 12644348), (0, 960, 360, 0, 0, 6255616), (10, 0, 0, 0, 0, 2816), (9, 0, 0, 0, 0, 512), (7, 9, 0, 0, 1428, 43012), (9, 2, 1, 0, 0, 536), (2, 166, 249, 0, 0, 750168), (9, 0, 0, 0, 0, 512)]),
        (27, false, 16, 1, [64, 64, 1], 7, (8, 4), (131072, 1), &[(0, 63, 63, 0, 0, 129520), (0, 32, 21, 0, 0, 104224), (6, 0, 0, 0, 1899, 11008), (5, 0, 0, 0, 2116, 25600), (6, 0, 0, 0, 2471, 61184), (5, 1, 0, 0, 0, 8208), (4, 3, 1, 0, 0, 880), (0, 42, 8, 0, 0, 123968)]),
        (24, false, 4, 1, [512, 512, 1], 10, (8, 4), (1441792, 3), &[(0, 511, 511, 0, 0, 1382908), (0, 256, 170, 0, 0, 807968), (9, 0, 0, 0, 0, 4864), (8, 0, 0, 0, 0, 2816), (3, 55, 28, 0, 0, 29908), (2, 85, 75, 0, 0, 119404), (3, 34, 9, 0, 775, 29208), (9, 0, 0, 0, 0, 4864)]),
        (24, false, 2, 1, [100, 60, 1], 7, (8, 4), (131072, 1), &[(0, 99, 59, 0, 0, 80414), (0, 50, 20, 0, 0, 70984), (6, 0, 0, 0, 0, 256), (5, 0, 0, 0, 0, 5120), (2, 5, 12, 0, 0, 18786), (4, 0, 2, 0, 0, 1040), (1, 34, 14, 0, 2943, 64344), (1, 22, 0, 0, 2241, 17192)]),
        (17, false, 1, 1, [255, 129, 1], 8, (8, 4), (131072, 1), &[(0, 254, 128, 0, 0, 126734), (0, 127, 43, 0, 0, 76735), (7, 0, 0, 0, 0, 1280), (6, 0, 0, 0, 0, 1536), (6, 2, 1, 0, 3143, 16658), (5, 3, 1, 0, 2486, 43539), (3, 1, 0, 0, 0, 8449), (0, 120, 38, 0, 0, 76648)]),
        (18, false, 4, 1, [70, 70, 3], 4, (8, 4), (393216, 1), &[(0, 69, 69, 2, 0, 377044), (0, 35, 23, 1, 4, 205244), (3, 0, 0, 0, 0, 8448), (2, 0, 0, 0, 0, 16384), (3, 5, 1, 2, 0, 270676), (1, 13, 0, 1, 0, 164420), (1, 12, 15, 1, 0, 164848), (3, 5, 7, 2, 0, 270836)]),
        (21, false, 2, 1, [250, 175, 1], 8, (8, 4), (135168, 3), &[(0, 249, 174, 0, 3374, 133858), (0, 125, 58, 0, 0, 59562), (7, 0, 0, 0, 0, 768), (6, 0, 0, 0, 0, 1024), (6, 0, 0, 0, 0, 1024), (2, 56, 7, 0, 3412, 6896), (3, 3, 15, 0, 0, 3446), (0, 248, 119, 0, 0, 100848)]),
        (22, false, 16, 1, [33, 17, 4], 6, (8, 4), (163840, 3), &[(0, 32, 16, 3, 0, 160768), (0, 16, 5, 2, 0, 102944), (5, 0, 0, 0, 1222, 768), (4, 0, 0, 0, 0, 1536), (2, 6, 3, 1, 927, 45536), (1, 4, 5, 2, 0, 90400), (3, 2, 1, 1, 0, 43104), (3, 0, 0, 1, 2788, 44032)]),
        (25, false, 4, 1, [128, 128, 7], 4, (8, 4), (917504, 1), &[(0, 127, 127, 6, 499, 858620), (0, 64, 42, 3, 0, 496928), (3, 0, 0, 0, 0, 8448), (2, 0, 0, 0, 0, 16384), (0, 29, 31, 1, 0, 200692), (0, 23, 117, 3, 0, 482524), (1, 49, 14, 4, 0, 568420), (3, 1, 2, 3, 0, 403236)]),
        (27, false, 8, 1, [77, 91, 33], 3, (8, 4), (6488064, 1), &[(0, 76, 90, 32, 0, 6435776), (0, 38, 30, 16, 0, 3239904), (2, 0, 0, 0, 0, 1024), (1, 0, 0, 0, 0, 33024), (0, 38, 85, 15, 0, 3131504), (2, 15, 9, 7, 0, 1389176), (1, 19, 18, 31, 0, 6129320), (0, 51, 49, 10, 0, 2048824)]),
        (25, false, 16, 4, [109, 71, 1], 6, (8, 4), (65536, 0), &[(0, 27, 17, 0, 0, 49104), (0, 14, 6, 0, 851, 52384), (5, 0, 0, 0, 0, 1536), (4, 0, 0, 0, 0, 3072), (0, 12, 5, 0, 1328, 44816), (1, 9, 2, 0, 0, 19552), (4, 0, 0, 0, 2695, 35584), (1, 0, 5, 0, 675, 57872)]),
        (25, true, 4, 1, [64, 32, 16], 7, (8, 4), (262144, 2), &[(0, 63, 31, 15, 0, 261884), (0, 32, 10, 8, 3329, 208672), (6, 0, 0, 0, 3928, 19712), (5, 0, 0, 0, 0, 3584), (0, 54, 23, 3, 2547, 205816), (4, 1, 1, 0, 1592, 13324), (4, 2, 0, 0, 0, 5184), (6, 0, 0, 0, 4064, 60672)]),
        (26, true, 4, 1, [16, 16, 64], 4, (8, 4), (262144, 0), &[(0, 15, 15, 63, 3716, 228092), (0, 8, 5, 32, 1625, 182024), (3, 0, 0, 0, 3966, 22016), (2, 0, 0, 0, 0, 32768), (3, 0, 0, 0, 0, 12288), (1, 3, 7, 31, 2687, 76028), (3, 1, 1, 7, 0, 14508), (1, 7, 4, 10, 0, 26516)]),
        (17, true, 1, 1, [33, 17, 40], 6, (8, 4), (262144, 1), &[(0, 32, 16, 39, 0, 246676), (0, 16, 5, 20, 2114, 95624), (5, 0, 0, 0, 0, 3328), (4, 0, 0, 0, 2128, 19968), (3, 0, 1, 4, 0, 5256), (3, 3, 1, 1, 0, 5135), (1, 1, 3, 10, 0, 34105), (3, 0, 1, 1, 0, 5132)]),
        (21, true, 4, 1, [4, 4, 16], 5, (8, 4), (8192, 0), &[(0, 3, 3, 15, 0, 6908), (0, 2, 1, 8, 2090, 7240), (4, 0, 0, 0, 3416, 0), (3, 0, 0, 0, 329, 0), (3, 0, 0, 0, 640, 256), (2, 0, 0, 2, 0, 640), (1, 1, 0, 6, 0, 1924), (0, 0, 0, 6, 0, 2176)]),
        (27, true, 4, 1, [64, 64, 32], 6, (8, 4), (2097152, 0), &[(0, 63, 63, 31, 0, 2064124), (0, 32, 21, 16, 0, 1074832), (5, 0, 0, 0, 0, 2816), (4, 0, 0, 0, 235, 57600), (3, 5, 6, 1, 2290, 123620), (2, 3, 13, 0, 2612, 4252), (2, 1, 15, 2, 0, 145076), (4, 3, 3, 1, 0, 67132)]),
        (24, true, 4, 1, [32, 32, 32], 6, (8, 4), (2097152, 0), &[(0, 31, 31, 31, 0, 2055932), (0, 16, 10, 16, 0, 1069856), (5, 0, 0, 0, 0, 2816), (4, 0, 0, 0, 374, 25600), (1, 7, 5, 4, 0, 295388), (0, 31, 26, 10, 0, 678516), (2, 7, 6, 2, 0, 141044), (5, 0, 0, 0, 0, 2816)]),
        (25, false, 4, 1, [256, 256, 1], 1, (4, 2), (262144, 1), &[(0, 255, 255, 0, 0, 261372), (0, 128, 85, 0, 0, 83536), (0, 150, 60, 0, 930, 113864), (0, 228, 227, 0, 0, 258224), (0, 19, 193, 0, 0, 149788), (0, 244, 137, 0, 0, 239760), (0, 58, 28, 0, 0, 11336), (0, 39, 121, 0, 0, 30620)]),
        (25, false, 4, 1, [100, 60, 1], 7, (4, 2), (131072, 1), &[(0, 99, 59, 0, 0, 112444), (0, 50, 20, 0, 0, 77640), (6, 0, 0, 0, 0, 1024), (5, 0, 0, 0, 0, 2304), (1, 44, 7, 0, 0, 41712), (4, 1, 0, 0, 897, 37124), (2, 12, 11, 0, 3625, 25264), (5, 2, 0, 0, 0, 2312)]),
        (26, false, 8, 1, [300, 200, 1], 9, (4, 2), (1179648, 3), &[(0, 299, 199, 0, 0, 1123256), (0, 150, 66, 0, 0, 657888), (8, 0, 0, 0, 0, 1024), (7, 0, 0, 0, 0, 2304), (8, 0, 0, 0, 1647, 26368), (4, 10, 3, 0, 0, 17072), (0, 55, 108, 0, 0, 618088), (3, 18, 1, 0, 0, 35120)]),
        (27, false, 4, 1, [1920, 1080, 1], 11, (4, 2), (12648448, 5), &[(0, 1919, 1079, 0, 1465, 12641532), (0, 960, 360, 0, 0, 6248704), (10, 0, 0, 0, 0, 1792), (9, 0, 0, 0, 2493, 45824), (3, 198, 1, 0, 0, 245848), (6, 16, 10, 0, 0, 35360), (7, 1, 5, 0, 0, 4500), (4, 14, 63, 0, 0, 77560)]),
        (27, false, 16, 1, [64, 64, 1], 7, (4, 2), (131072, 1), &[(0, 63, 63, 0, 0, 130544), (0, 32, 21, 0, 0, 84512), (6, 0, 0, 0, 3548, 50176), (5, 0, 0, 0, 3863, 13056), (2, 14, 10, 0, 0, 46528), (1, 16, 15, 0, 3288, 35744), (2, 6, 6, 0, 0, 36288), (2, 12, 10, 0, 571, 34432)]),
        (24, false, 4, 1, [512, 512, 1], 10, (4, 2), (1441792, 3), &[(0, 511, 511, 0, 0, 1379836), (0, 256, 170, 0, 0, 805152), (9, 0, 0, 0, 0, 2816), (8, 0, 0, 0, 0, 1792), (4, 18, 17, 0, 0, 33048), (6, 7, 4, 0, 0, 8660), (5, 9, 12, 0, 3691, 32388), (9, 0, 0, 0, 0, 2816)]),
        (24, false, 2, 1, [100, 60, 1], 7, (4, 2), (131072, 1), &[(0, 99, 59, 0, 0, 81694), (0, 50, 20, 0, 0, 67656), (6, 0, 0, 0, 0, 256), (5, 0, 0, 0, 0, 2304), (1, 16, 11, 0, 0, 17940), (6, 0, 0, 0, 0, 256), (3, 11, 4, 0, 0, 4298), (2, 21, 5, 0, 0, 33382)]),
        (17, false, 1, 1, [255, 129, 1], 8, (4, 2), (131072, 1), &[(0, 254, 128, 0, 0, 125710), (0, 127, 43, 0, 0, 77247), (7, 0, 0, 0, 2728, 42752), (6, 0, 0, 0, 0, 1024), (2, 60, 11, 0, 0, 19388), (1, 3, 16, 0, 3797, 20483), (7, 0, 0, 0, 1271, 62464), (3, 30, 1, 0, 0, 8734)]),
        (18, false, 4, 1, [70, 70, 3], 4, (4, 2), (393216, 1), &[(0, 69, 69, 2, 0, 377044), (0, 35, 23, 1, 0, 206524), (3, 0, 0, 0, 3969, 41216), (2, 0, 0, 0, 0, 16384), (1, 9, 3, 1, 0, 164404), (0, 38, 61, 1, 0, 210904), (1, 23, 23, 2, 304, 311292), (2, 12, 4, 2, 1692, 316096)]),
        (21, false, 2, 1, [250, 175, 1], 8, (4, 2), (135168, 3), &[(0, 249, 174, 0, 0, 133858), (0, 125, 58, 0, 0, 60586), (7, 0, 0, 0, 0, 768), (6, 0, 0, 0, 0, 1536), (2, 40, 1, 0, 3375, 6800), (2, 22, 1, 0, 630, 4124), (3, 10, 14, 0, 0, 2276), (0, 110, 34, 0, 0, 59820)]),
        (22, false, 16, 1, [33, 17, 4], 6, (4, 2), (163840, 3), &[(0, 32, 16, 3, 0, 160256), (0, 16, 5, 2, 1150, 103200), (5, 0, 0, 0, 0, 1792), (4, 0, 0, 0, 0, 1024), (5, 0, 0, 0, 194, 1280), (2, 4, 3, 1, 0, 45216), (2, 3, 3, 2, 0, 86256), (5, 0, 0, 0, 0, 1792)]),
        (25, false, 4, 1, [128, 128, 7], 4, (4, 2), (917504, 1), &[(0, 127, 127, 6, 0, 916732), (0, 64, 42, 3, 0, 496416), (3, 0, 0, 0, 1216, 57344), (2, 0, 0, 0, 1941, 53504), (3, 9, 1, 5, 0, 663572), (0, 8, 12, 4, 249, 651840), (0, 48, 91, 6, 0, 880176), (0, 110, 104, 6, 3048, 856968)]),
        (27, false, 8, 1, [77, 91, 33], 3, (4, 2), (6488064, 1), &[(0, 76, 90, 32, 2427, 6449344), (0, 38, 30, 16, 1512, 3266272), (2, 0, 0, 0, 2966, 4864), (1, 0, 0, 0, 0, 16384), (1, 31, 12, 31, 3559, 6143848), (1, 6, 39, 32, 0, 6342128), (0, 57, 39, 23, 0, 4631704), (2, 6, 0, 18, 3376, 3584352)]),
        (25, false, 16, 4, [109, 71, 1], 6, (4, 2), (65536, 0), &[(0, 27, 17, 0, 1220, 31184), (0, 14, 6, 0, 0, 35488), (5, 0, 0, 0, 0, 1024), (4, 0, 0, 0, 0, 2304), (4, 0, 0, 0, 3410, 23296), (0, 22, 16, 0, 0, 45696), (4, 0, 0, 0, 1634, 27392), (4, 0, 0, 0, 0, 2304)]),
        (25, true, 4, 1, [64, 32, 16], 7, (4, 2), (262144, 2), &[(0, 63, 31, 15, 0, 262140), (0, 32, 10, 8, 1614, 223520), (6, 0, 0, 0, 0, 2816), (5, 0, 0, 0, 0, 2048), (0, 10, 9, 8, 0, 145992), (2, 0, 3, 2, 0, 32936), (5, 1, 0, 0, 0, 2052), (2, 7, 6, 2, 0, 33764)]),
        (26, true, 4, 1, [16, 16, 64], 4, (4, 2), (262144, 0), &[(0, 15, 15, 63, 0, 261884), (0, 8, 5, 32, 0, 165384), (3, 0, 0, 0, 0, 12288), (2, 0, 0, 0, 0, 32768), (0, 4, 9, 55, 0, 211624), (2, 1, 1, 11, 0, 49324), (3, 1, 1, 0, 0, 12300), (3, 0, 1, 3, 0, 12456)]),
        (17, true, 1, 1, [33, 17, 40], 6, (4, 2), (262144, 1), &[(0, 32, 16, 39, 1660, 241812), (0, 16, 5, 20, 2660, 86408), (5, 0, 0, 0, 0, 2816), (4, 0, 0, 0, 0, 2048), (1, 8, 7, 0, 3762, 12584), (4, 0, 0, 1, 0, 2052), (4, 0, 0, 0, 0, 2048), (3, 1, 0, 1, 736, 61701)]),
        (21, true, 4, 1, [4, 4, 16], 5, (4, 2), (8192, 0), &[(0, 3, 3, 15, 483, 7932), (0, 2, 1, 8, 0, 6984), (4, 0, 0, 0, 0, 0), (3, 0, 0, 0, 3342, 768), (4, 0, 0, 0, 0, 0), (0, 0, 1, 3, 1173, 2968), (3, 0, 0, 0, 0, 256), (1, 1, 0, 4, 0, 1284)]),
        (27, true, 4, 1, [64, 64, 32], 6, (4, 2), (2097152, 0), &[(0, 63, 63, 31, 0, 2064380), (0, 32, 21, 16, 0, 1069712), (5, 0, 0, 0, 0, 1792), (4, 0, 0, 0, 0, 512), (3, 2, 6, 0, 0, 8616), (2, 14, 0, 5, 0, 333640), (3, 1, 5, 3, 0, 205716), (0, 18, 18, 0, 0, 16680)]),
        (24, true, 4, 1, [32, 32, 32], 6, (4, 2), (2097152, 0), &[(0, 31, 31, 31, 0, 2052092), (0, 16, 10, 16, 0, 1067552), (5, 0, 0, 0, 0, 1792), (4, 0, 0, 0, 0, 512), (4, 0, 1, 1, 1903, 90888), (1, 7, 8, 9, 1137, 654164), (4, 0, 0, 1, 0, 65536), (5, 0, 0, 0, 0, 1792)]),
        (25, false, 4, 1, [256, 256, 1], 1, (16, 8), (262144, 1), &[(0, 255, 255, 0, 0, 258300), (0, 128, 85, 0, 0, 83536), (0, 3, 115, 0, 0, 24124), (0, 18, 32, 0, 0, 4104)]),
        (27, false, 4, 1, [1920, 1080, 1], 11, (16, 8), (12648448, 5), &[(0, 1919, 1079, 0, 0, 12605948), (0, 960, 360, 0, 0, 6252800), (10, 0, 0, 0, 0, 4864), (9, 0, 0, 0, 1813, 1792)]),
        (24, false, 2, 1, [100, 60, 1], 7, (16, 8), (131072, 1), &[(0, 99, 59, 0, 0, 80670), (0, 50, 20, 0, 0, 67912), (6, 0, 0, 0, 0, 256), (5, 0, 0, 0, 0, 2048)]),
        (21, false, 2, 1, [250, 175, 1], 8, (16, 8), (135168, 3), &[(0, 249, 174, 0, 0, 132834), (0, 125, 58, 0, 0, 58282), (7, 0, 0, 0, 2967, 1024), (6, 0, 0, 0, 364, 2048)]),
        (27, false, 8, 1, [77, 91, 33], 3, (16, 8), (6488064, 1), &[(0, 76, 90, 32, 0, 6451136), (0, 38, 30, 16, 0, 3234784), (2, 0, 0, 0, 0, 1024), (1, 0, 0, 0, 2141, 51456)]),
        (26, true, 4, 1, [16, 16, 64], 4, (16, 8), (262144, 0), &[(0, 15, 15, 63, 0, 261884), (0, 8, 5, 32, 0, 165384), (3, 0, 0, 0, 0, 12288), (2, 0, 0, 0, 3796, 17408)]),
        (27, true, 4, 1, [64, 64, 32], 6, (16, 8), (2097152, 0), &[(0, 63, 63, 31, 0, 2062332), (0, 32, 21, 16, 0, 1068944), (5, 0, 0, 0, 0, 4864), (4, 0, 0, 0, 0, 512)]),
        (25, false, 4, 1, [256, 256, 1], 1, (2, 1), (262144, 1), &[(0, 255, 255, 0, 0, 261884), (0, 128, 85, 0, 0, 83024), (0, 137, 113, 0, 0, 87828), (0, 139, 178, 0, 0, 202540)]),
        (27, false, 16, 1, [64, 64, 1], 7, (2, 1), (131072, 1), &[(0, 63, 63, 0, 0, 130800), (0, 32, 21, 0, 0, 91424), (6, 0, 0, 0, 0, 2560), (5, 0, 0, 0, 0, 4096)]),
        (18, false, 4, 1, [70, 70, 3], 4, (2, 1), (393216, 1), &[(0, 69, 69, 2, 0, 377044), (0, 35, 23, 1, 0, 206012), (3, 0, 0, 0, 0, 8192), (2, 0, 0, 0, 0, 16384)]),
        (27, false, 8, 1, [77, 91, 33], 3, (2, 1), (6488064, 1), &[(0, 76, 90, 32, 0, 6434240), (0, 38, 30, 16, 0, 3226592), (2, 0, 0, 0, 0, 32768), (1, 0, 0, 0, 0, 16384)]),
        (17, true, 1, 1, [33, 17, 40], 6, (2, 1), (262144, 1), &[(0, 32, 16, 39, 0, 245908), (0, 16, 5, 20, 0, 78216), (5, 0, 0, 0, 3752, 8448), (4, 0, 0, 0, 282, 4864)]),
        (25, false, 4, 1, [100, 60, 1], 7, (32, 8), (131072, 1), &[(0, 99, 59, 0, 0, 106812), (0, 50, 20, 0, 0, 80968), (6, 0, 0, 0, 738, 33792)]),
        (24, false, 2, 1, [100, 60, 1], 7, (32, 8), (131072, 1), &[(0, 99, 59, 0, 0, 80670), (0, 50, 20, 0, 0, 67912), (6, 0, 0, 0, 2288, 37120)]),
        (25, false, 4, 1, [128, 128, 7], 4, (32, 8), (917504, 1), &[(0, 127, 127, 6, 0, 913404), (0, 64, 42, 3, 2810, 464672), (3, 0, 0, 0, 1551, 16128)]),
        (17, true, 1, 1, [33, 17, 40], 6, (32, 8), (262144, 1), &[(0, 32, 16, 39, 0, 252052), (0, 16, 5, 20, 1640, 76168), (5, 0, 0, 0, 0, 2304)]),
    ];

    /// Sizes and addresses of the XOR modes against the reference implementation (addrlib,
    /// built from Mesa's copy and run for these textures and GPUs).
    #[test]
    fn xor_modes_match_addrlib() {
        for &(
            mode,
            three_d,
            element_bytes,
            block,
            [width, height, depth],
            mips,
            (pipes, packers),
            expected,
            points,
        ) in ADDRLIB
        {
            let d = Desc {
                dimension: if three_d {
                    Dimension::Tex3d
                } else {
                    Dimension::Tex2d
                },
                width,
                height,
                depth,
                mips,
                element_bytes,
                block,
                samples: 1,
                swizzle: Swizzle(mode),
            };
            let gpu = Gpu::from_counts(pipes, packers).unwrap();
            let l = Layout::with_gpu(&d, gpu).unwrap();
            let name = format!(
                "{} {d:?} on {pipes} pipes and {packers} packers",
                d.swizzle.name()
            );
            assert_eq!((l.size, l.first_tail_mip), expected, "{name}");
            for &(mip, x, y, z, pipe_bank_xor, address) in points {
                let l = l.clone().with_pipe_bank_xor(pipe_bank_xor);
                assert_eq!(
                    l.offset(mip, x, y, z),
                    Ok(address),
                    "{name} mip {mip} ({x}, {y}, {z}) xor {pipe_bank_xor}"
                );
            }
        }
    }

    #[test]
    fn modes_without_a_pattern_are_refused() {
        let mut d = desc(64, 64, 1, 4, Swizzle::SW_4KB_R_X);
        let l = Layout::new(&d).unwrap();
        assert!(!l.addressable());
        assert_eq!(l.offset(0, 0, 0, 0), Err(Error::Unsupported));
        let tiled = vec![0; l.size as usize];
        assert_eq!(
            l.detile(&tiled, 0, 0, &mut [0; 1024], 256, 0),
            Err(Error::Unsupported)
        );
        // Multisampled XOR modes have sample bits in their patterns, ours have none.
        d.swizzle = Swizzle::SW_64KB_R_X;
        d.samples = 4;
        assert_eq!(
            Layout::new(&d).unwrap().offset(0, 0, 0, 0),
            Err(Error::Unsupported)
        );
        assert_eq!(Swizzle::SW_64KB_R_X.name(), "SW_64KB_R_X");
        assert_eq!(Swizzle::SW_64KB_S_T.name(), "SW_64KB_S_T");
        assert_eq!(Swizzle::LINEAR.name(), "SW_LINEAR");
        assert_eq!(Swizzle(3).name(), "SW_?");
    }

    #[test]
    fn gpus_are_named_by_their_pipes_and_packers() {
        assert_eq!(
            (Gpu::SERIES_X.pipes_log2(), Gpu::SERIES_X.packers_log2()),
            (3, 2)
        );
        assert_eq!(
            (Gpu::SERIES_S.pipes_log2(), Gpu::SERIES_S.packers_log2()),
            (2, 1)
        );
        assert_eq!(Gpu::from_counts(8, 4), Some(Gpu::SERIES_X));
        assert_eq!(Gpu::from_counts(4, 2), Some(Gpu::SERIES_S));
        assert_eq!(Gpu::from_counts(6, 4), None);
        assert_eq!(Gpu::from_counts(2, 8), None);
        assert_eq!(Gpu::new(7, 4), None);
        for &(pipes, packers) in &xp::GPUS {
            let gpu = Gpu::new(u32::from(pipes), u32::from(packers)).unwrap();
            assert_eq!(
                (gpu.pipes_log2(), gpu.packers_log2()),
                (u32::from(pipes), u32::from(packers))
            );
        }
    }

    /// The XOR modes differ between GPUs (the other modes do not), and a surface 64 KiB-aligned
    /// has no pipe-bank XOR of its own.
    #[test]
    fn only_xor_modes_depend_on_the_gpu() {
        let at = |mode, gpu| {
            let l = Layout::with_gpu(&desc(256, 256, 1, 4, mode), gpu).unwrap();
            (0..256)
                .map(|i| l.offset(0, i, 255 - i, 0).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            at(Swizzle::SW_64KB_S, Gpu::SERIES_X),
            at(Swizzle::SW_64KB_S, Gpu::SERIES_S)
        );
        assert_ne!(
            at(Swizzle::SW_64KB_S_X, Gpu::SERIES_X),
            at(Swizzle::SW_64KB_S_X, Gpu::SERIES_S)
        );
        let l = Layout::new(&desc(256, 256, 1, 4, Swizzle::SW_64KB_S_X)).unwrap();
        assert_eq!(l, l.clone().with_pipe_bank_xor(0));
        assert_ne!(l, l.clone().with_pipe_bank_xor(0x2a5));
        let plain = Layout::new(&desc(256, 256, 1, 4, Swizzle::SW_64KB_S)).unwrap();
        assert_eq!(plain, plain.clone().with_pipe_bank_xor(0x2a5));
    }

    #[test]
    fn tiled_chunks_match_element_offsets_with_odd_row_pitches() {
        for swizzle in [
            Swizzle::SW_256B_S,
            Swizzle::SW_256B_D,
            Swizzle::SW_4KB_S,
            Swizzle::SW_4KB_D,
            Swizzle::SW_64KB_S,
            Swizzle::SW_64KB_D,
        ] {
            for n in [1, 2, 4, 8, 16] {
                let l = Layout::new(&Desc {
                    depth: 2,
                    ..desc(73, 39, 7, n, swizzle)
                })
                .unwrap();
                let n = n as usize;
                let tiled: Vec<_> = (0..l.size)
                    .map(|i| (i.wrapping_mul(0x9e37_79b9) >> 24) as u8)
                    .collect();
                let mut retiled = vec![0xbb; tiled.len()];
                let mut expected_tiled = retiled.clone();
                for slice in 0..l.slices {
                    for (mip, m) in l.mips.iter().enumerate() {
                        let mip = mip as u32;
                        let bytes = m.width as usize * n;
                        let pitch = bytes + 3;
                        // Keep padding between rows, with exactly the active
                        // bytes in the last row.
                        let len = (m.height as usize - 1) * pitch + bytes;
                        let mut linear = vec![0xcc; len];
                        let mut expected = linear.clone();
                        for y in 0..m.height {
                            for x in 0..m.width {
                                let src = l.offset(mip, x, y, slice).unwrap() as usize;
                                let dst = y as usize * pitch + x as usize * n;
                                expected[dst..][..n].copy_from_slice(&tiled[src..][..n]);
                                expected_tiled[src..][..n].copy_from_slice(&tiled[src..][..n]);
                            }
                        }
                        l.detile(&tiled, mip, slice, &mut linear, pitch, len)
                            .unwrap();
                        assert_eq!(linear, expected, "{swizzle:?} {n} mip {mip} slice {slice}");
                        l.tile(&mut retiled, mip, slice, &expected, pitch, len)
                            .unwrap();
                    }
                }
                assert_eq!(retiled, expected_tiled, "{swizzle:?} {n}");
            }
        }
    }

    /// Hand-checked: in a 64 KiB `D` block of 4-byte elements, x bits go to
    /// address bits 2, 3, 6, 9, 11, 13, 15 and y bits to 4, 5, 7, 8, 10, 12,
    /// 14; a 4 KiB `S` block of 16-byte elements is 16x16 with y first.
    #[test]
    fn hand_checked_addresses() {
        let l = Layout::new(&desc(128, 128, 1, 4, Swizzle::SW_64KB_D)).unwrap();
        assert_eq!(l.offset(0, 1, 0, 0), Ok(1 << 2));
        assert_eq!(l.offset(0, 4, 0, 0), Ok(1 << 6));
        assert_eq!(l.offset(0, 0, 4, 0), Ok(1 << 7));
        assert_eq!(l.offset(0, 0, 8, 0), Ok(1 << 8));
        assert_eq!(l.offset(0, 64, 64, 0), Ok((1 << 15) | (1 << 14)));
        let l = Layout::new(&desc(32, 32, 1, 16, Swizzle::SW_4KB_S)).unwrap();
        assert_eq!(l.block, [16, 16, 1]);
        assert_eq!(l.offset(0, 0, 1, 0), Ok(1 << 4));
        assert_eq!(l.offset(0, 1, 0, 0), Ok(1 << 6));
        // Blocks row by row: (16, 0) is the second block, (0, 16) the third.
        assert_eq!(l.offset(0, 16, 0, 0), Ok(4096));
        assert_eq!(l.offset(0, 0, 16, 0), Ok(8192));
    }

    #[test]
    fn layouts_share_identical_swizzle_tables() {
        for (swizzle, kind, dimension, block_log2) in [
            (Swizzle::SW_256B_S, 0, Dimension::Tex2d, 8),
            (Swizzle::SW_256B_D, 1, Dimension::Tex2d, 8),
            (Swizzle::SW_4KB_S, 0, Dimension::Tex2d, 12),
            (Swizzle::SW_4KB_D, 1, Dimension::Tex2d, 12),
            (Swizzle::SW_64KB_S, 0, Dimension::Tex2d, 16),
            (Swizzle::SW_64KB_D, 1, Dimension::Tex2d, 16),
            (Swizzle::SW_4KB_S, 2, Dimension::Tex3d, 12),
            (Swizzle::SW_64KB_S, 2, Dimension::Tex3d, 16),
        ] {
            for element_log2 in 0..5 {
                let d = Desc {
                    dimension,
                    depth: 8,
                    ..desc(128, 96, 7, 1 << element_log2, swizzle)
                };
                let first = Layout::new(&d).unwrap();
                let second = Layout::new(&Desc {
                    width: 32,
                    height: 24,
                    mips: 3,
                    depth: 1,
                    ..d
                })
                .unwrap();
                let expected = pattern(kind, element_log2, block_log2).map(|bits| spread(&bits));
                assert_eq!(first.pattern, &expected);
                assert!(std::ptr::eq(first.pattern, second.pattern));
                assert!(std::ptr::eq(first.pattern, first.clone().pattern));
            }
        }
    }

    /// A 256x256 BC7 chain in 64 KiB `D`: one tail block holding mips 1-8,
    /// then mip 0; mip 1 sits at x = 32 elements, 32 KiB into the tail.
    #[test]
    fn mip_tail_comes_first() {
        let l = Layout::new(&desc(64, 64, 9, 16, Swizzle::SW_64KB_D)).unwrap();
        assert_eq!(l.first_tail_mip, 1);
        assert_eq!(l.size, 2 * 65536);
        assert_eq!(l.mips[0].offset, 65536);
        assert_eq!(
            l.mips[1].tail,
            Some(Tail {
                offset: 32768,
                x: 32,
                y: 0
            })
        );
        assert_eq!(l.offset(1, 0, 0, 0), Ok(32768));
        // 4 KiB blocks hold fewer tail mips: 16x16 BC7 mips stay outside.
        let l = Layout::new(&desc(64, 64, 9, 16, Swizzle::SW_4KB_S)).unwrap();
        assert_eq!(l.first_tail_mip, 3);
        assert_eq!(l.mips[2].offset, 4096);
        assert_eq!(l.mips[1].offset, 8192);
        assert_eq!(l.mips[0].offset, 8192 + 16384);
    }

    /// A volume deeper than its block keeps its whole chain in the tail when
    /// every mip's width and height fit it (addrlib's `mipChainInTail`): each
    /// depth slab has its own tail block, holding that slab's slices of
    /// every mip. 16x16x64 in 64 KiB `S` blocks of 32x32x16 is four slabs
    /// of one block; mip 1's 32 slices span the first two. Addresses from
    /// addrlib, as in [`layouts_match_addrlib`].
    #[test]
    fn deep_volumes_keep_their_chain_in_each_slabs_tail() {
        let l = Layout::new(&Desc {
            dimension: Dimension::Tex3d,
            depth: 64,
            ..desc(16, 16, 4, 4, Swizzle::SW_64KB_S)
        })
        .unwrap();
        assert_eq!(l.block, [32, 32, 16]);
        assert_eq!((l.first_tail_mip, l.size), (0, 4 * 65536));
        for (mip, x, y, z, address) in [
            (0, 0, 0, 0, 32768),
            (0, 0, 0, 15, 42128),
            (0, 0, 0, 16, 65536 + 32768),
            (0, 15, 15, 63, 3 * 65536 + 49148),
            (1, 0, 0, 0, 16384),
            (1, 0, 0, 16, 65536 + 16384),
            (1, 0, 0, 31, 65536 + 25744),
            (2, 0, 0, 0, 4096),
            (3, 1, 1, 7, 3740),
        ] {
            let at = l.offset(mip, x, y, z);
            assert_eq!(at, Ok(address), "mip {mip} ({x},{y},{z})");
        }
    }

    /// Layouts as AMD's addrlib computes them (Mesa `src/amd/addrlib` at ed09d38,
    /// `Addr2ComputeSurfaceInfo` and `Addr2ComputeSurfaceAddrFromCoord`,
    /// Navi 21 settings, no pipe-bank XOR): its surface size, its first mip
    /// in the tail (`mipChainInTail` is 0; linear has no tail) and an
    /// FNV-1a hash of the address of every element (mips in order, then
    /// slices, rows and columns; 8 little-endian bytes each). Element sizes
    /// 1-16 bytes, block compression, arrays, mip tails and deep volumes, in
    /// every mode [`Layout::offset`] addresses.
    #[test]
    fn layouts_match_addrlib() {
        use Dimension::{Tex1d, Tex2d, Tex3d};
        #[rustfmt::skip]
        let cases = [
            // mode, dimension, element bytes, texels per element side,
            // [width, height, depth or array], mips,
            //     (size, first tail mip, hash)
            (Swizzle::SW_256B_S, Tex2d, 1, 1, [83, 31, 1], 6,
                (5120, 6, 0x435e_9cad_96ff_dd81)),
            (Swizzle::SW_256B_D, Tex2d, 16, 4, [109, 71, 1], 6,
                (13824, 6, 0xbc2b_b84f_c528_7cb3)),
            (Swizzle::SW_4KB_S, Tex2d, 8, 1, [250, 175, 1], 8,
                (495616, 4, 0x1e87_7240_4dcd_c56e)),
            (Swizzle::SW_4KB_S, Tex2d, 16, 1, [70, 70, 3], 4,
                (479232, 4, 0xf948_6d1d_511c_d75c)),
            (Swizzle::SW_4KB_D, Tex2d, 1, 1, [33, 17, 1], 6,
                (8192, 1, 0xaae1_0e3f_15a1_892e)),
            (Swizzle::SW_64KB_S, Tex2d, 2, 1, [40, 9, 1], 3,
                (65536, 0, 0xf7a3_7fe6_41c2_d565)),
            (Swizzle::SW_64KB_S, Tex2d, 4, 1, [100, 60, 6], 7,
                (786432, 1, 0xe6b7_50a0_b65e_09a3)),
            (Swizzle::SW_64KB_S, Tex2d, 8, 1, [65, 33, 1], 7,
                (131072, 1, 0xdf0a_7ddb_9b93_41ee)),
            (Swizzle::SW_64KB_D, Tex2d, 16, 4, [256, 256, 1], 9,
                (131072, 1, 0xe569_7630_03d8_3e28)),
            (Swizzle::SW_64KB_D, Tex2d, 4, 1, [100, 60, 1], 7,
                (131072, 1, 0xb1db_1641_adaf_77a6)),
            (Swizzle::SW_64KB_D, Tex2d, 1, 1, [255, 129, 2], 8,
                (262144, 1, 0xd706_42f5_9a01_5115)),
            (Swizzle::LINEAR, Tex2d, 4, 1, [30, 20, 1], 5,
                (10240, 5, 0x819a_9c12_81e6_40f2)),
            (Swizzle::LINEAR, Tex2d, 2, 1, [17, 9, 3], 5,
                (15360, 5, 0xbf1a_74f1_7803_df62)),
            (Swizzle::LINEAR, Tex1d, 4, 1, [1000, 1, 1], 10,
                (9216, 10, 0xd37e_f9ac_a5de_cab4)),
            (Swizzle::SW_4KB_S, Tex3d, 4, 1, [64, 32, 16], 7,
                (188416, 3, 0xf06b_d42a_7183_90e6)),
            (Swizzle::SW_4KB_S, Tex3d, 4, 1, [4, 4, 16], 5,
                (8192, 0, 0xbf87_001b_ad72_d375)),
            (Swizzle::SW_4KB_S, Tex3d, 1, 1, [1, 1, 17], 2,
                (8192, 0, 0xcdf4_4ee3_33e5_e03d)),
            (Swizzle::SW_64KB_S, Tex3d, 4, 1, [16, 16, 64], 4,
                (262144, 0, 0x5d1a_a7af_c371_e8e5)),
            (Swizzle::SW_64KB_S, Tex3d, 1, 1, [8, 8, 128], 4,
                (262144, 0, 0x8fec_f010_6542_6225)),
            (Swizzle::SW_64KB_S, Tex3d, 16, 1, [33, 17, 40], 6,
                (1966080, 3, 0xec9c_fd44_b89c_2fd5)),
            (Swizzle::LINEAR, Tex3d, 4, 1, [9, 7, 5], 3,
                (16640, 3, 0x6d5b_351d_4a3d_7b5a)),
        ];
        for (swizzle, dimension, element_bytes, block, extent, mips, expected) in cases {
            let [width, height, depth] = extent;
            let d = Desc {
                dimension,
                width,
                height,
                depth,
                mips,
                element_bytes,
                block,
                samples: 1,
                swizzle,
            };
            let l = Layout::new(&d).unwrap();
            let mut h = 0xcbf2_9ce4_8422_2325u64;
            for (mip, m) in (0..).zip(&l.mips) {
                let slices = if dimension == Tex3d { m.depth } else { depth };
                for z in 0..slices {
                    for y in 0..m.height {
                        for x in 0..m.width {
                            for byte in l.offset(mip, x, y, z).unwrap().to_le_bytes() {
                                h = (h ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
                            }
                        }
                    }
                }
            }
            assert_eq!((l.size, l.first_tail_mip, h), expected, "{d:?}");
        }
    }

    /// The smaller mips lie first: they are read from the memory that holds
    /// them alone, as from the whole texture's.
    #[test]
    fn smaller_mips_need_only_the_start_of_the_texture() {
        for swizzle in [Swizzle::SW_4KB_S, Swizzle(9)] {
            let l = Layout::new(&Desc { block: 4, ..desc(1024, 1024, 9, 8, swizzle) }).unwrap();
            assert_eq!(l.bytes_from(0), l.size);
            let needs: Vec<u64> = (0..9).map(|mip| l.bytes_from(mip)).collect();
            assert!(needs.windows(2).all(|w| w[0] >= w[1]), "{needs:?}");
            assert!(needs[3] * 4 < l.size, "{needs:?}");
            let tiled: Vec<u8> = (0..l.size).map(|i| (i * 7 >> 3) as u8).collect();
            for mip in 1..9 {
                let m = l.mips[mip as usize];
                let pitch = m.width as usize * 8;
                let read = |tiled: &[u8]| {
                    let mut linear = vec![0; pitch * m.height as usize];
                    l.detile(tiled, mip, 0, &mut linear, pitch, pitch * m.height as usize).map(|()| linear)
                };
                let len = l.bytes_from(mip) as usize;
                assert_eq!(read(&tiled[..len]), read(&tiled));
                assert_eq!(read(&tiled[..len - 1]), Err(Error::TooSmall));
            }
        }
        // Slices each hold their chain: the whole texture.
        let l = Layout::new(&Desc { depth: 2, block: 4, ..desc(1024, 1024, 9, 8, Swizzle::SW_4KB_S) }).unwrap();
        assert_eq!(l.bytes_from(4), l.size);
    }

    #[test]
    fn depth_stencil_is_two_planes() {
        let t = Texture::new(&TextureDesc {
            dimension: Dimension::Tex2d,
            width: 1920,
            height: 1080,
            depth_or_array: 1,
            mips: 1,
            format: 20,
            samples: 1,
            swizzle: Swizzle::SW_64KB_Z_X,
        })
        .unwrap();
        assert_eq!(t.planes.len(), 2);
        // 4-byte depth in 128x128 blocks, 1-byte stencil in 256x256 blocks.
        assert_eq!(t.planes[0].layout.size, 1920 * 1152 * 4);
        assert_eq!(t.planes[1].offset, t.planes[0].layout.size);
        assert_eq!(t.planes[1].layout.size, 2048 * 1280);
        assert!(!t.planes[0].layout.addressable());
    }

    #[test]
    fn texture_copies_reject_overflowing_pitches() {
        let layout = Layout::new(&desc(4, 2, 1, 4, Swizzle::LINEAR)).unwrap();
        let mut tiled = vec![0; layout.size as usize];
        let mut linear = [0; 32];
        assert_eq!(
            layout.detile(&tiled, 0, 0, &mut linear, usize::MAX, 0),
            Err(Error::TooSmall)
        );
        assert_eq!(
            layout.tile(&mut tiled, 0, 0, &linear, usize::MAX, 0),
            Err(Error::TooSmall)
        );
    }

    fn volume_pitch_layout(swizzle: Swizzle) -> Layout {
        Layout::new(&Desc {
            dimension: Dimension::Tex3d,
            depth: 2,
            ..desc(4, 2, 1, 4, swizzle)
        })
        .unwrap()
    }

    #[test]
    fn depth_pitch_rejects_overlapping_detile() {
        for swizzle in [Swizzle::LINEAR, Swizzle::SW_4KB_S] {
            let l = volume_pitch_layout(swizzle);
            let tiled = vec![0x42; l.size as usize];
            let mut linear = [0xcc; 72];

            for pitch in [0, 16, 35] {
                assert_eq!(
                    l.detile(&tiled, 0, 0, &mut linear, 20, pitch),
                    Err(Error::TooSmall),
                    "{swizzle:?}, pitch {pitch}"
                );

                assert_eq!(linear, [0xcc; 72]);
            }
        }
    }

    #[test]
    fn depth_pitch_rejects_overlapping_tile() {
        for swizzle in [Swizzle::LINEAR, Swizzle::SW_4KB_S] {
            let l = volume_pitch_layout(swizzle);
            let mut tiled = vec![0xcc; l.size as usize];
            let linear = [0x42; 72];

            for pitch in [0, 16, 35] {
                assert_eq!(
                    l.tile(&mut tiled, 0, 0, &linear, 20, pitch),
                    Err(Error::TooSmall),
                    "{swizzle:?}, pitch {pitch}"
                );

                assert!(tiled.iter().all(|&b| b == 0xcc));
            }
        }
    }

    #[test]
    fn depth_pitch_accepts_exact_and_padded_slices() {
        for swizzle in [Swizzle::LINEAR, Swizzle::SW_4KB_S] {
            let l = volume_pitch_layout(swizzle);

            for pitch in [36usize, 40, 64] {
                let len = pitch + 36;
                let mut tiled = vec![0; l.size as usize];

                let linear: Vec<u8> = (0..len).map(|i| i as u8).collect();

                let mut back = vec![0xcc; len];

                l.tile(&mut tiled, 0, 0, &linear, 20, pitch).unwrap();

                l.detile(&tiled, 0, 0, &mut back, 20, pitch).unwrap();

                for z in 0..2 {
                    for y in 0..2 {
                        let start = z * pitch + y * 20;

                        assert_eq!(back[start..start + 16], linear[start..start + 16]);
                    }
                }
            }
        }
    }

    #[test]
    fn volume_copy_rejects_overflowing_depth_pitch() {
        let layout = Layout::new(&Desc {
            dimension: Dimension::Tex3d,
            depth: 3,
            ..desc(4, 2, 1, 4, Swizzle::LINEAR)
        })
        .unwrap();
        let tiled = vec![0; layout.size as usize];
        let mut linear = [0; 96];
        assert_eq!(
            layout.detile(&tiled, 0, 0, &mut linear, 16, usize::MAX),
            Err(Error::TooSmall)
        );
    }

    #[test]
    fn bad_descriptions_and_buffers_are_refused() {
        assert_eq!(
            Layout::new(&desc(0, 4, 1, 4, Swizzle::SW_4KB_S)),
            Err(Error::Invalid)
        );
        assert_eq!(
            Layout::new(&desc(4, 4, 1, 12, Swizzle::SW_4KB_S)),
            Err(Error::Invalid)
        );
        assert_eq!(
            Layout::new(&desc(4, 4, 1, 4, Swizzle(3))),
            Err(Error::Invalid)
        );
        let l = Layout::new(&desc(64, 64, 1, 4, Swizzle::SW_4KB_S)).unwrap();
        let mut out = vec![0; 64 * 64 * 4];
        assert_eq!(
            l.detile(&[0; 100], 0, 0, &mut out, 256, 0),
            Err(Error::TooSmall)
        );
        assert_eq!(l.offset(0, 64, 0, 0), Err(Error::Invalid));
    }
}
