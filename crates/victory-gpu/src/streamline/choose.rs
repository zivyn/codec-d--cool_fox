//! Whether DLSS Super Resolution runs without being asked, and where NVIDIA's
//! files are looked for. Pure: nothing here reads the environment, the file
//! system or DXGI; `streamline` does that and passes the answers in, so every
//! rule is a test.
//!
//! `VICTORY_DLSS` is three-way: `1` forces it on (Streamline's refusal is a
//! warning), `0` forces it off, unset is automatic: on when the title runs as
//! the Series S, frame generation is not asked alone, the adapter is an
//! NVIDIA one and NVIDIA's files are in place; Streamline's own feature check
//! then has the last word (RTX or not is its answer, never the adapter's
//! name), and a "no" there is one info line and the title's own upscaler.
//!
//! NVIDIA's files are looked for in one place, never under `%LOCALAPPDATA%`:
//! `VICTORY_STREAMLINE_DIR` (the folder itself), else `VICTORY_HOME` (its
//! `nvidia-sdk\Streamline\bin\x64`), else `victory-home` beside the folder of
//! `VICTORY_CACHE_DIR` (the cache's drive), else `victory-home` beside the
//! program.

use std::path::{Path, PathBuf};

/// The PCI vendor id DXGI reports for NVIDIA adapters.
pub const NVIDIA_VENDOR_ID: u32 = 0x10DE;

/// The folder (beside the cache folder, or beside the program) that holds
/// `nvidia-sdk` when `VICTORY_HOME` is not set.
pub const HOME_FOLDER: &str = "victory-home";

/// The files DLSS Super Resolution loads (frame generation adds its own).
pub const SR_FILES: [&str; 4] = [
    "sl.interposer.dll",
    "sl.common.dll",
    "sl.dlss.dll",
    "nvngx_dlss.dll",
];

/// What `VICTORY_DLSS` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    Off,
    On,
    /// Not set (or empty): decided by [`choose`].
    Auto,
}

/// `VICTORY_DLSS`: `1`, `on`, `true`, `yes` force it on; `0`, `off`, `false`, `no`
/// force it off; unset or empty is automatic. Anything else is an error, not
/// a silent "off" (a typo must not turn the default off unseen).
pub fn parse_switch(value: Option<&str>) -> Result<Switch, String> {
    let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(Switch::Auto);
    };
    match v.to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "yes" => Ok(Switch::On),
        "0" | "off" | "false" | "no" => Ok(Switch::Off),
        _ => Err(format!(
            "VICTORY_DLSS={v}: not 1 (on), 0 (off) or unset (automatic)"
        )),
    }
}

/// What is known when deciding.
#[derive(Clone, Copy, Debug)]
pub struct Facts {
    pub switch: Switch,
    /// Frame generation was asked (DLSS or FSR 3) without `VICTORY_DLSS`:
    /// that combination keeps the scene at the output's size and stays so.
    pub framegen_asked: bool,
    /// The Series S profile (the one the picture was checked on).
    pub series_s: bool,
    /// The vendor id of the adapter the title will run on; `None` if DXGI gave none.
    pub vendor: Option<u32>,
    /// NVIDIA's files for Super Resolution are all in the folder.
    pub files_present: bool,
}

/// Why DLSS is not tried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// `VICTORY_DLSS=0`.
    Switched,
    /// Not the Series S profile: `VICTORY_DLSS=1` asks for it there.
    Console,
    /// Frame generation was asked and DLSS was not: that is frame generation alone.
    FrameGenAlone,
    NoAdapter,
    NotNvidia,
    NoFiles,
}

/// What to do with DLSS Super Resolution at the start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Asked for: Streamline is loaded and anything it refuses is a warning.
    Forced,
    /// By default: Streamline is loaded and its feature check decides; a
    /// refusal is one info line.
    Try,
    Off(Reason),
}

