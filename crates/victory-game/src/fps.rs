//! The frame rate the title aims at.
//!
//! The title's own pacing is set by its system settings file (in its storage
//! folder): a target frame rate, a frame-rate limit and the number of display
//! refreshes a frame stays on screen. A title on a console that reports a 60 Hz
//! display aims at 30 with two refreshes per frame; the fork's layer has no
//! limiter of its own and answers the interval the title sets
//! (`SetFrameIntervalX`) with a clock of that length, so what the title asks is
//! what it gets.
//!
//! `VICTORY_TARGET_FPS=<fps>` (15, 20, 30 or 60) makes the three values of that
//! file what that frame rate needs before the title starts: the target and the
//! limit are `<fps>`, the refreshes per frame are the 60 Hz the layer reports
//! divided by it (`60` is one, `30` is two). Unset, the file is not touched.
//! The first change keeps the file as it was beside it
//! (`settings.xml.victory-orig`; `VICTORY_TARGET_FPS=30` writes the defaults
//! again). Whether the title reads the values, and what it does at 60 (its
//! time step, physics and animation are the title's own), is for a run to say:
//! its first `frame interval` line in the log is the interval it asked for.

use std::io;
use std::path::Path;

/// The switch.
pub const ENV: &str = "VICTORY_TARGET_FPS";

/// The refresh rate the layer reports for the display.
const REFRESH: u32 = 60;

/// The file, under the title's storage folder.
const FILE: [&str; 2] = ["settings", "settings.xml"];

/// The name the file is kept under before the first change.
const KEPT: &str = "settings.xml.victory-orig";

/// The frame rate the switch names: one a whole number of 60 Hz refreshes
/// gives (`60`, `30`, `20`, `15`).
pub fn parse(value: Option<&str>) -> Option<u32> {
    let fps: u32 = value?.trim().parse().ok()?;
    matches!(fps, 15 | 20 | 30 | 60).then_some(fps)
}

/// The display refreshes a frame stays for at `fps`.
pub fn refreshes(fps: u32) -> u32 {
    REFRESH / fps.max(1)
}

