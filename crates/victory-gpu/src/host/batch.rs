use super::{Batch, ranges_cover};
use std::sync::Arc;

/// What is left is what a look at each in turn leaves, in the order it was in, on the threads or not.
#[test]
fn what_is_known_is_left_out_whether_it_is_asked_on_the_threads_or_one() {
    use super::retain_where;
    let mut dice = Dice(0x0123_4567_89ab_cdef);
    for n in [0usize, 1, 2, 255, 511, 512, 513, 1000, 4097, 9000] {
        for threads in [false, true] {
            let items: Vec<(usize, Arc<usize>)> = (0..n).map(|i| (i, Arc::new(dice.below(3)))).collect();
            let want: Vec<usize> = items.iter().filter(|(_, k)| **k != 0).map(|(i, _)| *i).collect();
            let mut got = items.clone();
            retain_where(&mut got, |(_, k)| **k == 0, threads);
            assert_eq!(got.iter().map(|(i, _)| *i).collect::<Vec<_>>(), want, "{n} items, threads {threads}");
            // The holds the jobs took are let go of.
            assert!(got.iter().all(|(_, k)| Arc::strong_count(k) == 2), "{n} items");
        }
    }
}

/// A small generator of numbers (xorshift).
struct Dice(u64);

impl Dice {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as usize % n
    }
}

/// By bisection or by looking at all, a range is held by the same ranges when they are in order and apart; and
/// whatever the ranges, with `sorted` false every one is looked at.
#[test]
fn ranges_that_are_apart_are_looked_through_by_bisection() {
    let mut dice = Dice(0x2545_f491_4f6c_dd1d);
    for _ in 0..3000 {
        let mut ranges = Vec::new();
        let mut at = dice.below(50);
        for _ in 0..dice.below(40) {
            let len = dice.below(30);
            ranges.push((at, at + len));
            at += len + dice.below(30);
        }
        for _ in 0..60 {
            let address = dice.below(at + 40);
            let end = address + dice.below(12);
            let all = ranges.iter().any(|&(from, to)| from <= address && end <= to);
            assert_eq!(ranges_cover(&ranges, address, end, true), all, "{ranges:?} {address} {end}");
            assert_eq!(ranges_cover(&ranges, address, end, false), all);
        }
        // Ranges that overlap and nest, looked through whole.
        let mut tangled: Vec<(usize, usize)> = Vec::new();
        for _ in 0..dice.below(30) {
            let from = dice.below(200);
            tangled.push((from, from + dice.below(120)));
        }
        tangled.sort_unstable_by_key(|r| std::cmp::Reverse(r.1 - r.0));
        for _ in 0..60 {
            let address = dice.below(300);
            let end = address + dice.below(12);
            let all = tangled.iter().any(|&(from, to)| from <= address && end <= to);
            assert_eq!(ranges_cover(&tangled, address, end, false), all);
        }
    }
}

/// What a batch adds to its refreshes, or leaves because the copy is refreshed ahead of the steps already, is the same with the
/// lean bookkeeping as without, whatever comes between: ranges put ahead (in order or not, one or several for a copy),
/// ranges forgotten (one copy's, or all), the copies' steps on end or alternating.
#[test]
fn a_lean_batch_adds_what_a_plain_one_adds() {
    use crate::shadow::Shadow;
    let mut dice = Dice(0x9E37_79B9_7F4A_7C15);
    let memory = vec![0u8; 4096];
    let copies: Vec<Arc<Shadow>> = (0..9).map(|i| Shadow::unbacked(0x10_0000 * (i + 1), memory.as_ptr().cast_mut(), 4096, false)).collect();
    let key = |c: &Arc<Shadow>| Arc::as_ptr(c) as usize;
    let seen = |b: &Batch| b.refreshes.iter().map(|(c, from, to)| (key(c), *from, *to)).collect::<Vec<_>>();
    let (mut covered, mut open) = (0, 0);
    for trial in 0..400 {
        let (mut plain, mut lean) = (Batch::new(false), Batch::new(true));
        for step in 0..200 {
            let c = &copies[if dice.below(3) == 0 { dice.below(copies.len()) } else { (step / 7) % copies.len() }];
            let base = 0x10_0000 * (copies.iter().position(|x| Arc::ptr_eq(x, c)).unwrap() + 1);
            match dice.below(12) {
                0 | 1 => {
                    // In order and apart, or not.
                    let from = base + dice.below(3000);
                    let to = if trial % 3 == 0 { from + dice.below(400) } else { base + 3000 + dice.below(1000) };
                    for b in [&mut plain, &mut lean] {
                        b.put_ahead(key(c), from, to);
                    }
                }
                2 => {
                    let copy = (dice.below(2) == 0).then(|| key(c));
                    for b in [&mut plain, &mut lean] {
                        b.forget_ahead(copy);
                    }
                }
                _ => {
                    let at = base + dice.below(4000);
                    let len = dice.below(300);
                    let (a, b) = (plain.covered(key(c), at, at + len), lean.covered(key(c), at, at + len));
                    assert_eq!(a, b, "trial {trial} step {step}: {at:#x}+{len}");
                    if a {
                        covered += 1;
                    } else {
                        open += 1;
                    }
                    plain.add(c, at, len);
                    lean.add(c, at, len);
                }
            }
            assert_eq!(seen(&plain), seen(&lean), "trial {trial} step {step}");
        }
    }
    assert!(covered > 500 && open > 500, "{covered} covered, {open} not");
}

#[test]
fn a_texture_is_checked_once_per_submission() {
    let mut batch = Batch::default();
    let (a, b) = (Arc::new([0u8; 16]), Arc::new([0u8; 16]));
    // The first list sampling each texture checks it ...
    assert!(batch.texture_due(&a));
    assert!(batch.texture_due(&b));
    // ... later lists of the same submission do not.
    assert!(!batch.texture_due(&a));
    assert!(!batch.texture_due(&a.clone()));
    assert!(!batch.texture_due(&b));
}

