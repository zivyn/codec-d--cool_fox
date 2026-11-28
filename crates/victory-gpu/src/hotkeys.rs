//! Diagnostics: hotkeys that change what the title shows while it runs
//! (`VICTORY_HOTKEYS=1`; off by default: no thread, no key is looked at).
//!
//! While one of our windows is in front, with Ctrl and Shift both held (a
//! chord the title does not use; nothing is taken from the title, which keeps
//! seeing every key as before):
//!
//! - Ctrl+Shift+F5: frame generation off, and on again (at the amount the
//!   start asked, 2 if it asked none).
//! - Ctrl+Shift+F6: the colour without the interface (the HUD-less copy) off
//!   and on again.
//! - Ctrl+Shift+F7: the next DLSS model preset (default, j, k, l, m, default).
//! - Ctrl+Shift+F8: a mark in the log (no change).
//!
//! F3 is the stats panel's own (`host/panel.rs`): its last line says frame
//! generation, what is shown and the HUD-less copy while they are there.
//!
//! The DLSS mode (quality, balanced, ...) is not here: the title's scene size
//! is fixed by the image the run mapped (patched for it, `native_1440p.rs`
//! and the upscaler image caches), and DLSS is given that size, so changing
//! the mode with the title running would ask DLSS for a size the title does
//! not render.
//!
//! Every press is a line in the log with its phase number, the seconds since
//! the thread began and the clock's time, so that the frame-rate lines
//! between two of them are one phase. A thread polls (every 15 ms: a frame
//! lasts longer, and a short press would fall between two frames).

use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_F5, VK_F6, VK_F7, VK_F8, VK_SHIFT};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// What a hotkey does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    FrameGen,
    Hudless,
    Preset,
    Mark,
}

/// The keys, in the order of the `down` array [`Edges::step`] takes.
const KEYS: [(u16, Action); 4] =
    [(VK_F5, Action::FrameGen), (VK_F6, Action::Hudless), (VK_F7, Action::Preset), (VK_F8, Action::Mark)];

/// Which keys went down.
#[derive(Default)]
pub(crate) struct Edges {
    held: [bool; KEYS.len()],
}

impl Edges {
    /// The actions whose key went down since the last call, if `chord` (Ctrl
    /// and Shift held, our window in front) holds now. A key already down
    /// before the chord was is not a press, and a held key is one press.
    pub(crate) fn step(&mut self, down: [bool; KEYS.len()], chord: bool) -> Vec<Action> {
        let mut pressed = Vec::new();
        for (i, (_, action)) in KEYS.iter().enumerate() {
            if down[i] && !self.held[i] && chord {
                pressed.push(*action);
            }
            self.held[i] = down[i];
        }
        pressed
    }
}

/// The line that marks a phase: its number, what was done, and when (seconds
/// since the thread began, and the clock's, in seconds).
pub(crate) fn phase_line(phase: u64, what: &str, since: f64, unix: f64) -> String {
    format!("hotkeys phase {phase}: {what} (+{since:.1} s since the hotkeys began, unix {unix:.3})")
}

/// The preset after `current` (`sl::DLSSPreset` numbers): default, j, k, l, m,
/// then default again; any other is taken to the default.
pub(crate) fn next_preset(current: u32) -> u32 {
    match current {
        0 => 10,
        10 => 11,
        11 => 12,
        12 => 13,
        _ => 0,
    }
}

/// The name of a preset for the log and the panel.
pub(crate) fn preset_name(preset: u32) -> &'static str {
    match preset {
        0 => "default",
        5 => "e",
        6 => "f",
        10 => "j",
        11 => "k",
        12 => "l",
        13 => "m",
        _ => "?",
    }
}

fn down(vk: u16) -> bool {
    // SAFETY: a plain query.
    let state = unsafe { GetAsyncKeyState(i32::from(vk)) };
    state < 0
}

/// One of our windows is in front.
fn ours() -> bool {
    let mut process = 0;
    // SAFETY: plain queries; room for the answer.
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut process) };
    process == std::process::id()
}

static PHASE: AtomicU64 = AtomicU64::new(0);

