#![cfg(windows)]

use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn census_reports_unreadable_files_and_continues_scanning() {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let path = std::env::temp_dir().join(format!(
        "victory-census-unreadable-{}-{}",
        std::process::id(),
        timestamp.as_nanos()
    ));
    fs::create_dir(&path).unwrap();
    let scratch = ScratchDir(path);

    let locked = scratch.0.join("locked.exe");
    let empty = scratch.0.join("empty");
    let short = scratch.0.join("short");
    let text = scratch.0.join("note.txt");
    let readable = scratch.0.join("readable.exe");
    fs::write(&empty, b"").unwrap();
    fs::write(&short, b"M").unwrap();
    fs::write(&text, b"ordinary text").unwrap();
    // The integration test executable is a synthetic PE linked by Rust.
    let fixture = std::env::current_exe().unwrap();
    fs::copy(&fixture, &locked).unwrap();
    fs::copy(fixture, &readable).unwrap();
    let exclusive = OpenOptions::new()
        .write(true)
        .share_mode(0)
        .open(&locked)
        .unwrap();
    assert_eq!(
        File::open(&locked).unwrap_err().raw_os_error(),
        Some(32), // ERROR_SHARING_VIOLATION
    );

    let output = Command::new(env!("CARGO_BIN_EXE_victory"))
        .args(["census", "--strict"])
        .args([&locked, &empty, &short, &text, &readable])
        .output()
        .unwrap();
    drop(exclusive);

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("1 images scanned, 1 unreadable"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("  skipped {}:", locked.display())),
        "{stdout}"
    );
}