/// The values the file gets for `fps`: `(key, value)`.
fn wanted(fps: u32) -> [(&'static str, u32); 3] {
    [
        ("desiredFPS", fps),
        ("frameRateLimit", fps),
        ("vSync", refreshes(fps)),
    ]
}

/// Where the `value` of element `key` is in `xml`, if the element is there
/// exactly once.
fn value_at(xml: &str, key: &str) -> Option<std::ops::Range<usize>> {
    let open = format!("<{key} ");
    let at = xml.find(&open)?;
    let after = at + open.len();
    if xml[after..].contains(&open) {
        return None;
    }
    let end = after + xml[after..].find('>')?;
    let from = after + xml[after..end].find("value=\"")? + "value=\"".len();
    let len = xml[from..end].find('"')?;
    Some(from..from + len)
}

/// `xml` with the values of `fps`; `Ok(None)` if it has them already, and the
/// name of the element that is missing (or there twice, or not shaped as
/// `<name value="n" />`) as the error.
pub fn retarget(xml: &str, fps: u32) -> Result<Option<String>, &'static str> {
    let mut text = xml.to_owned();
    for (key, value) in wanted(fps) {
        let range = value_at(&text, key).ok_or(key)?;
        text.replace_range(range, &value.to_string());
    }
    Ok((text != xml).then_some(text))
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// There is no settings file yet (the title writes it on its first run).
    NoFile,
    /// It has the values already.
    Same,
    /// This element is not in it once, shaped as expected: left as it was.
    Unreadable(&'static str),
    /// Rewritten.
    Changed,
}

/// Gives the title's settings file under `storage` the values of `fps`.
pub fn apply(storage: &Path, fps: u32) -> io::Result<Applied> {
    let path = FILE
        .iter()
        .fold(storage.to_path_buf(), |p, part| p.join(part));
    let xml = match std::fs::read_to_string(&path) {
        Ok(xml) => xml,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Applied::NoFile),
        Err(e) => return Err(e),
    };
    let text = match retarget(&xml, fps) {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(Applied::Same),
        Err(key) => return Ok(Applied::Unreadable(key)),
    };
    let kept = path.with_file_name(KEPT);
    if !kept.exists() {
        std::fs::write(&kept, &xml)?;
    }
    let new = path.with_file_name("settings.xml.victory-new");
    std::fs::write(&new, text)?;
    std::fs::rename(&new, &path)?;
    Ok(Applied::Changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A settings file as the title writes one: what is not ours around the
    /// three elements is made up.
    fn file(target: u32, limit: u32, sync: u32) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<collection>\n  <graphics>\n    \
             <gamma value=\"15\" />\n    <desiredFPS value=\"{target}\" />\n    \
             <other value=\"1\" />\n  </graphics>\n  <video>\n    \
             <frameRateLimit value=\"{limit}\" />\n    <vSync value=\"{sync}\" />\n  \
             </video>\n</collection>"
        )
    }

    #[test]
    fn the_switch_names_whole_refreshes() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some(" 60 ")), Some(60));
        assert_eq!(parse(Some("30")), Some(30));
        assert_eq!(parse(Some("20")), Some(20));
        assert_eq!(parse(Some("15")), Some(15));
        // 40 and 120 need a 120 Hz display; 45 and 0 fit no refresh.
        for no in ["40", "120", "45", "0", "", "sixty", "-30"] {
            assert_eq!(parse(Some(no)), None, "{no}");
        }
        assert_eq!(
            [60, 30, 20, 15].map(refreshes),
            [1, 2, 3, 4],
            "frames of 16.7, 33.3, 50 and 66.7 ms at 60 Hz"
        );
    }

    #[test]
    fn sixty_changes_the_three_values_and_nothing_else() {
        let new = retarget(&file(30, 30, 2), 60).unwrap().unwrap();
        assert_eq!(new, file(60, 60, 1));
        assert!(new.contains("<gamma value=\"15\" />") && new.contains("<other value=\"1\" />"));
    }

    #[test]
    fn thirty_writes_the_defaults_back() {
        let new = retarget(&file(60, 60, 1), 30).unwrap().unwrap();
        assert_eq!(new, file(30, 30, 2));
    }

    #[test]
    fn the_values_it_has_are_left_alone() {
        assert_eq!(retarget(&file(60, 60, 1), 60), Ok(None));
        // One of three off: the file changes, and only that value.
        let new = retarget(&file(60, 30, 1), 60).unwrap().unwrap();
        assert_eq!(new, file(60, 60, 1));
    }

    #[test]
    fn a_file_that_is_not_as_expected_is_not_touched() {
        let without = file(30, 30, 2).replace("<vSync value=\"2\" />", "");
        assert_eq!(retarget(&without, 60), Err("vSync"));
        let twice = file(30, 30, 2).replace("</video>", "<vSync value=\"2\" />\n</video>");
        assert_eq!(retarget(&twice, 60), Err("vSync"));
        let bare = file(30, 30, 2).replace("<desiredFPS value=\"30\" />", "<desiredFPS />");
        assert_eq!(retarget(&bare, 60), Err("desiredFPS"));
        assert_eq!(retarget("", 60), Err("desiredFPS"));
        // A name that only starts like ours is not ours.
        let longer = file(30, 30, 2).replace("<vSync ", "<vSyncMode ");
        assert_eq!(retarget(&longer, 60), Err("vSync"));
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("victory-fps-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("settings")).unwrap();
        dir
    }

    #[test]
    fn the_file_is_kept_once_and_rewritten() {
        let dir = scratch("apply");
        let path = dir.join("settings").join("settings.xml");
        let kept = dir.join("settings").join(KEPT);
        std::fs::write(&path, file(30, 30, 2)).unwrap();

        assert_eq!(apply(&dir, 60).unwrap(), Applied::Changed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file(60, 60, 1));
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), file(30, 30, 2));
        assert!(
            !dir.join("settings")
                .join("settings.xml.victory-new")
                .exists()
        );

        assert_eq!(apply(&dir, 60).unwrap(), Applied::Same);
        // Back to 30: the copy of the first change is not overwritten.
        assert_eq!(apply(&dir, 30).unwrap(), Applied::Changed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file(30, 30, 2));
        std::fs::write(&path, file(40, 40, 3)).unwrap();
        assert_eq!(apply(&dir, 30).unwrap(), Applied::Changed);
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), file(30, 30, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_file_and_unreadable_files_are_reported_and_left() {
        let dir = scratch("none");
        assert_eq!(apply(&dir, 60).unwrap(), Applied::NoFile);
        let path = dir.join("settings").join("settings.xml");
        std::fs::write(&path, "<collection />").unwrap();
        assert_eq!(apply(&dir, 60).unwrap(), Applied::Unreadable("desiredFPS"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "<collection />");
        assert!(!dir.join("settings").join(KEPT).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