/// Starts the thread, once, if `VICTORY_HOTKEYS=1`. Called where the DLSS and
/// frame generation calls begin (the first upscaling, the first present with
/// frame generation planned); a run without them has no use for it.
pub(crate) fn ensure_started() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if !crate::switch::on("VICTORY_HOTKEYS") {
            return;
        }
        let started = Instant::now();
        let spawned = std::thread::Builder::new().name("victory-hotkeys".into()).spawn(move || {
            let mut edges = Edges::default();
            loop {
                std::thread::sleep(Duration::from_millis(15));
                let keys = KEYS.map(|(vk, _)| down(vk));
                // (Only asked which window is in front when a key is down.)
                let chord = keys.iter().any(|k| *k) && down(VK_CONTROL) && down(VK_SHIFT) && ours();
                for action in edges.step(keys, chord) {
                    run(action, started);
                }
            }
        });
        match spawned {
            Ok(_) => tracing::info!(
                "hotkeys: with one of our windows in front, Ctrl+Shift+F5 frame generation off/on, F6 the colour without the interface off/on, F7 next DLSS model preset, F8 a mark in the log; F3 the stats panel"
            ),
            Err(error) => tracing::warn!(%error, "hotkeys: the thread was not started"),
        }
    });
}

fn run(action: Action, started: Instant) {
    let what = match action {
        Action::FrameGen => {
            if !crate::streamline::framegen_live() {
                "frame generation is not ready in this run (VICTORY_FRAMEGEN=1 with VICTORY_DLSS=1 and VICTORY_THIN=1): nothing changed".to_owned()
            } else {
                let asked = crate::streamline::toggle_framegen();
                match asked {
                    0 => "frame generation OFF".to_owned(),
                    n => format!("frame generation ON, {n} frames shown for each one rendered"),
                }
            }
        }
        Action::Hudless => {
            let on = crate::streamline::toggle_hudless();
            format!("the colour without the interface (HUD-less copy) {}", if on { "ON" } else { "OFF" })
        }
        Action::Preset => {
            let preset = crate::streamline::next_dlss_preset();
            format!("DLSS model preset {}", preset_name(preset))
        }
        Action::Mark => "mark".to_owned(),
    };
    let phase = PHASE.fetch_add(1, Relaxed) + 1;
    let unix = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    let (presents, shown) = crate::streamline::displayed_counts();
    tracing::info!(presents, displayed = shown, "{}", phase_line(phase, &what, started.elapsed().as_secs_f64(), unix));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_press_is_one_action_while_the_chord_is_held() {
        let mut edges = Edges::default();
        // Nothing down.
        assert!(edges.step([false; 4], true).is_empty());
        // F5 goes down with the chord: one press; held: no more; let go and again: another.
        assert_eq!(edges.step([true, false, false, false], true), vec![Action::FrameGen]);
        assert!(edges.step([true, false, false, false], true).is_empty());
        assert!(edges.step([false; 4], true).is_empty());
        assert_eq!(edges.step([true, false, false, false], true), vec![Action::FrameGen]);
        // Two keys at once are two presses, in the order of the keys.
        assert!(edges.step([false; 4], true).is_empty());
        assert_eq!(edges.step([false, true, true, false], true), vec![Action::Hudless, Action::Preset]);
        assert_eq!(edges.step([false, true, true, true], true), vec![Action::Mark]);
    }

    #[test]
    fn a_key_without_the_chord_or_held_before_it_is_no_press() {
        let mut edges = Edges::default();
        // F6 alone (or in another program's window): the title's own, nothing happens.
        assert!(edges.step([false, true, false, false], false).is_empty());
        // The chord comes with the key already down: still not a press.
        assert!(edges.step([false, true, false, false], true).is_empty());
        // Let go, then pressed with the chord.
        assert!(edges.step([false; 4], true).is_empty());
        assert_eq!(edges.step([false, true, false, false], true), vec![Action::Hudless]);
    }

    #[test]
    fn the_presets_come_round() {
        let mut preset = 0;
        let mut seen = Vec::new();
        for _ in 0..5 {
            preset = next_preset(preset);
            seen.push(preset_name(preset));
        }
        assert_eq!(seen, ["j", "k", "l", "m", "default"]);
        // One from the environment that is not in the turn (e) goes to the default.
        assert_eq!(next_preset(5), 0);
        assert_eq!(preset_name(next_preset(99)), "default");
    }

    #[test]
    fn the_phase_line_carries_number_and_times() {
        let line = phase_line(3, "frame generation OFF", 12.34, 1_796_000_000.5);
        assert!(line.starts_with("hotkeys phase 3: frame generation OFF"));
        assert!(line.contains("+12.3 s") && line.contains("unix 1796000000.500"), "{line}");
    }
}