/// The decision (see the module's text).
pub fn choose(f: &Facts) -> Choice {
    match f.switch {
        Switch::Off => Choice::Off(Reason::Switched),
        Switch::On => Choice::Forced,
        Switch::Auto => {
            if !f.series_s {
                Choice::Off(Reason::Console)
            } else if f.framegen_asked {
                Choice::Off(Reason::FrameGenAlone)
            } else {
                match f.vendor {
                    None => Choice::Off(Reason::NoAdapter),
                    Some(v) if v != NVIDIA_VENDOR_ID => Choice::Off(Reason::NotNvidia),
                    Some(_) if !f.files_present => Choice::Off(Reason::NoFiles),
                    Some(_) => Choice::Try,
                }
            }
        }
    }
}

/// The one line that says why the title's own upscaler runs, if the user
/// should be told (`VICTORY_DLSS=0` is their own doing: no line).
pub fn off_line(reason: Reason, dir: &Path, missing: &[&str]) -> Option<String> {
    let force = "VICTORY_DLSS=1 forces it on";
    Some(match reason {
        Reason::Switched => return None,
        Reason::Console => format!("DLSS Super Resolution is off by default on the Series X profile; {force}"),
        Reason::FrameGenAlone => {
            format!("DLSS Super Resolution stays off with frame generation alone; {force}")
        }
        Reason::NoAdapter => "DLSS Super Resolution is off: no graphics adapter was found; the title's own upscaler runs".into(),
        Reason::NotNvidia => "DLSS Super Resolution is off: the adapter is not an NVIDIA one; the title's own upscaler runs".into(),
        Reason::NoFiles => format!(
            "DLSS Super Resolution is off: NVIDIA's files are not in {} ({} missing); the title's own upscaler runs. \
             Play-VIctory.cmd get-dlss fetches them, VICTORY_HOME or VICTORY_STREAMLINE_DIR names another folder",
            dir.display(),
            missing.join(", ")
        ),
    })
}

/// Where NVIDIA's files and the places that name them are, from the environment.
#[derive(Clone, Copy, Debug, Default)]
pub struct Locations<'a> {
    /// `VICTORY_STREAMLINE_DIR`: the folder of `sl.interposer.dll` itself.
    pub streamline_dir: Option<&'a Path>,
    /// `VICTORY_HOME`.
    pub home: Option<&'a Path>,
    /// The cache folder, if one was named (`VICTORY_CACHE_DIR` or the config file).
    pub cache_dir: Option<&'a Path>,
    /// The folder of the program.
    pub exe_dir: Option<&'a Path>,
}

/// `<home>\nvidia-sdk\Streamline\bin\x64`.
pub fn sdk_dir(home: &Path) -> PathBuf {
    home.join("nvidia-sdk")
        .join("Streamline")
        .join("bin")
        .join("x64")
}

/// The home folder beside the cache folder: `<parent of the cache>\victory-home`
/// (the cache folder itself if it has no parent).
pub fn home_beside(cache_dir: &Path) -> PathBuf {
    cache_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(cache_dir)
        .join(HOME_FOLDER)
}

/// The folders to look in, first first. A named folder (`VICTORY_STREAMLINE_DIR`,
/// then `VICTORY_HOME`) is the only one; without either, the folder beside the
/// cache and then the one beside the program.
pub fn candidates(l: &Locations) -> Vec<PathBuf> {
    if let Some(d) = l.streamline_dir {
        return vec![d.to_path_buf()];
    }
    if let Some(h) = l.home {
        return vec![sdk_dir(h)];
    }
    l.cache_dir
        .map(|c| sdk_dir(&home_beside(c)))
        .into_iter()
        .chain(l.exe_dir.map(|e| sdk_dir(&e.join(HOME_FOLDER))))
        .collect()
}

/// The folder to use: the first with all the files, else the first (so that
/// the message names where they should go), else beside the working folder.
pub fn pick(candidates: &[PathBuf], complete: impl Fn(&Path) -> bool) -> PathBuf {
    candidates
        .iter()
        .find(|d| complete(d))
        .or(candidates.first())
        .cloned()
        .unwrap_or_else(|| sdk_dir(Path::new(HOME_FOLDER)))
}

