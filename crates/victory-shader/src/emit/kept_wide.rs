//! The skips and early-outs `VICTORY_KEEP_LOOP_SKIPS` keeps over loops, taken by the whole wave
//! ([`Form::KeptSkipsWide`]): a wave with no lane left does not walk the loop (which ran to its
//! limit where its exit reads a lane that is off: DefragHeap, 7.8 ms of the frame), and the loop
//! is code every lane runs, so the two host waves exchange in it. Taken only where no branch of
//! the form can hang the thread group ([`super::wide_class`]): a group barrier under a branch that
//! the waves of a group may decide differently is how the device was lost.
//! `VICTORY_KEPT_SKIPS_WIDE=0` leaves the form out, `=all` takes it whatever the classes say.

use super::form::{self, Mode};
use super::hang;
use super::wide_class;
use super::*;

/// Whether the program has skips or early-outs over loops that the switch keeps, and the wide form
/// of them is asked for.
pub(super) fn applicable(p: &ir::Program) -> bool {
    form::kept_skips_mode() != Mode::Off
        && ir::keep_loop_skips()
        && std::env::var_os("VICTORY_WAVE_WIDE").is_none_or(|v| v != "0")
        && (!ir::drop_early_outs_as(p, true).1.is_empty()
            || !ir::drop_skips_as(p, false, true).1.is_empty()
            || !ir::drop_skips_as(p, true, true).1.is_empty())
}

/// The program's translation with its kept skips wave-wide, if it lowers that way and every
/// branch is of class A or B; None for the forms the program has without the switch.
pub(super) fn attempt(
    c: &Container,
    native: &NativeProgram,
    p: &ir::Program,
    why: bool,
) -> Option<Result<(HostShader, Option<u32>), ShaderError>> {
    let mode = form::kept_skips_mode();
    if mode == Mode::Off
        || !ir::keep_loop_skips()
        || std::env::var_os("VICTORY_WAVE_WIDE").is_some_and(|v| v == "0")
    {
        return None;
    }
    let deep_on = std::env::var_os("VICTORY_WAVE_DEEP_SKIPS").is_none_or(|v| v != "0");
    let (early, early_kept) = ir::drop_early_outs_as(p, true);
    let (skips, skips_kept) = ir::drop_skips_as(p, false, true);
    let (deep, deep_kept) = if deep_on { ir::drop_skips_as(p, true, true) } else { (None, Vec::new()) };
    let done = |r: &Result<(HostShader, Option<u32>), ShaderError>| matches!(r, Ok((_, wave)) if *wave != Some(64));
    // (the program itself where nothing was left out: only the kept branches differ)
    for (program, kept) in [(early, early_kept), (skips, skips_kept), (deep, deep_kept)] {
        if kept.is_empty() {
            continue;
        }
        let base = program.as_ref().unwrap_or(p);
        hang::audit_from_now_on();
        let r = widen_seeded(c, native, base, why, false, &kept);
        hang::stop_auditing();
        if !done(&r) {
            continue;
        }
        let classes = wide_class::last();
        // (the classes are an argument about the branches; the audit looks at the control flow
        // that was emitted: no group barrier may stand where some thread of the group can
        // fail to come, `Mode::Classes` leaves the audit out)
        let audit = hang::last_audit();
        let audited = match mode {
            Mode::Safe => audit.as_ref().is_none_or(|a| a.safe(true)),
            Mode::Strict => audit.as_ref().is_none_or(|a| a.safe(false)),
            _ => true,
        };
        let safe = mode == Mode::All || wide_class::safe(&classes) && audited;
        if why {
            eprintln!(
                "WAVE64-WIDE-CLASS ~{} #{:07x} threads={} {} audit({}) {}",
                code_digits(c),
                super::lower::program_tag() & 0x0FFF_FFFF,
                match native.io {
                    crate::native::StageIo::Cs(cs) => cs.threads.iter().product::<u32>(),
                    _ => 0,
                },
                wide_class::describe(&classes),
                audit.as_ref().map_or("no barriers".to_string(), |a| a.describe()),
                if safe { "taken" } else { "refused" }
            );
        }
        if safe {
            return Some(r);
        }
    }
    None
}