#[test]
fn title_writes_make_textures_due_again() {
    let mut batch = Batch::default();
    let a = Arc::new(1u32);
    assert!(batch.texture_due(&a));
    assert!(!batch.texture_due(&a));
    // A copy into title memory or work that may write it may have
    // changed the texture's memory.
    batch.title_written();
    assert!(batch.texture_due(&a));
    assert!(!batch.texture_due(&a));
    // A fresh submission starts over.
    let mut next = Batch::default();
    assert!(next.texture_due(&a));
}

/// The hashing a submission does for textures in physical-page memory,
/// with every list checking the textures it samples (before) and with
/// each checked once (after). Run with
/// `cargo test --release -p victory-gpu -- --ignored --nocapture texture_checks`.
#[test]
#[ignore = "benchmark"]
fn texture_checks_per_submission() {
    const TEXTURES: usize = 48;
    const SIZE: usize = 4 << 20;
    const LISTS: usize = 12;
    let textures: Vec<Arc<Vec<u8>>> = (0..TEXTURES)
        .map(|t| Arc::new((0..SIZE).map(|i| (i * 31 + t) as u8).collect()))
        .collect();
    let submit = |dedupe: bool| {
        let mut batch = Batch::default();
        let mut hashed = 0usize;
        let started = std::time::Instant::now();
        for _ in 0..LISTS {
            for t in &textures {
                if !dedupe || batch.texture_due(t) {
                    std::hint::black_box(blake3::hash(t));
                    hashed += 1;
                }
            }
        }
        (started.elapsed(), hashed)
    };
    submit(false); // warm up
    let (before, n_before) = submit(false);
    let (after, n_after) = submit(true);
    assert_eq!((n_before, n_after), (TEXTURES * LISTS, TEXTURES));
    println!(
        "{LISTS} lists sampling {TEXTURES} textures of {} MiB: \
         before {n_before} hashes in {before:?}, after {n_after} in {after:?} ({:.1}x)",
        SIZE >> 20,
        before.as_secs_f64() / after.as_secs_f64()
    );
}

#[test]
fn vram_idle_ms_scales_with_pressure() {
    let vram_idle_ms = |memory| super::vram_idle_for(memory, false);
    let keeps = |memory| super::vram_idle_for(memory, true);
    let gib = 1024 * 1024 * 1024;
    assert_eq!(keeps(Some([100, 67, 0, 0])), 600_000);
    assert_eq!(keeps(Some([100, 68, 0, 0])), 30_000);
    assert_eq!(keeps(Some([100, 75, 0, 0])), 2_500);
    assert_eq!(keeps(Some([100, 85, 0, 0])), 1_000);
    assert_eq!(keeps(None), 5_000);
    // Normal (< 75%)
    assert_eq!(vram_idle_ms(Some([10 * gib, 0, 0, 0])), 5_000);
    assert_eq!(vram_idle_ms(Some([10 * gib, 7 * gib, 0, 0])), 5_000);
    assert_eq!(vram_idle_ms(Some([100, 74, 0, 0])), 5_000);
    // Moderate (75%..85%)
    assert_eq!(vram_idle_ms(Some([100, 75, 0, 0])), 2_500);
    assert_eq!(vram_idle_ms(Some([100, 84, 0, 0])), 2_500);
    // High (>= 85%, including over-budget)
    assert_eq!(vram_idle_ms(Some([100, 85, 0, 0])), 1_000);
    assert_eq!(vram_idle_ms(Some([10 * gib, 12 * gib, 0, 0])), 1_000);
    // Missing or zero budget falls back to normal
    assert_eq!(vram_idle_ms(None), 5_000);
    assert_eq!(vram_idle_ms(Some([0, 100, 0, 0])), 5_000);
}

#[test]
fn textures_are_kept_while_the_card_has_room() {
    use super::vram_keep;
    let gib = 1 << 30;
    assert_eq!(vram_keep(100 * gib, 87 * gib).0, 600_000);
    // Past it: the longest idle ones, enough to come down to 85 %.
    let (ms, bytes) = vram_keep(100 * gib, 90 * gib);
    assert_eq!((ms, bytes), (5_000, 5 * gib));
    assert_eq!(vram_keep(100 * gib, 120 * gib), (1_500, 35 * gib));
    assert_eq!(vram_keep(0, 7).0, 5_000);
}

#[test]
fn ram_idle_ms_scales_with_pressure() {
    let ram_idle_ms = |avail| super::ram_idle_for(avail, false);
    assert_eq!(super::ram_idle_for(Some(12 << 30), true), 300_000);
    assert_eq!(super::ram_idle_for(Some((12 << 30) - 1), true), 10_000);
    assert_eq!(super::ram_idle_for(None, true), 10_000);
    let gib = 1024 * 1024 * 1024;
    // Normal (>= 4 GiB)
    assert_eq!(ram_idle_ms(Some(8 * gib)), 10_000);
    assert_eq!(ram_idle_ms(Some(4 * gib)), 10_000);
    // Moderate (2 GiB..4 GiB)
    assert_eq!(ram_idle_ms(Some(4 * gib - 1)), 5_000);
    assert_eq!(ram_idle_ms(Some(2 * gib)), 5_000);
    // High (< 2 GiB)
    assert_eq!(ram_idle_ms(Some(2 * gib - 1)), 2_000);
    assert_eq!(ram_idle_ms(Some(0)), 2_000);
    // Missing falls back to normal
    assert_eq!(ram_idle_ms(None), 10_000);
}