/// Which of [`SR_FILES`] `present` does not have.
pub fn missing(present: impl Fn(&str) -> bool) -> Vec<&'static str> {
    SR_FILES.iter().copied().filter(|f| !present(f)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            switch: Switch::Auto,
            framegen_asked: false,
            series_s: true,
            vendor: Some(NVIDIA_VENDOR_ID),
            files_present: true,
        }
    }

    #[test]
    fn the_switch_is_three_way() {
        assert_eq!(parse_switch(None), Ok(Switch::Auto));
        assert_eq!(parse_switch(Some("")), Ok(Switch::Auto));
        assert_eq!(parse_switch(Some("  ")), Ok(Switch::Auto));
        assert_eq!(parse_switch(Some("1")), Ok(Switch::On));
        assert_eq!(parse_switch(Some(" ON ")), Ok(Switch::On));
        assert_eq!(parse_switch(Some("0")), Ok(Switch::Off));
        assert_eq!(parse_switch(Some("False")), Ok(Switch::Off));
        let bad = parse_switch(Some("2")).unwrap_err();
        assert!(bad.contains("VICTORY_DLSS=2"), "{bad}");
    }

    #[test]
    fn nvidia_adapter_with_files_tries_by_default() {
        assert_eq!(choose(&facts()), Choice::Try);
    }

    #[test]
    fn zero_forces_off_and_one_forces_on_whatever_else_holds() {
        let none = Facts {
            vendor: None,
            files_present: false,
            series_s: false,
            framegen_asked: true,
            ..facts()
        };
        assert_eq!(
            choose(&Facts {
                switch: Switch::Off,
                ..facts()
            }),
            Choice::Off(Reason::Switched)
        );
        assert_eq!(
            choose(&Facts {
                switch: Switch::Off,
                ..none
            }),
            Choice::Off(Reason::Switched)
        );
        assert_eq!(
            choose(&Facts {
                switch: Switch::On,
                ..facts()
            }),
            Choice::Forced
        );
        assert_eq!(
            choose(&Facts {
                switch: Switch::On,
                ..none
            }),
            Choice::Forced
        );
    }

    #[test]
    fn without_an_nvidia_adapter_or_without_the_files_it_is_off() {
        assert_eq!(
            choose(&Facts {
                vendor: Some(0x1002),
                ..facts()
            }),
            Choice::Off(Reason::NotNvidia)
        );
        assert_eq!(
            choose(&Facts {
                vendor: Some(0x8086),
                ..facts()
            }),
            Choice::Off(Reason::NotNvidia)
        );
        assert_eq!(
            choose(&Facts {
                vendor: None,
                ..facts()
            }),
            Choice::Off(Reason::NoAdapter)
        );
        assert_eq!(
            choose(&Facts {
                files_present: false,
                ..facts()
            }),
            Choice::Off(Reason::NoFiles)
        );
        // No files on a card that is not NVIDIA's: the card is the reason.
        assert_eq!(
            choose(&Facts {
                vendor: Some(0x1002),
                files_present: false,
                ..facts()
            }),
            Choice::Off(Reason::NotNvidia)
        );
    }

    #[test]
    fn the_default_is_the_series_s_profile_and_not_frame_generation_alone() {
        assert_eq!(
            choose(&Facts {
                series_s: false,
                ..facts()
            }),
            Choice::Off(Reason::Console)
        );
        assert_eq!(
            choose(&Facts {
                framegen_asked: true,
                ..facts()
            }),
            Choice::Off(Reason::FrameGenAlone)
        );
    }

    #[test]
    fn every_reason_but_the_users_own_has_one_line() {
        let dir = Path::new(r"G:\victory-home\nvidia-sdk\Streamline\bin\x64");
        assert_eq!(off_line(Reason::Switched, dir, &[]), None);
        for r in [
            Reason::Console,
            Reason::FrameGenAlone,
            Reason::NoAdapter,
            Reason::NotNvidia,
            Reason::NoFiles,
        ] {
            let line = off_line(r, dir, &["sl.dlss.dll"]).unwrap();
            assert!(
                !line.contains('\n') && line.starts_with("DLSS Super Resolution"),
                "{line}"
            );
        }
        let line = off_line(Reason::NoFiles, dir, &["sl.dlss.dll", "nvngx_dlss.dll"]).unwrap();
        assert!(
            line.contains(r"G:\victory-home") && line.contains("sl.dlss.dll, nvngx_dlss.dll"),
            "{line}"
        );
    }

    #[test]
    fn the_search_never_names_the_user_profile_and_a_named_folder_is_alone() {
        let exe = Path::new(r"D:\Games\VIctory");
        let cache = Path::new(r"G:\victory-data\cache");
        let streamline = Path::new(r"E:\sdk\x64");
        let home = Path::new(r"F:\home");
        let all = Locations {
            streamline_dir: Some(streamline),
            home: Some(home),
            cache_dir: Some(cache),
            exe_dir: Some(exe),
        };
        assert_eq!(candidates(&all), vec![PathBuf::from(r"E:\sdk\x64")]);
        let no_sl = Locations {
            streamline_dir: None,
            ..all
        };
        assert_eq!(candidates(&no_sl), vec![sdk_dir(home)]);
        let no_home = Locations {
            home: None,
            ..no_sl
        };
        assert_eq!(
            candidates(&no_home),
            vec![
                sdk_dir(Path::new(r"G:\victory-data\victory-home")),
                sdk_dir(&exe.join(HOME_FOLDER))
            ]
        );
        let only_exe = Locations {
            cache_dir: None,
            ..no_home
        };
        assert_eq!(candidates(&only_exe), vec![sdk_dir(&exe.join(HOME_FOLDER))]);
        assert!(candidates(&Locations::default()).is_empty());
        for d in candidates(&no_home).iter().chain(&candidates(&only_exe)) {
            assert!(
                !d.to_string_lossy().to_ascii_lowercase().contains("appdata"),
                "{}",
                d.display()
            );
        }
    }

    #[test]
    fn the_home_beside_the_cache_is_on_the_cache_drive() {
        assert_eq!(
            home_beside(Path::new(r"G:\victory-data\cache")),
            PathBuf::from(r"G:\victory-data\victory-home")
        );
        assert_eq!(
            home_beside(Path::new(r"D:\VIctory\cache\")),
            PathBuf::from(r"D:\VIctory\victory-home")
        );
        assert_eq!(
            home_beside(Path::new(r"G:\cache")),
            PathBuf::from(r"G:\victory-home")
        );
        // A cache that is a drive's root has no parent: the home is inside it.
        assert_eq!(
            home_beside(Path::new(r"G:\")),
            PathBuf::from(r"G:\victory-home")
        );
        assert_eq!(
            home_beside(Path::new("cache")),
            Path::new("cache").join(HOME_FOLDER)
        );
    }

    #[test]
    fn the_first_complete_folder_wins_else_the_first_names_where_they_go() {
        let a = PathBuf::from("a");
        let b = PathBuf::from("b");
        let both = [a.clone(), b.clone()];
        assert_eq!(pick(&both, |d| d == Path::new("b")), b);
        assert_eq!(pick(&both, |_| true), a);
        assert_eq!(pick(&both, |_| false), a);
        assert_eq!(pick(&[], |_| true), sdk_dir(Path::new(HOME_FOLDER)));
    }

    #[test]
    fn an_empty_folder_misses_every_file_and_a_full_one_none() {
        assert_eq!(missing(|_| false), SR_FILES.to_vec());
        assert!(missing(|_| true).is_empty());
        assert_eq!(missing(|f| f != "nvngx_dlss.dll"), vec!["nvngx_dlss.dll"]);
        // Super Resolution needs the interposer and its common and DLSS plugins and the model.
        for f in [
            "sl.interposer.dll",
            "sl.common.dll",
            "sl.dlss.dll",
            "nvngx_dlss.dll",
        ] {
            assert!(SR_FILES.contains(&f));
        }
    }
}
