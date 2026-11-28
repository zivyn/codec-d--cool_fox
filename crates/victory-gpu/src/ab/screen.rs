//! `VICTORY_AB_SCREEN=1`: one run measures the main effect of every named switch (up to 31) on the frame time.
//!
//! The A/B has two plans: every combination of up to 8 switches (`2^n` phases), and a bundle (all off against all on).
//! The first is too long past three or four switches, the second says nothing of any one of them. A screening
//! design keeps the length of the first and answers the question of the second: the phases follow the rows of a
//! Hadamard matrix, so each switch is on in half the phases and off in the other half, and for every pair of
//! switches the four combinations come equally often. The effect of a switch is then the mean of the frames of the
//! phases it is on in against the mean of those it is off in, and the others cancel out of it. 11 switches take 32
//! phases, 31 switches 64, a phase being what `VICTORY_AB` says (10 s in the pilot's rows).
//!
//! Each row is followed by its complement (every switch the other way), so that a drift of the process that is
//! straight over the two phases cancels out of every effect. The fit also carries a drift term over the phases, so
//! that a drift that is not straight in the process's life is taken out too. What the effects are given with is
//! the spread of the phases around the fit and its degrees of freedom: Student's t on them, as the other plans.
//!
//! This is the main effect of each switch, with the others on or off half of the time: it says what turning the
//! switch on does on average over the mixtures the run went through, not what it does next to one other switch. Two
//! switches that are known to be bad together (one aborts the process with the other) are not to be named in the same
//! screen. A switch whose effect is large against its interval is then to be taken to a plain pair.

/// The order of the Hadamard matrix (a power of two) whose columns 1.. hold `switches` switches: the least power of
/// two above their number.
pub(super) fn order(switches: usize) -> usize {
    (switches + 1).next_power_of_two().max(2)
}

/// How many phases come before the same phase is the same again: each row and its complement.
pub(super) fn cycle(switches: usize) -> u64 {
    2 * order(switches) as u64
}

/// The switches that are on in phase `phase`, a bit each. Phase 0 has every switch off and phase 1 every switch on
/// (the first row and its complement); the others follow the rows of the matrix, each followed by its complement.
pub(super) fn bits(phase: u64, switches: usize) -> u32 {
    let n = order(switches) as u64;
    let row = (phase / 2) % n;
    let complement = phase % 2 == 1;
    let mut bits = 0u32;
    for c in 0..switches {
        // Column c + 1 of the Sylvester matrix: the sign of row `row` is the parity of the bits they share.
        let on = ((row & (c as u64 + 1)).count_ones() % 2 == 1) != complement;
        if on {
            bits |= 1 << c;
        }
    }
    bits
}

/// The effects of a screen: what turning each switch on adds to the frame time, with the half of its 95 % interval.
#[derive(Debug, PartialEq)]
pub(super) struct Fit {
    /// Per switch: on minus off, in ms a frame, and half its interval.
    pub(super) effects: Vec<(f64, f64)>,
    /// What the frame time moves by from one phase to the next, in ms, outside the switches.
    pub(super) drift: f64,
    /// The spread of the phases around the fit, in ms.
    pub(super) sd: f64,
    /// Phases used and their degrees of freedom.
    pub(super) phases: usize,
    pub(super) df: usize,
}

/// Solves `a x = b` for the symmetric `a` of order `m` and gives the diagonal of its inverse too (Gauss-Jordan with
/// partial pivoting, on a copy). `None` when `a` is singular.
fn solve(a: &[Vec<f64>], b: &[f64]) -> Option<(Vec<f64>, Vec<f64>)> {
    let m = a.len();
    // The matrix with the identity and the right hand side beside it.
    let mut w: Vec<Vec<f64>> = (0..m)
        .map(|i| {
            let mut row = a[i].clone();
            row.extend((0..m).map(|j| f64::from(u8::from(i == j))));
            row.push(b[i]);
            row
        })
        .collect();
    for col in 0..m {
        let pivot = (col..m).max_by(|&x, &y| w[x][col].abs().total_cmp(&w[y][col].abs()))?;
        if w[pivot][col].abs() < 1e-9 {
            return None;
        }
        w.swap(col, pivot);
        let p = w[col][col];
        for v in &mut w[col] {
            *v /= p;
        }
        let pivot_row = w[col].clone();
        for (r, row) in w.iter_mut().enumerate() {
            if r != col && row[col] != 0.0 {
                let f = row[col];
                for (v, p) in row.iter_mut().zip(&pivot_row) {
                    *v -= f * p;
                }
            }
        }
    }
    let x: Vec<f64> = (0..m).map(|i| w[i][2 * m]).collect();
    let diagonal: Vec<f64> = (0..m).map(|i| w[i][m + i]).collect();
    Some((x, diagonal))
}

/// The effects of the `switches` switches from the phases that ended (`means`: bits, mean ms, frames), by least
/// squares on a constant, a drift over the phases and one column of plus and minus one for each switch. `None` until
/// there are more phases than terms, or when the phases do not tell the switches apart.
pub(super) fn fit(means: &[(u32, f64, usize)], switches: usize) -> Option<Fit> {
    let terms = switches + 2;
    let used: Vec<(usize, u32, f64)> = means
        .iter()
        .enumerate()
        .filter(|(_, m)| m.2 > 0 && m.1.is_finite())
        .map(|(t, m)| (t, m.0, m.1))
        .collect();
    if used.len() <= terms {
        return None;
    }
    let mid = (means.len() as f64 - 1.0) / 2.0;
    let row = |t: usize, bits: u32| -> Vec<f64> {
        let mut x = Vec::with_capacity(terms);
        x.push(1.0);
        x.push(t as f64 - mid);
        x.extend((0..switches).map(|c| if bits >> c & 1 != 0 { 1.0 } else { -1.0 }));
        x
    };
    let mut xtx = vec![vec![0.0; terms]; terms];
    let mut xty = vec![0.0; terms];
    for &(t, bits, y) in &used {
        let x = row(t, bits);
        for i in 0..terms {
            xty[i] += x[i] * y;
            for j in 0..terms {
                xtx[i][j] += x[i] * x[j];
            }
        }
    }
    let (b, inverse) = solve(&xtx, &xty)?;
    let rss: f64 = used
        .iter()
        .map(|&(t, bits, y)| {
            let x = row(t, bits);
            let fitted: f64 = x.iter().zip(&b).map(|(a, c)| a * c).sum();
            (y - fitted).powi(2)
        })
        .sum();
    let df = used.len() - terms;
    let variance = rss / df as f64;
    let t = super::t95(df);
    let effects = (0..switches)
        .map(|c| {
            let at = c + 2;
            (
                2.0 * b[at],
                2.0 * t * (variance * inverse[at].max(0.0)).sqrt(),
            )
        })
        .collect();
    Some(Fit {
        effects,
        drift: b[1],
        sd: variance.sqrt(),
        phases: used.len(),
        df,
    })
}

/// The log line of a fit: the switches by the size of their effect, `*` marking those whose interval holds no zero.
pub(super) fn line(names: &[String], fit: &Fit) -> String {
    let mut order: Vec<usize> = (0..names.len()).collect();
    order.sort_by(|&a, &b| fit.effects[b].0.abs().total_cmp(&fit.effects[a].0.abs()));
    let parts: Vec<String> = order
        .iter()
        .map(|&c| {
            let (e, ci) = fit.effects[c];
            format!(
                "{}{} {e:+.2}+-{ci:.2}",
                if e.abs() > ci { "*" } else { "" },
                names[c]
            )
        })
        .collect();
    format!(
        "A/B screen so far ({} phases, {} degrees of freedom, sd {:.2} ms, drift {:+.3} ms a phase; on minus off, ms/frame, 95%, * = not zero): {}",
        fit.phases,
        fit.df,
        fit.sd,
        fit.drift,
        parts.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(switches: usize) -> Vec<u32> {
        (0..cycle(switches)).map(|p| bits(p, switches)).collect()
    }

    #[test]
    fn the_order_holds_the_switches_in_columns_after_the_first() {
        assert_eq!(
            [1, 2, 3, 4, 7, 8, 11, 15, 16, 31].map(order),
            [2, 4, 4, 8, 8, 16, 16, 16, 32, 32]
        );
        assert_eq!([1, 11, 31].map(cycle), [4, 32, 64]);
    }

    #[test]
    fn the_first_phase_is_every_switch_off_and_the_second_every_switch_on() {
        for k in [1, 3, 7, 11, 31] {
            assert_eq!(bits(0, k), 0, "{k}");
            assert_eq!(bits(1, k), ((1u64 << k) - 1) as u32, "{k}");
        }
    }

    #[test]
    fn each_switch_is_on_in_half_the_phases_and_each_pair_in_a_quarter_in_each_combination() {
        for k in [1usize, 2, 3, 5, 7, 11, 15, 16, 31] {
            let phases = all(k);
            let p = phases.len();
            for a in 0..k {
                assert_eq!(
                    phases.iter().filter(|b| *b >> a & 1 != 0).count(),
                    p / 2,
                    "switch {a} of {k}"
                );
                for b in a + 1..k {
                    for (x, y) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                        let n = phases
                            .iter()
                            .filter(|v| (*v >> a & 1, *v >> b & 1) == (x, y))
                            .count();
                        assert_eq!(n, p / 4, "switches {a} and {b} of {k}, combination {x}{y}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_row_is_followed_by_its_complement_and_the_rows_are_all_different() {
        let k = 11;
        let phases = all(k);
        let full = ((1u64 << k) - 1) as u32;
        for pair in phases.chunks(2) {
            assert_eq!(pair[0] ^ pair[1], full);
        }
        let mut rows: Vec<u32> = phases.iter().step_by(2).copied().collect();
        rows.sort_unstable();
        rows.dedup();
        assert_eq!(
            rows.len(),
            order(k),
            "every row of the matrix is a different row"
        );
        // And the cycle repeats.
        assert_eq!(bits(cycle(k), k), bits(0, k));
        assert_eq!(bits(cycle(k) + 5, k), bits(5, k));
    }

    /// The means of a run of `phases` phases of a process whose frame time is made of the given effects, a drift
    /// and `noise` (a function of the phase).
    fn run(
        switches: usize,
        phases: u64,
        base: f64,
        effects: &[f64],
        drift: f64,
        noise: impl Fn(u64) -> f64,
    ) -> Vec<(u32, f64, usize)> {
        (0..phases)
            .map(|p| {
                let b = bits(p, switches);
                let ms = base
                    + drift * p as f64
                    + effects
                        .iter()
                        .enumerate()
                        .map(|(c, e)| if b >> c & 1 != 0 { *e / 2.0 } else { -*e / 2.0 })
                        .sum::<f64>()
                    + noise(p);
                (b, ms, 100)
            })
            .collect()
    }

    #[test]
    fn the_effects_are_found_each_apart_from_the_others_and_from_a_drift() {
        let mut effects = vec![0.0; 11];
        effects[2] = 6.0;
        effects[5] = -1.5;
        effects[9] = 0.5;
        // Two cycles' worth of phases with a straight drift of 0.2 ms a phase and a little noise that cannot be fitted.
        let wiggle = |p: u64| ((p * 7 % 5) as f64 - 2.0) * 0.1;
        let means = run(11, 64, 110.0, &effects, 0.2, wiggle);
        let fit = fit(&means, 11).expect("64 phases fit 13 terms");
        assert_eq!(fit.phases, 64);
        assert_eq!(fit.df, 64 - 13);
        for (c, want) in effects.iter().enumerate() {
            assert!(
                (fit.effects[c].0 - want).abs() < 0.15,
                "switch {c}: {:?} against {want}",
                fit.effects[c]
            );
        }
        assert!((fit.drift - 0.2).abs() < 0.01, "{:?}", fit);
        assert!(fit.sd < 0.3, "{:?}", fit);
        // The ones that move the frame stand clear of their interval, the ones that do not hold zero.
        assert!(fit.effects[2].0.abs() > fit.effects[2].1);
        assert!(fit.effects[0].0.abs() < fit.effects[0].1 + 0.2);
    }

    #[test]
    fn a_cycle_that_is_not_whole_still_gives_the_effects_when_the_drift_is_fitted() {
        // 20 phases of a 12-switch screen is not a whole number of cycles; the fit still separates them.
        let mut effects = vec![0.0; 7];
        effects[1] = 4.0;
        effects[4] = -2.0;
        let means = run(7, 24, 100.0, &effects, -0.3, |_| 0.0);
        let fit = fit(&means, 7).unwrap();
        assert!(
            (fit.effects[1].0 - 4.0).abs() < 1e-6 && (fit.effects[4].0 + 2.0).abs() < 1e-6,
            "{:?}",
            fit.effects
        );
        assert!((fit.drift + 0.3).abs() < 1e-6, "{}", fit.drift);
        assert!(fit.sd < 1e-6);
    }

    #[test]
    fn nothing_is_said_until_there_are_more_phases_than_terms_and_phases_without_frames_are_passed_over()
     {
        let means = run(3, 20, 100.0, &[1.0, 0.0, 0.0], 0.0, |p| (p % 3) as f64);
        // 3 switches have 5 terms: 5 phases are not enough, a whole cycle of 8 is (3 degrees).
        assert!(fit(&means[..5], 3).is_none());
        assert_eq!(fit(&means[..8], 3).map(|f| f.df), Some(3));
        let mut holes = means.clone();
        holes[4].2 = 0;
        holes[7].1 = f64::NAN;
        let fit = fit(&holes, 3).unwrap();
        assert_eq!(fit.phases, 18);
        assert_eq!(fit.df, 18 - 5);
        // Phases that do not tell the switches apart (the same bits throughout) are not a fit.
        let same: Vec<(u32, f64, usize)> =
            (0..12).map(|p| (0b101, 100.0 + p as f64, 100)).collect();
        assert!(super::fit(&same, 3).is_none());
    }

    #[test]
    fn the_line_names_the_switches_by_the_size_of_their_effect_and_marks_the_clear_ones() {
        let fit = Fit {
            effects: vec![(0.2, 0.5), (-3.0, 1.0), (1.0, 0.9)],
            drift: 0.01,
            sd: 0.7,
            phases: 16,
            df: 11,
        };
        let names: Vec<String> = ["VICTORY_A", "VICTORY_B", "VICTORY_C"]
            .map(String::from)
            .to_vec();
        let text = line(&names, &fit);
        let b = text.find("*VICTORY_B -3.00+-1.00").expect(&text);
        let c = text.find("*VICTORY_C +1.00+-0.90").expect(&text);
        let a = text.find(" VICTORY_A +0.20+-0.50").expect(&text);
        assert!(b < c && c < a, "{text}");
        assert!(text.contains("16 phases, 11 degrees of freedom"), "{text}");
    }
}
