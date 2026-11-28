//! XGameSave: the user's game saves, kept on the host under the title's
//! data directory, next to persistent local storage (`runtime::Title::
//! saves`), never in the cache. A provider is a folder per user and
//! service configuration, a container a folder in it.
//!
//! An update is all or nothing, as documented: each one writes a complete
//! new version of the container (`v<N>.new/`, unchanged blobs hard-linked
//! from the previous version), flushed to disk, and one directory rename
//! commits it (`v<N>`); the highest committed version is the container.
//! A crash leaves the old version or the new one, never a mix; leftovers
//! are removed on sight. Deleting a container renames it away first.
//!
//! Names are exact: a version's `manifest` holds the container's name, its
//! display name and the blob names (blob `i` is the file `i`), so case,
//! characters the host forbids, reserved device names and length never
//! reach a file name. A container's folder is its name made safe (and
//! shortened) plus a hash of the exact name, so names differing only in
//! case get different folders. Offline there is no cloud copy, so nothing
//! ever needs a sync.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CString, c_char, c_void};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::xasync::{self, AsyncBlock, E_INVALIDARG};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, text,
};
use super::{storage, xuser};
use crate::runtime;

pub const PROVIDER: Guid = Guid::new(
    0x704c_3f58,
    0xe629,
    0x4cc2,
    [0xb1, 0x97, 0x30, 0x51, 0x1b, 0x99, 0x6f, 0xe2],
);
/// As the title (2026) asks for it; every slot it calls takes
/// the public API's arguments in the same order as under the provider's
/// own id (older titles).
pub const IID_2026: Guid = Guid::new(
    0x1bff_f3af,
    0xf14a,
    0x40a3,
    [0x8e, 0x35, 0x9a, 0xda, 0x90, 0x65, 0x93, 0xf9],
);

pub static OBJECT: Object = Object::new("game save", &VTABLE);
pub const INTERFACES: &[Interface] =
    &[(PROVIDER, PROVIDER, &OBJECT), (PROVIDER, IID_2026, &OBJECT)];

// The public API's error codes.
pub const INVALID_CONTAINER_NAME: HRESULT = 0x8083_0001_u32 as i32;
pub const NO_ACCESS: HRESULT = 0x8083_0002_u32 as i32;
pub const OUT_OF_LOCAL_STORAGE: HRESULT = 0x8083_0003_u32 as i32;
pub const UPDATE_TOO_BIG: HRESULT = 0x8083_0005_u32 as i32;
pub const QUOTA_EXCEEDED: HRESULT = 0x8083_0006_u32 as i32;
pub const BUFFER_TOO_SMALL: HRESULT = 0x8083_0007_u32 as i32;
pub const BLOB_NOT_FOUND: HRESULT = 0x8083_0008_u32 as i32;

/// Per user and title, as documented.
const QUOTA: u64 = 256 << 20;
/// What one update may write in all, as documented.
const MAX_UPDATE: usize = 16 << 20;
const MANIFEST: &str = "manifest";
/// Readable part of a container's folder name, at most.
const READABLE: usize = 100;

/// Every save operation, so none sees another's half-made version.
static SAVES: Mutex<()> = Mutex::new(());

fn saves_lock() -> std::sync::MutexGuard<'static, ()> {
    SAVES.lock().unwrap_or_else(|e| e.into_inner())
}

/// `name` as a file name: letters, digits, `-` and `_` stay, every other
/// byte becomes `%XX`. The result never contains a dot.
fn file_name(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The name behind a file name from [`file_name`]; `None` for other files.
fn decode(file: &str) -> Option<String> {
    if file.contains('.') {
        return None;
    }
    let mut out = Vec::new();
    let mut bytes = file.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

/// The container's folder: its name made safe, cut short, then `~` and a
/// hash of the exact name (64-bit FNV-1a, stable across builds).
fn container_dir(root: &Path, name: &str) -> PathBuf {
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    });
    let mut readable = file_name(name);
    readable.truncate(READABLE); // ASCII only
    root.join(format!("{readable}~{hash:016x}"))
}

/// Documented: the path part (up to the last `/`) letters, digits, `_` and
/// `/`; the file part letters, digits, `_`, `.` and `-`, not empty, not
/// ending in `.`, no `..`; 256 characters at most.
fn valid_container(name: &str) -> bool {
    let (path, file) = name.rsplit_once('/').unwrap_or(("", name));
    name.len() <= 256
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'/')
        && !file.is_empty()
        && !file.ends_with('.')
        && !file.contains("..")
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A prefix of a container name: the name's characters, in any order (empty
/// is every container).
fn valid_prefix(prefix: &str) -> bool {
    prefix.len() <= 256
        && prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'/' | b'.' | b'-'))
}

/// A committed version of a container.
struct Version {
    dir: PathBuf,
    /// The highest version number any entry in the container's folder
    /// carries, readable or not; the next commit goes above it.
    highest: u64,
    name: String,
    display: String,
    /// Blob names; blob `i` is the file `i`.
    blobs: Vec<String>,
}

impl Version {
    fn blob(&self, i: usize) -> PathBuf {
        self.dir.join(i.to_string())
    }

    fn sizes(&self) -> Vec<(String, u64)> {
        let size = |i| std::fs::metadata(self.blob(i)).map_or(0, |m| m.len());
        let blobs = self.blobs.iter().enumerate();
        blobs.map(|(i, n)| (n.clone(), size(i))).collect()
    }
}

/// The manifest: length-prefixed strings (u32 LE): container name, display
/// name, then each blob name.
fn manifest(name: &str, display: &str, blobs: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for s in [name, display].into_iter().chain(blobs.iter().copied()) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    out
}

fn parse_manifest(bytes: &[u8]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (len, tail) = rest.split_first_chunk::<4>()?;
        let len = u32::from_le_bytes(*len) as usize;
        out.push(String::from_utf8(tail.get(..len)?.to_vec()).ok()?);
        rest = &tail[len..];
    }
    (out.len() >= 2).then_some(out)
}

/// The number of a version folder exactly as [`commit`] names it (`v1`,
/// `v2`, ...); `None` for anything else, such as `v01`, `v+1` or `v0`.
fn version_number(file: &str) -> Option<u64> {
    let n = file.strip_prefix('v')?.parse::<u64>().ok()?;
    (n > 0 && format!("v{n}") == file).then_some(n)
}

/// The names in version folder `dir`: container, display, blobs; `None` if
/// its manifest is missing or does not parse.
fn load(dir: &Path) -> std::io::Result<Option<(String, String, Vec<String>)>> {
    let bytes = match std::fs::read(dir.join(MANIFEST)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let Some(mut names) = parse_manifest(&bytes) else {
        return Ok(None);
    };
    let blobs = names.split_off(2);
    let [name, display] = <[String; 2]>::try_from(names).expect("two names, checked by the parser");
    Ok(Some((name, display, blobs)))
}

/// The container's newest readable committed version, if any. Removes what
/// a crash left: unfinished versions, and versions older than that one.
/// An unreadable newer folder (a stray, or a broken manifest) is neither
/// taken nor removed, and never costs the version below it.
fn current(dir: &Path) -> std::io::Result<Option<Version>> {
    let mut committed = Vec::new();
    let mut highest = 0;
    for entry in std::fs::read_dir(dir)?.flatten() {
        let file = entry.file_name();
        let file = file.to_string_lossy();
        if file.ends_with(".new") {
            std::fs::remove_dir_all(entry.path())?;
        } else if let Some(n) = version_number(&file) {
            highest = highest.max(n);
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                committed.push(n);
            }
        }
    }
    committed.sort_unstable_by(|a, b| b.cmp(a));
    if committed.is_empty() {
        return Ok(None);
    }
    for (at, &number) in committed.iter().enumerate() {
        let version = dir.join(format!("v{number}"));
        let Some((name, display, blobs)) = load(&version)? else {
            continue;
        };
        for old in &committed[at + 1..] {
            std::fs::remove_dir_all(dir.join(format!("v{old}")))?;
        }
        return Ok(Some(Version {
            dir: version,
            highest,
            name,
            display,
            blobs,
        }));
    }
    // Versions exist but none reads: keep them all, and refuse.
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "no save version has a readable manifest",
    ))
}

/// The version of container `name` in `dir`; none for a missing container
/// or another name that hashes alike.
fn current_named(dir: &Path, name: &str) -> std::io::Result<Option<Version>> {
    match current(dir) {
        Ok(v) => Ok(v.filter(|v| v.name == name)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Where a crash test stops the process (test builds only).
fn step() {
    #[cfg(test)]
    tests::crash_point();
}

/// A blob of a new version: new data, or the previous version's file.
enum Source<'a> {
    Data(&'a [u8]),
    Kept(PathBuf),
}

/// Writes version `number` of the container in `dir` and commits it.
fn commit(
    dir: &Path,
    number: u64,
    name: &str,
    display: &str,
    blobs: &[(&str, Source<'_>)],
) -> std::io::Result<()> {
    let staged = dir.join(format!("v{number}.new"));
    if staged.exists() {
        std::fs::remove_dir_all(&staged)?;
    }
    std::fs::create_dir_all(&staged)?;
    let write = |path: PathBuf, data: &[u8]| -> std::io::Result<()> {
        let mut file = std::fs::File::create(path)?;
        file.write_all(data)?;
        file.sync_all()
    };
    for (i, (_, source)) in blobs.iter().enumerate() {
        let path = staged.join(i.to_string());
        match source {
            Source::Data(data) => write(path, data)?,
            // Files are never changed in place, so sharing one is safe.
            Source::Kept(from) => {
                std::fs::hard_link(from, &path).or_else(|_| write(path, &std::fs::read(from)?))?
            }
        }
        step();
    }
    let names: Vec<&str> = blobs.iter().map(|(n, _)| *n).collect();
    write(staged.join(MANIFEST), &manifest(name, display, &names))?;
    step();
    std::fs::rename(&staged, dir.join(format!("v{number}")))?;
    step();
    Ok(())
}

/// The containers under a provider's root, by exact name; containers in
/// the old one-file-per-blob layout are converted first.
fn containers(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        let file = entry.file_name().to_string_lossy().into_owned();
        if !path.is_dir() {
            continue;
        }
        if file.ends_with(".gone") {
            let _ = std::fs::remove_dir_all(&path);
        } else if file.contains('~') {
            if let Ok(Some(v)) = current(&path) {
                out.push((v.name, path));
            }
        } else if let Some(name) = decode(&file)
            && let Some(dir) = migrate(root, &name, &path)
        {
            out.push((name, dir));
        }
    }
    out.sort();
    out
}

/// Moves a container of the old layout (blob files named by encoded name,
/// a `.display` file) into a committed version, then removes it.
fn migrate(root: &Path, name: &str, old: &Path) -> Option<PathBuf> {
    let dir = container_dir(root, name);
    let done = current_named(&dir, name).ok()?.is_some(); // a crash after the commit
    if !done {
        let display = std::fs::read_to_string(old.join(".display")).unwrap_or_default();
        let mut blobs: Vec<(String, Vec<u8>)> = std::fs::read_dir(old)
            .ok()?
            .flatten()
            .filter_map(|e| {
                let blob = decode(e.file_name().to_str()?)?;
                Some((blob, std::fs::read(e.path()).ok()?))
            })
            .collect();
        blobs.sort();
        let sources: Vec<_> = blobs
            .iter()
            .map(|(n, d)| (n.as_str(), Source::Data(d)))
            .collect();
        commit(&dir, 1, name, &display, &sources).ok()?;
    }
    std::fs::remove_dir_all(old).ok()?;
    tracing::info!(container = name, "game save moved to the versioned layout");
    Some(dir)
}

fn used_bytes(root: &Path) -> u64 {
    containers(root)
        .iter()
        .filter_map(|(name, dir)| current_named(dir, name).ok().flatten())
        .map(|v| v.sizes().iter().map(|(_, size)| size).sum::<u64>())
        .sum()
}

struct Provider {
    root: PathBuf,
}
struct Container {
    name: String,
    dir: PathBuf,
}
struct Update {
    name: String,
    dir: PathBuf,
    display: Option<String>,
    /// Blob writes (`Some`) and deletions (`None`), in order.
    changes: Vec<(String, Option<Vec<u8>>)>,
}

/// A handle we gave out.
fn get<'a, T>(handle: *const T) -> Option<&'a T> {
    // SAFETY: handles are boxes we created and the title has not closed.
    unsafe { handle.as_ref() }
}
fn give<T>(value: T) -> *mut T {
    Box::into_raw(Box::new(value))
}
fn take<T>(handle: *mut T) {
    if !handle.is_null() {
        // SAFETY: as in `get`; closing ends the handle.
        drop(unsafe { Box::from_raw(handle) });
    }
}

impl Provider {
    /// The provider for the only user and `config` (the service
    /// configuration id), under `saves`. An empty id names no
    /// configuration, like none: as a folder name it would be the user's
    /// folder itself, where the other configurations' folders are.
    fn open(saves: &Path, config: Option<&str>) -> Result<Provider, HRESULT> {
        let config = config.filter(|c| !c.is_empty()).unwrap_or("default");
        let root = saves
            .join(format!("{:016X}", xuser::XUID))
            .join(file_name(config));
        std::fs::create_dir_all(&root).map_err(|_| NO_ACCESS)?;
        let _saves = saves_lock();
        let found = containers(&root); // converts old containers now
        let names: Vec<&str> = found.iter().map(|(n, _)| n.as_str()).take(32).collect();
        tracing::info!(
            root = %root.display(),
            config,
            containers = found.len(),
            ?names,
            "game saves opened"
        );
        Ok(Provider { root })
    }

    fn remaining(&self) -> u64 {
        let _saves = saves_lock();
        remaining(&self.root)
    }

    fn container(&self, name: &str) -> Container {
        Container {
            name: name.to_owned(),
            dir: container_dir(&self.root, name),
        }
    }
}

/// Quota left under a provider's root, or less if the host disk is fuller.
fn remaining(root: &Path) -> u64 {
    QUOTA
        .saturating_sub(used_bytes(root))
        .min(storage::disk_free(root))
}

/// Why an update writing `written` bytes, `replaced` of them over blobs it
/// replaces, does not fit: past the per-user quota (`used` bytes kept), or
/// past the host disk's `free` bytes, which is not the user's quota (the
/// new version is written before the old one goes, so nothing it replaces
/// counts there).
fn no_room(written: u64, replaced: u64, used: u64, free: u64) -> Option<HRESULT> {
    if written > QUOTA.saturating_sub(used) + replaced {
        Some(QUOTA_EXCEEDED)
    } else if written > free {
        Some(OUT_OF_LOCAL_STORAGE)
    } else {
        None
    }
}

fn io_error(dir: &Path, e: &std::io::Error) -> HRESULT {
    tracing::warn!(dir = %dir.display(), %e, "game save");
    if e.kind() == std::io::ErrorKind::StorageFull {
        OUT_OF_LOCAL_STORAGE
    } else {
        NO_ACCESS
    }
}

impl Update {
    /// Applies the update as one new version of the container.
    fn submit(&self) -> HRESULT {
        let hr = self.apply();
        let written: usize = self
            .changes
            .iter()
            .map(|(_, d)| d.as_ref().map_or(0, Vec::len))
            .sum();
        let deleted = self.changes.iter().filter(|(_, d)| d.is_none()).count();
        tracing::info!(
            container = self.name,
            blobs = self.changes.len() - deleted,
            deleted,
            bytes = written,
            hr = format_args!("{:#010x}", hr as u32),
            "game save update submitted"
        );
        hr
    }

    fn apply(&self) -> HRESULT {
        let _saves = saves_lock();
        let current = match current(&self.dir) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return io_error(&self.dir, &e),
        };
        if current.as_ref().is_some_and(|v| v.name != self.name) {
            // Another container's name hashes alike: never mix the two.
            return NO_ACCESS;
        }
        // The blobs after the update: name → new data or the kept file.
        let mut blobs: Vec<(&str, Source<'_>)> = current
            .iter()
            .flat_map(|v| {
                let kept = v.blobs.iter().enumerate();
                kept.map(|(i, n)| (n.as_str(), Source::Kept(v.blob(i))))
            })
            .collect();
        for (name, data) in &self.changes {
            blobs.retain(|(n, _)| n != name);
            if let Some(data) = data {
                blobs.push((name, Source::Data(data)));
            }
        }
        blobs.sort_by(|a, b| a.0.cmp(b.0));
        let written: usize = blobs
            .iter()
            .map(|(_, s)| match s {
                Source::Data(d) => d.len(),
                Source::Kept(_) => 0,
            })
            .sum();
        if written > MAX_UPDATE {
            return UPDATE_TOO_BIG;
        }
        let replaced: u64 = current.as_ref().map_or(0, |v| {
            let sizes = v.sizes();
            let changed = sizes
                .iter()
                .filter(|(n, _)| self.changes.iter().any(|(c, _)| c == n));
            changed.map(|(_, size)| size).sum()
        });
        let root = self.dir.parent().unwrap_or(&self.dir);
        let free = storage::disk_free(root);
        if let Some(hr) = no_room(written as u64, replaced, used_bytes(root), free) {
            return hr;
        }
        let display = self.display.as_deref();
        let display = display.or(current.as_ref().map(|v| v.display.as_str()));
        let number = current.as_ref().map_or(1, |v| v.highest + 1);
        if let Err(e) = commit(&self.dir, number, &self.name, display.unwrap_or(""), &blobs) {
            return io_error(&self.dir, &e);
        }
        // The old version is garbage now; a crash here leaves it for later.
        if let Some(old) = &current {
            let _ = std::fs::remove_dir_all(&old.dir);
            step();
        }
        S_OK
    }
}

/// Deletes a container: renamed away (one step), then removed.
fn delete(root: &Path, name: &str) -> HRESULT {
    let hr = remove(root, name);
    tracing::info!(
        container = name,
        hr = format_args!("{:#010x}", hr as u32),
        "game save container deleted"
    );
    hr
}

fn remove(root: &Path, name: &str) -> HRESULT {
    let _saves = saves_lock();
    let dir = container_dir(root, name);
    match current_named(&dir, name) {
        Ok(Some(_)) => {}
        Ok(None) => return S_OK,
        Err(e) => return io_error(&dir, &e),
    }
    let gone = dir.with_extension("gone");
    let _ = std::fs::remove_dir_all(&gone);
    step();
    if let Err(e) = std::fs::rename(&dir, &gone) {
        return io_error(&dir, &e);
    }
    step();
    let _ = std::fs::remove_dir_all(&gone);
    S_OK
}

/// Reads the named blobs (all of them for `None`) of a container.
fn read(c: &Container, names: Option<Vec<String>>) -> Result<Vec<(String, Vec<u8>)>, HRESULT> {
    let asked = names.as_ref().map(Vec::len);
    let result = read_blobs_of(c, names);
    match &result {
        Ok(blobs) => tracing::info!(
            container = c.name,
            asked,
            blobs = blobs.len(),
            bytes = blobs.iter().map(|(_, d)| d.len()).sum::<usize>(),
            "game save read"
        ),
        // A container the title probes for and does not have is normal.
        Err(hr) => tracing::info!(
            container = c.name,
            asked,
            hr = format_args!("{:#010x}", *hr as u32),
            "game save read failed"
        ),
    }
    result
}

fn read_blobs_of(
    c: &Container,
    names: Option<Vec<String>>,
) -> Result<Vec<(String, Vec<u8>)>, HRESULT> {
    let _saves = saves_lock();
    let version = current_named(&c.dir, &c.name)
        .map_err(|e| io_error(&c.dir, &e))?
        .ok_or(BLOB_NOT_FOUND)?;
    let names = names.unwrap_or_else(|| version.blobs.clone());
    names
        .into_iter()
        .map(|name| {
            let i = version
                .blobs
                .iter()
                .position(|b| *b == name)
                .ok_or(BLOB_NOT_FOUND)?;
            let data = std::fs::read(version.blob(i)).map_err(|e| io_error(&c.dir, &e))?;
            Ok((name, data))
        })
        .collect()
}

/// One blob as the public API lays it out: name, size, data.
const BLOB: usize = 24;

/// Blobs laid out as the title reads them: the blob array, then each name
/// (NUL-terminated) and its data, with pointers `base` plus offset.
fn layout(blobs: &[(String, Vec<u8>)], base: usize) -> Vec<u8> {
    let mut out = vec![0u8; blobs.len() * BLOB];
    for (i, (name, data)) in blobs.iter().enumerate() {
        let name_at = base + out.len();
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        let data_at = base + out.len();
        out.extend_from_slice(data);
        let entry = &mut out[i * BLOB..(i + 1) * BLOB];
        entry[..8].copy_from_slice(&name_at.to_le_bytes());
        entry[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        entry[16..].copy_from_slice(&data_at.to_le_bytes());
    }
    out
}

/// The names a title passes (`count` strings, or all blobs if null).
fn names(names: *const *const c_char, count: u32) -> Option<Vec<String>> {
    if names.is_null() {
        return None;
    }
    // SAFETY: the title passes `count` strings.
    let names = unsafe { std::slice::from_raw_parts(names, count as usize) };
    Some(names.iter().map(|&n| text(n).unwrap_or_default()).collect())
}

type This = *const Object;
type ContainerCallback = extern "system" fn(*const ContainerInfo, *mut c_void) -> bool;
type BlobCallback = extern "system" fn(*const BlobInfo, *mut c_void) -> bool;

#[repr(C)]
pub struct ContainerInfo {
    pub name: *const c_char,
    pub display_name: *const c_char,
    pub blob_count: u32,
    pub total_size: u64,
    pub last_modified: i64,
    pub needs_sync: bool,
}

#[repr(C)]
pub struct BlobInfo {
    pub name: *const c_char,
    pub size: u32,
}

/// Calls `callback` with the information of each container that passes
/// `keep` until it returns false; never under our lock (it may call us).
fn report(
    root: &Path,
    keep: impl Fn(&str) -> bool,
    context: *mut c_void,
    callback: ContainerCallback,
) -> usize {
    let infos: Vec<(CString, CString, u32, u64, i64)> = {
        let _saves = saves_lock();
        containers(root)
            .into_iter()
            .filter(|(name, _)| keep(name))
            .filter_map(|(name, dir)| current_named(&dir, &name).ok().flatten())
            .map(|v| {
                let modified = std::fs::metadata(v.dir.join(MANIFEST))
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs() as i64);
                let size = v.sizes().iter().map(|(_, s)| s).sum();
                let count = v.blobs.len() as u32;
                (cstring(&v.name), cstring(&v.display), count, size, modified)
            })
            .collect()
    };
    let count = infos.len();
    for (name, display, blob_count, total_size, last_modified) in &infos {
        let info = ContainerInfo {
            name: name.as_ptr(),
            display_name: display.as_ptr(),
            blob_count: *blob_count,
            total_size: *total_size,
            last_modified: *last_modified,
            needs_sync: false,
        };
        if !callback(&info, context) {
            break;
        }
    }
    count
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

static INIT: u8 = 0;
static QUOTA_CALL: u8 = 0;
static DELETE: u8 = 0;
static READ: u8 = 0;
static SUBMIT: u8 = 0;

/// The provider for the title's only user, under `saves`; none while the
/// title has no storage folder (nothing may land in the working directory).
fn open_at(saves: &Path, config: Option<&str>) -> Result<Provider, HRESULT> {
    if saves.parent().is_none_or(|p| p.as_os_str().is_empty()) {
        Err(NO_ACCESS)
    } else {
        Provider::open(saves, config)
    }
}

fn open_provider(config: *const c_char) -> Result<Provider, HRESULT> {
    open_at(&runtime::title().saves(), text(config).as_deref())
}

/// `XGameSaveInitializeProvider`: the same, answered at once.
extern "system" fn initialize(
    _: This,
    _user: *const c_void,
    config: *const c_char,
    _sync_on_demand: bool,
    out: *mut *mut Provider,
) -> HRESULT {
    match open_provider(config) {
        Ok(provider) => put(out, give(provider)),
        Err(hr) => hr,
    }
}
extern "system" fn initialize_async(
    _: This,
    _user: *const c_void,
    config: *const c_char,
    _sync_on_demand: bool,
    block: *mut AsyncBlock,
) -> HRESULT {
    let result = open_provider(config).map(|p| xasync::bytes(give(p)));
    xasync::finished_with(block, (&raw const INIT).cast(), result)
}
extern "system" fn initialize_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut *mut Provider,
) -> HRESULT {
    xasync::result(block, (&raw const INIT).cast(), out)
}
extern "system" fn close_provider(_: This, provider: *mut Provider) {
    take(provider);
}

/// `XGameSaveGetRemainingQuota`, answered at once.
extern "system" fn quota(_: This, provider: *const Provider, out: *mut i64) -> HRESULT {
    match get(provider) {
        Some(p) => put(out, p.remaining() as i64),
        None => E_INVALIDARG,
    }
}
extern "system" fn quota_async(
    _: This,
    provider: *const Provider,
    block: *mut AsyncBlock,
) -> HRESULT {
    let result = get(provider)
        .ok_or(E_INVALIDARG)
        .map(|p| xasync::bytes(p.remaining() as i64));
    xasync::finished_with(block, (&raw const QUOTA_CALL).cast(), result)
}
extern "system" fn quota_result(_: This, block: *mut AsyncBlock, out: *mut i64) -> HRESULT {
    xasync::result(block, (&raw const QUOTA_CALL).cast(), out)
}

fn delete_named(provider: *const Provider, name: *const c_char) -> HRESULT {
    match (get(provider), text(name)) {
        (Some(p), Some(name)) if valid_container(&name) => delete(&p.root, &name),
        (Some(_), Some(_)) => INVALID_CONTAINER_NAME,
        _ => E_INVALIDARG,
    }
}

/// `XGameSaveDeleteContainer`, answered at once.
extern "system" fn delete_now(_: This, provider: *const Provider, name: *const c_char) -> HRESULT {
    delete_named(provider, name)
}
extern "system" fn delete_async(
    _: This,
    provider: *const Provider,
    name: *const c_char,
    block: *mut AsyncBlock,
) -> HRESULT {
    let hr = delete_named(provider, name);
    xasync::finished(block, (&raw const DELETE).cast(), hr)
}
extern "system" fn delete_result(_: This, block: *mut AsyncBlock) -> HRESULT {
    let none = std::ptr::null_mut();
    xasync::XAsyncGetResult(block, (&raw const DELETE).cast(), 0, none, none.cast())
}

/// The containers whose names start with the text the title passes, the one
/// named exactly first (a title asking for one container gets it; one asking
/// for a family of them, such as every slot, gets the family); a missing
/// one is no error, the callback just does not run.
extern "system" fn container_info(
    _: This,
    provider: *const Provider,
    prefix: *const c_char,
    context: *mut c_void,
    callback: Option<ContainerCallback>,
) -> HRESULT {
    let (Some(p), Some(prefix), Some(callback)) = (get(provider), text(prefix), callback) else {
        return E_INVALIDARG;
    };
    if !valid_prefix(&prefix) {
        return INVALID_CONTAINER_NAME;
    }
    let found = report(&p.root, |n| n.starts_with(&prefix), context, callback);
    tracing::info!(prefix, found, "game save containers listed by name");
    S_OK
}

extern "system" fn enumerate_containers(
    _: This,
    provider: *const Provider,
    context: *mut c_void,
    callback: Option<ContainerCallback>,
) -> HRESULT {
    let (Some(p), Some(callback)) = (get(provider), callback) else {
        return E_INVALIDARG;
    };
    let found = report(&p.root, |_| true, context, callback);
    tracing::info!(found, "game save containers listed");
    S_OK
}

/// The container exists once an update to it is submitted.
extern "system" fn create_container(
    _: This,
    provider: *const Provider,
    name: *const c_char,
    out: *mut *mut Container,
) -> HRESULT {
    let (Some(p), Some(name)) = (get(provider), text(name)) else {
        return E_INVALIDARG;
    };
    if !valid_container(&name) {
        tracing::info!(name, "game save container refused: not a valid name");
        return INVALID_CONTAINER_NAME;
    }
    put(out, give(p.container(&name)))
}
extern "system" fn close_container(_: This, container: *mut Container) {
    take(container);
}

extern "system" fn enumerate_blobs(
    _: This,
    container: *const Container,
    prefix: *const c_char,
    context: *mut c_void,
    callback: Option<BlobCallback>,
) -> HRESULT {
    let (Some(c), Some(callback)) = (get(container), callback) else {
        return E_INVALIDARG;
    };
    let prefix = text(prefix).unwrap_or_default();
    let blobs = {
        let _saves = saves_lock();
        let version = current_named(&c.dir, &c.name).ok().flatten();
        version.map(|v| v.sizes()).unwrap_or_default()
    };
    for (name, size) in blobs.into_iter().filter(|(n, _)| n.starts_with(&prefix)) {
        let name = cstring(&name);
        let info = BlobInfo {
            name: name.as_ptr(),
            size: size as u32,
        };
        if !callback(&info, context) {
            break;
        }
    }
    S_OK
}

extern "system" fn read_blobs(
    _: This,
    container: *const Container,
    blob_names: *const *const c_char,
    count: *mut u32,
    size: usize,
    out: *mut u8,
) -> HRESULT {
    let Some(c) = get(container) else {
        return E_INVALIDARG;
    };
    // SAFETY: the title passes its count.
    let wanted = unsafe { count.as_ref() }.copied().unwrap_or(0);
    let blobs = match read(c, names(blob_names, wanted)) {
        Ok(blobs) => blobs,
        Err(hr) => return hr,
    };
    let bytes = layout(&blobs, out as usize);
    if out.is_null() || size < bytes.len() {
        return BUFFER_TOO_SMALL;
    }
    // SAFETY: the buffer holds `size` bytes, checked above.
    unsafe { out.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len()) };
    put(count, blobs.len() as u32)
}

/// Read at once; the result holds the layout with pointers relative to the
/// title's buffer, fixed up when the title takes it.
extern "system" fn read_async(
    _: This,
    container: *const Container,
    blob_names: *const *const c_char,
    count: u32,
    block: *mut AsyncBlock,
) -> HRESULT {
    let result = match get(container) {
        Some(c) => read(c, names(blob_names, count)).map(|blobs| layout(&blobs, 0)),
        None => Err(E_INVALIDARG),
    };
    xasync::finished_with(block, (&raw const READ).cast(), result)
}
extern "system" fn read_result(
    _: This,
    block: *mut AsyncBlock,
    size: usize,
    out: *mut u8,
    count: *mut u32,
) -> HRESULT {
    let mut used = 0;
    let hr = xasync::XAsyncGetResult(block, (&raw const READ).cast(), size, out.cast(), &mut used);
    // The documented error for a small buffer; the result stays for a
    // retry with a larger one.
    if hr == xasync::E_NOT_SUFFICIENT_BUFFER {
        return BUFFER_TOO_SMALL;
    }
    if hr < 0 {
        return hr;
    }
    // The first name follows the blob array, so its offset gives the count.
    let blobs = if used == 0 {
        0
    } else {
        // SAFETY: the result holds at least one blob entry.
        unsafe { out.cast::<usize>().read_unaligned() / BLOB }
    };
    for i in 0..blobs {
        for field in [0, 16] {
            // SAFETY: within the blob array just written.
            unsafe {
                let p = out.add(i * BLOB + field).cast::<usize>();
                p.write_unaligned(p.read_unaligned() + out as usize);
            }
        }
    }
    put(count, blobs as u32)
}

extern "system" fn create_update(
    _: This,
    container: *const Container,
    display: *const c_char,
    out: *mut *mut Update,
) -> HRESULT {
    let Some(c) = get(container) else {
        return E_INVALIDARG;
    };
    put(
        out,
        give(Update {
            name: c.name.clone(),
            dir: c.dir.clone(),
            display: text(display),
            changes: Vec::new(),
        }),
    )
}
extern "system" fn close_update(_: This, update: *mut Update) {
    take(update);
}

extern "system" fn write_blob(
    _: This,
    update: *mut Update,
    name: *const c_char,
    data: *const u8,
    size: usize,
) -> HRESULT {
    // SAFETY: a handle we gave out; the title does not use it concurrently.
    let (Some(update), Some(name)) = (unsafe { update.as_mut() }, text(name)) else {
        return E_INVALIDARG;
    };
    if name.is_empty() || (data.is_null() && size > 0) {
        return E_INVALIDARG;
    }
    if size > MAX_UPDATE {
        return UPDATE_TOO_BIG;
    }
    let data = if size == 0 {
        Vec::new()
    } else {
        // SAFETY: the title passes `size` bytes.
        unsafe { std::slice::from_raw_parts(data, size) }.to_vec()
    };
    update.changes.push((name, Some(data)));
    S_OK
}

extern "system" fn delete_blob(_: This, update: *mut Update, name: *const c_char) -> HRESULT {
    // SAFETY: as in `write_blob`.
    match (unsafe { update.as_mut() }, text(name)) {
        (Some(update), Some(name)) if !name.is_empty() => {
            update.changes.push((name, None));
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

/// `XGameSaveSubmitUpdate`, answered at once.
extern "system" fn submit(_: This, update: *const Update) -> HRESULT {
    get(update).map_or(E_INVALIDARG, Update::submit)
}
extern "system" fn submit_async(_: This, update: *const Update, block: *mut AsyncBlock) -> HRESULT {
    let hr = get(update).map_or(E_INVALIDARG, Update::submit);
    xasync::finished(block, (&raw const SUBMIT).cast(), hr)
}
extern "system" fn submit_result(_: This, block: *mut AsyncBlock) -> HRESULT {
    let none = std::ptr::null_mut();
    xasync::XAsyncGetResult(block, (&raw const SUBMIT).cast(), 0, none, none.cast())
}

static VTABLE: Methods<30> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    initialize as *const (),           // 0x18 (blocking)
    initialize_async as *const (),     // 0x20
    initialize_result as *const (),    // 0x28
    close_provider as *const (),       // 0x30
    quota as *const (),                // 0x38 remaining quota (blocking)
    quota_async as *const (),          // 0x40
    quota_result as *const (),         // 0x48
    delete_now as *const (),           // 0x50 delete (blocking)
    delete_async as *const (),         // 0x58
    delete_result as *const (),        // 0x60
    container_info as *const (),       // 0x68 by name (prefix)
    enumerate_containers as *const (), // 0x70
    missing::<15> as *const (),        // 0x78 by name prefix
    create_container as *const (),     // 0x80
    close_container as *const (),      // 0x88
    missing::<18> as *const (),        // 0x90 all blob info
    enumerate_blobs as *const (),      // 0x98 by name prefix
    read_blobs as *const (),           // 0xa0
    read_async as *const (),           // 0xa8
    read_result as *const (),          // 0xb0
    create_update as *const (),        // 0xb8
    close_update as *const (),         // 0xc0
    write_blob as *const (),           // 0xc8
    delete_blob as *const (),          // 0xd0
    submit as *const (),               // 0xd8 (blocking)
    submit_async as *const (),         // 0xe0
    submit_result as *const (),        // 0xe8
]);

#[cfg(test)]
mod tests {
    use std::ffi::CStr;
    use std::io::BufRead;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::modules::xgameruntime::method;

    fn block() -> AsyncBlock {
        AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }
    }

    static STEPS: AtomicU32 = AtomicU32::new(0);

    /// In a child process of the crash test, stops for good at the step
    /// `VICTORY_SAVE_STOP` names and says so.
    pub(super) fn crash_point() {
        let Ok(stop) = std::env::var("VICTORY_SAVE_STOP") else {
            return;
        };
        if stop.parse() == Ok(STEPS.fetch_add(1, Ordering::SeqCst) + 1) {
            println!("STOPPED");
            let _ = std::io::stdout().flush();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
    }

    /// The per-user quota and the host disk fail with their own errors.
    #[test]
    fn a_full_disk_is_not_the_quota() {
        let mb = 1 << 20;
        assert_eq!(no_room(2 * mb, 0, 0, mb), Some(OUT_OF_LOCAL_STORAGE));
        assert_eq!(
            no_room(2 * mb, 0, QUOTA - mb, 100 * mb),
            Some(QUOTA_EXCEEDED)
        );
        // Replacing a blob of the same size fits a full quota.
        assert_eq!(no_room(mb, mb, QUOTA, 100 * mb), None);
        assert_eq!(no_room(mb, 0, 0, mb), None);
    }

    fn update(root: &Path, display: &str, changes: &[(&str, Option<&str>)]) -> HRESULT {
        Update {
            name: "Slot".into(),
            dir: container_dir(root, "Slot"),
            display: Some(display.into()),
            changes: changes
                .iter()
                .map(|(n, d)| (n.to_string(), d.map(|d| d.as_bytes().to_vec())))
                .collect(),
        }
        .submit()
    }

    /// The container `Slot`: display name and blobs.
    fn state(root: &Path) -> Option<(String, Vec<(String, String)>)> {
        let c = Provider { root: root.into() }.container("Slot");
        let display = current_named(&c.dir, "Slot").unwrap()?.display;
        let blobs = read(&c, None).unwrap();
        let text = |(n, d): (String, Vec<u8>)| (n, String::from_utf8(d).unwrap());
        Some((display, blobs.into_iter().map(text).collect()))
    }

    fn old_state() -> Option<(String, Vec<(String, String)>)> {
        Some((
            "one".into(),
            vec![("a".into(), "old a".into()), ("b".into(), "old b".into())],
        ))
    }

    /// Run by the crash test in a child process; does nothing otherwise.
    #[test]
    fn crash_child() {
        let (Ok(root), Ok(op)) = (
            std::env::var("VICTORY_SAVE_CHILD"),
            std::env::var("VICTORY_SAVE_OP"),
        ) else {
            return;
        };
        let root = PathBuf::from(root);
        let hr = if op == "update" {
            update(
                &root,
                "two",
                &[("a", Some("new a")), ("b", None), ("c", Some("new c"))],
            )
        } else {
            delete(&root, "Slot")
        };
        assert_eq!(hr, S_OK);
        println!("FINISHED");
    }

    /// Kills a process in the middle of an update (and of a deletion) at
    /// every step: what is left is the whole old container or the whole
    /// new one.
    #[test]
    fn a_kill_at_any_step_leaves_old_or_new() {
        let new = Some((
            "two".into(),
            vec![("a".into(), "new a".into()), ("c".into(), "new c".into())],
        ));
        for (op, after) in [("update", new), ("delete", None)] {
            let mut seen = (0, 0);
            for stop in 1.. {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path();
                assert_eq!(
                    update(root, "one", &[("a", Some("old a")), ("b", Some("old b"))]),
                    S_OK
                );
                let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "modules::xgamesave::tests::crash_child",
                        "--nocapture",
                    ])
                    .args(["--test-threads=1"])
                    .env("VICTORY_SAVE_CHILD", root)
                    .env("VICTORY_SAVE_OP", op)
                    .env("VICTORY_SAVE_STOP", stop.to_string())
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                let out = std::io::BufReader::new(child.stdout.take().unwrap());
                let mut finished = false;
                for line in out.lines() {
                    let line = line.unwrap();
                    if line.contains("STOPPED") {
                        break;
                    }
                    finished |= line.contains("FINISHED");
                }
                let _ = child.kill();
                child.wait().unwrap();
                let left = state(root);
                if left == old_state() {
                    seen.0 += 1;
                } else {
                    assert_eq!(left, after, "{op}, killed at step {stop}");
                    seen.1 += 1;
                }
                if finished {
                    break;
                }
            }
            assert!(seen.0 > 0 && seen.1 > 1, "{op}: {seen:?}");
        }
    }

    /// A kept blob that cannot be hard-linked (NTFS allows 1024 links to a
    /// file) is copied into the new version with its exact bytes.
    #[cfg(windows)]
    #[test]
    fn a_kept_blob_past_the_link_limit_is_copied() {
        let dir = tempfile::tempdir().unwrap();
        let links = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            update(root, "one", &[("a", Some("old a")), ("b", Some("old b"))]),
            S_OK
        );
        const NTFS: &str = "the temp folder must be on NTFS, which allows 1024 links to a file";
        let old_b = container_dir(root, "Slot").join("v1").join("1");
        for i in 0..1023 {
            std::fs::hard_link(&old_b, links.path().join(i.to_string())).expect(NTFS);
        }
        let full = std::fs::hard_link(&old_b, links.path().join("full")).expect_err(NTFS);
        assert_eq!(full.kind(), std::io::ErrorKind::TooManyLinks, "{NTFS}");

        assert_eq!(update(root, "two", &[("a", Some("new a"))]), S_OK);

        let new = (
            "two".into(),
            vec![("a".into(), "new a".into()), ("b".into(), "old b".into())],
        );
        assert_eq!(state(root), Some(new));
        assert!(!old_b.exists());
        assert_eq!(std::fs::read(links.path().join("0")).unwrap(), b"old b");
    }

    #[test]
    fn names_are_exact() {
        for name in ["Save_1", "slot/2", "a.b c%", "\u{e9}t\u{e9}"] {
            let file = file_name(name);
            assert!(!file.contains(['.', '/', ' ']), "{file}");
            assert_eq!(decode(&file).as_deref(), Some(name));
        }
        assert_eq!(decode("x.new"), None);
        for good in ["Profile/Slot_1", "a/b/Save-2.sav", "x", &"a".repeat(256)] {
            assert!(valid_container(good), "{good}");
        }
        for bad in [
            "a b",
            "",
            "a/",
            "a.",
            "a..b",
            "a.b/c",
            "a-b/c",
            &"a".repeat(257),
        ] {
            assert!(!valid_container(bad), "{bad}");
        }

        // Containers and blobs differing only in case, reserved device
        // names, characters the host forbids, long names: all distinct.
        let dir = tempfile::tempdir().unwrap();
        let p = Provider {
            root: dir.path().into(),
        };
        let long = "n".repeat(300);
        let blobs = [
            "A",
            "a",
            "CON",
            "nul.txt",
            "x/y:z*?\"<>|",
            long.as_str(),
            "end. ",
        ];
        for container in ["Save", "save", "SAVE/CON.x"] {
            let c = p.container(container);
            let u = Update {
                name: c.name.clone(),
                dir: c.dir.clone(),
                display: None,
                changes: blobs
                    .iter()
                    .map(|b| (b.to_string(), Some(format!("{container}:{b}").into_bytes())))
                    .collect(),
            };
            assert_eq!(u.submit(), S_OK, "{container}");
        }
        let _saves = saves_lock();
        let names: Vec<String> = containers(dir.path()).into_iter().map(|c| c.0).collect();
        drop(_saves);
        assert_eq!(names, ["SAVE/CON.x", "Save", "save"]);
        for container in ["Save", "save", "SAVE/CON.x"] {
            let read = read(&p.container(container), None).unwrap();
            let mut expected: Vec<_> = blobs
                .iter()
                .map(|b| (b.to_string(), format!("{container}:{b}").into_bytes()))
                .collect();
            expected.sort();
            assert_eq!(read, expected, "{container}");
        }
    }

    #[test]
    fn the_old_layout_moves_over() {
        let saves = tempfile::tempdir().unwrap();
        let root = saves
            .path()
            .join(format!("{:016X}", xuser::XUID))
            .join("scid");
        let old = root.join("Player");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("Save%20Game%20Data"), b"data").unwrap();
        std::fs::write(old.join("Save%20Game%20Header"), b"header").unwrap();
        std::fs::write(old.join("Save%20Game%20Data.new"), b"half").unwrap();
        std::fs::write(old.join(".display"), b"Chapter 1").unwrap();
        let p = Provider::open(saves.path(), Some("scid")).unwrap();
        assert!(!old.exists());
        let c = p.container("Player");
        let blobs = read(&c, None).unwrap();
        assert_eq!(
            blobs,
            [
                ("Save Game Data".into(), b"data".to_vec()),
                ("Save Game Header".into(), b"header".to_vec())
            ]
        );
        let v = current_named(&c.dir, "Player").unwrap().unwrap();
        assert_eq!(
            (v.dir.file_name(), v.display.as_str()),
            (Some("v1".as_ref()), "Chapter 1")
        );
    }

    /// An empty configuration id names none. As a folder name it would be
    /// the folder every configuration lives in, whose folders then look
    /// like containers of the old layout and are converted away.
    #[test]
    fn an_empty_configuration_id_is_the_default_one() {
        let saves = tempfile::tempdir().unwrap();
        let scid = Provider::open(saves.path(), Some("scid")).unwrap();
        assert_eq!(update(&scid.root, "one", &[("a", Some("kept"))]), S_OK);
        let kept = Some(("one".into(), vec![("a".into(), "kept".into())]));
        assert_eq!(state(&scid.root), kept);

        let empty = Provider::open(saves.path(), Some("")).unwrap();
        assert!(scid.root.is_dir(), "another configuration's saves are gone");
        assert_eq!(state(&scid.root), kept);
        let default = Provider::open(saves.path(), None).unwrap();
        assert_eq!(empty.root, default.root);

        // Named configurations stay apart, from it and from each other.
        assert_ne!(empty.root, scid.root);
        assert_eq!(state(&empty.root), None);
        let other = Provider::open(saves.path(), Some("other")).unwrap();
        assert_eq!(state(&other.root), None);
        assert_eq!(state(&scid.root), kept);
    }

    #[test]
    fn saves_persist_across_providers() {
        let this: This = &OBJECT;
        let saves = tempfile::tempdir().unwrap();
        let provider = give(Provider::open(saves.path(), Some("scid")).unwrap());

        // Write two blobs through an update.
        let mut container = std::ptr::null_mut();
        type Create = extern "system" fn(
            This,
            *const Provider,
            *const c_char,
            *mut *mut Container,
        ) -> HRESULT;
        let create = method::<Create>(&OBJECT, 0x80);
        assert_eq!(
            create(this, provider, c"bad name".as_ptr(), &mut container),
            INVALID_CONTAINER_NAME
        );
        assert_eq!(
            create(this, provider, c"Slot_1".as_ptr(), &mut container),
            S_OK
        );
        let mut update = std::ptr::null_mut();
        type Update_ =
            extern "system" fn(This, *const Container, *const c_char, *mut *mut Update) -> HRESULT;
        assert_eq!(
            method::<Update_>(&OBJECT, 0xb8)(this, container, c"Chapter 1".as_ptr(), &mut update),
            S_OK
        );
        type Write =
            extern "system" fn(This, *mut Update, *const c_char, *const u8, usize) -> HRESULT;
        let write = method::<Write>(&OBJECT, 0xc8);
        assert_eq!(
            write(this, update, c"world".as_ptr(), b"hello".as_ptr(), 5),
            S_OK
        );
        assert_eq!(
            write(this, update, c"player.dat".as_ptr(), b"xy".as_ptr(), 2),
            S_OK
        );
        assert_eq!(
            write(this, update, c"gone".as_ptr(), b"z".as_ptr(), 1),
            S_OK
        );
        type Delete = extern "system" fn(This, *mut Update, *const c_char) -> HRESULT;
        assert_eq!(
            method::<Delete>(&OBJECT, 0xd0)(this, update, c"gone".as_ptr()),
            S_OK
        );
        type Submit = extern "system" fn(This, *const Update, *mut AsyncBlock) -> HRESULT;
        let mut b = block();
        assert_eq!(method::<Submit>(&OBJECT, 0xe0)(this, update, &mut b), S_OK);
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        close_update(this, update);
        close_container(this, container);
        close_provider(this, provider);

        // A new provider sees the container and reads both blobs back.
        let provider = give(Provider::open(saves.path(), Some("scid")).unwrap());
        extern "system" fn seen(info: *const ContainerInfo, context: *mut c_void) -> bool {
            // SAFETY: valid during the callback; the context is the test's vector.
            let (info, out) = unsafe {
                (
                    &*info,
                    &mut *context.cast::<Vec<(String, String, u32, u64)>>(),
                )
            };
            out.push((
                text(info.name).unwrap(),
                text(info.display_name).unwrap(),
                info.blob_count,
                info.total_size,
            ));
            true
        }
        let mut found: Vec<(String, String, u32, u64)> = Vec::new();
        let context = (&raw mut found).cast();
        assert_eq!(
            enumerate_containers(this, provider, context, Some(seen)),
            S_OK
        );
        assert_eq!(found, [("Slot_1".into(), "Chapter 1".into(), 2, 7)]);

        let mut container = std::ptr::null_mut();
        assert_eq!(
            create_container(this, provider, c"Slot_1".as_ptr(), &mut container),
            S_OK
        );
        let mut b = block();
        let wanted = [c"world".as_ptr(), c"player.dat".as_ptr()];
        assert_eq!(
            read_async(this, container, wanted.as_ptr(), 2, &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        let mut size = 0;
        assert_eq!(xasync::XAsyncGetResultSize(&mut b, &mut size), S_OK);
        assert_eq!(
            size,
            2 * BLOB + "world\0hello".len() + "player.dat\0xy".len()
        );
        let mut buffer = vec![0u64; size.div_ceil(8)];
        let mut count = 0;
        assert_eq!(
            read_result(this, &mut b, 8, buffer.as_mut_ptr().cast(), &mut count),
            BUFFER_TOO_SMALL
        );
        assert_eq!(
            read_result(this, &mut b, size, buffer.as_mut_ptr().cast(), &mut count),
            S_OK
        );
        assert_eq!(count, 2);
        let blob = |i: usize| {
            // SAFETY: the layout the call wrote.
            unsafe {
                let entry = buffer.as_ptr().cast::<u8>().add(i * BLOB);
                let name = text(entry.cast::<*const c_char>().read_unaligned()).unwrap();
                let len = entry.add(8).cast::<u32>().read_unaligned() as usize;
                let data = std::slice::from_raw_parts(
                    entry.add(16).cast::<*const u8>().read_unaligned(),
                    len,
                );
                (name, data.to_vec())
            }
        };
        assert_eq!(blob(0), ("world".into(), b"hello".to_vec()));
        assert_eq!(blob(1), ("player.dat".into(), b"xy".to_vec()));

        let missing = [c"nothing".as_ptr()];
        let mut one = 1;
        let mut small = [0u8; 8];
        let read_now = method::<
            extern "system" fn(
                This,
                *const Container,
                *const *const c_char,
                *mut u32,
                usize,
                *mut u8,
            ) -> HRESULT,
        >(&OBJECT, 0xa0);
        assert_eq!(
            read_now(
                this,
                container,
                missing.as_ptr(),
                &mut one,
                8,
                small.as_mut_ptr()
            ),
            BLOB_NOT_FOUND
        );
        assert_eq!(
            read_now(
                this,
                container,
                wanted.as_ptr(),
                &mut 2,
                8,
                small.as_mut_ptr()
            ),
            BUFFER_TOO_SMALL
        );

        // Quota counts the blobs; deleting the container frees it.
        let mut b = block();
        assert_eq!(quota_async(this, provider, &mut b), S_OK);
        let mut left = 0i64;
        assert_eq!(quota_result(this, &mut b, &mut left), S_OK);
        assert!(left as u64 <= QUOTA - 7);
        let mut b = block();
        assert_eq!(
            delete_async(this, provider, c"Slot_1".as_ptr(), &mut b),
            S_OK
        );
        assert_eq!(delete_result(this, &mut b), S_OK);
        found.clear();
        let context = (&raw mut found).cast();
        assert_eq!(
            enumerate_containers(this, provider, context, Some(seen)),
            S_OK
        );
        assert!(found.is_empty());
        close_container(this, container);
        close_provider(this, provider);
    }

    /// Only `v<N>` as `commit` writes it is a version: `v01` and the like
    /// are neither read, counted nor removed (#870).
    #[test]
    fn a_noncanonical_folder_does_not_remove_a_committed_save() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            update(root, "one", &[("a", Some("old a")), ("b", Some("old b"))]),
            S_OK
        );
        let slot = container_dir(root, "Slot");
        for stray in ["v01", "v001", "v+1", "v0", "v02"] {
            std::fs::create_dir(slot.join(stray)).unwrap();
        }
        assert_eq!(state(root), old_state());
        assert!(slot.join("v1").is_dir());
        for stray in ["v01", "v001", "v+1", "v0", "v02"] {
            assert!(slot.join(stray).exists(), "{stray} was removed");
        }
    }

    /// A newer version folder that does not read (an empty folder, a file,
    /// a broken manifest) neither hides the committed save, nor removes it,
    /// nor blocks the next update, which goes above it.
    #[test]
    fn an_unreadable_newer_version_does_not_cost_the_save() {
        type Make = fn(&Path);
        let strays: [(&str, Make); 3] = [
            ("empty folder", |p| std::fs::create_dir(p).unwrap()),
            ("file", |p| std::fs::write(p, b"not a version").unwrap()),
            ("broken manifest", |p| {
                std::fs::create_dir(p).unwrap();
                std::fs::write(p.join(MANIFEST), [9, 0, 0, 0, b'x']).unwrap();
            }),
        ];
        for (what, make) in strays {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            assert_eq!(
                update(root, "one", &[("a", Some("old a")), ("b", Some("old b"))]),
                S_OK
            );
            let slot = container_dir(root, "Slot");
            make(&slot.join("v2"));

            assert_eq!(state(root), old_state(), "{what}: read");
            assert!(slot.join("v1").is_dir(), "{what}: v1 was removed");

            assert_eq!(
                update(root, "two", &[("a", Some("new a"))]),
                S_OK,
                "{what}: update"
            );
            let blobs = vec![("a".into(), "new a".into()), ("b".into(), "old b".into())];
            let after = Some(("two".into(), blobs));
            assert_eq!(state(root), after, "{what}: after the update");
            assert!(
                slot.join("v3").is_dir(),
                "{what}: the update did not go above v2"
            );
            // Read again: the new version stays the container.
            assert_eq!(state(root), after, "{what}: second read");
        }
    }

    /// With no readable version at all, reads and updates fail and nothing
    /// is removed.
    #[test]
    fn no_readable_version_is_refused_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(update(root, "one", &[("a", Some("old a"))]), S_OK);
        let slot = container_dir(root, "Slot");
        std::fs::write(slot.join("v1").join(MANIFEST), [1, 2, 3]).unwrap();
        let c = Provider { root: root.into() }.container("Slot");
        assert_eq!(read(&c, None).err(), Some(NO_ACCESS));
        assert_eq!(update(root, "two", &[("a", Some("new a"))]), NO_ACCESS);
        assert!(slot.join("v1").join("0").is_file());
        assert!(!slot.join("v2").exists());
    }

    type Names = Vec<(String, u32, u64)>;

    extern "system" fn collect_containers(
        info: *const ContainerInfo,
        context: *mut c_void,
    ) -> bool {
        // SAFETY: valid during the callback; the context is the test's vector.
        let (info, out) = unsafe { (&*info, &mut *context.cast::<Names>()) };
        out.push((text(info.name).unwrap(), info.blob_count, info.total_size));
        true
    }

    extern "system" fn collect_blobs(info: *const BlobInfo, context: *mut c_void) -> bool {
        // SAFETY: as above.
        let (info, out) = unsafe { (&*info, &mut *context.cast::<Vec<(String, u32)>>()) };
        out.push((text(info.name).unwrap(), info.size));
        true
    }

    /// What a title does with its saves, step by step over the vtable.
    struct Title {
        provider: *mut Provider,
    }

    impl Title {
        fn start(saves: &Path) -> Title {
            let provider = Provider::open(saves, Some("scid")).unwrap();
            Title {
                provider: give(provider),
            }
        }

        fn list(&self, prefix: Option<&CStr>) -> Result<Names, HRESULT> {
            let mut found = Names::new();
            let context = (&raw mut found).cast();
            let collect = Some(collect_containers as ContainerCallback);
            let hr = match prefix {
                None => enumerate_containers(&OBJECT, self.provider, context, collect),
                Some(p) => container_info(&OBJECT, self.provider, p.as_ptr(), context, collect),
            };
            if hr == S_OK { Ok(found) } else { Err(hr) }
        }

        fn container(&self, name: &CStr) -> *mut Container {
            let mut container = std::ptr::null_mut();
            let hr = create_container(&OBJECT, self.provider, name.as_ptr(), &mut container);
            assert_eq!(hr, S_OK);
            container
        }

        /// One update: blobs written (`Some`) or deleted (`None`), then
        /// submitted the blocking way.
        fn save(&self, name: &CStr, display: &CStr, changes: &[(&CStr, Option<&[u8]>)]) -> HRESULT {
            let container = self.container(name);
            let mut update = std::ptr::null_mut();
            assert_eq!(
                create_update(&OBJECT, container, display.as_ptr(), &mut update),
                S_OK
            );
            for (blob, data) in changes {
                let hr = match data {
                    Some(data) => {
                        write_blob(&OBJECT, update, blob.as_ptr(), data.as_ptr(), data.len())
                    }
                    None => delete_blob(&OBJECT, update, blob.as_ptr()),
                };
                assert_eq!(hr, S_OK);
            }
            let hr = submit(&OBJECT, update);
            close_update(&OBJECT, update);
            close_container(&OBJECT, container);
            hr
        }

        fn blobs(&self, name: &CStr, prefix: &CStr) -> Vec<(String, u32)> {
            let container = self.container(name);
            let mut blobs = Vec::new();
            let context = (&raw mut blobs).cast();
            let collect = Some(collect_blobs as BlobCallback);
            let hr = enumerate_blobs(&OBJECT, container, prefix.as_ptr(), context, collect);
            assert_eq!(hr, S_OK);
            close_container(&OBJECT, container);
            blobs
        }

        /// Reads blobs into a buffer sized from the enumerated sizes, as a
        /// title must, and parses what the read laid out.
        fn load(&self, name: &CStr, wanted: &[&CStr]) -> Result<Vec<(String, Vec<u8>)>, HRESULT> {
            let sizes = self.blobs(name, c"");
            let want = |n: &str| wanted.iter().any(|w| w.to_str() == Ok(n));
            let need: usize = sizes
                .iter()
                .filter(|(n, _)| want(n))
                .map(|(n, size)| BLOB + n.len() + 1 + *size as usize)
                .sum();
            let mut buffer = vec![0u64; need.div_ceil(8) + 1];
            let container = self.container(name);
            let names: Vec<*const c_char> = wanted.iter().map(|w| w.as_ptr()).collect();
            let mut count = names.len() as u32;
            let out = buffer.as_mut_ptr().cast::<u8>();
            let room = buffer.len() * 8;
            let hr = read_blobs(&OBJECT, container, names.as_ptr(), &mut count, room, out);
            close_container(&OBJECT, container);
            if hr != S_OK {
                return Err(hr);
            }
            let mut blobs = Vec::new();
            for i in 0..count as usize {
                // SAFETY: the layout the call wrote into `buffer`.
                unsafe {
                    let entry = out.add(i * BLOB);
                    let name = text(entry.cast::<*const c_char>().read_unaligned()).unwrap();
                    let len = entry.add(8).cast::<u32>().read_unaligned() as usize;
                    let data = entry.add(16).cast::<*const u8>().read_unaligned();
                    blobs.push((name, std::slice::from_raw_parts(data, len).to_vec()));
                }
            }
            Ok(blobs)
        }

        fn quota(&self) -> i64 {
            let mut left = 0;
            assert_eq!(quota(&OBJECT, self.provider, &mut left), S_OK);
            left
        }

        /// The title quits: its handles close.
        fn quit(self) {
            close_provider(&OBJECT, self.provider);
        }
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    /// The flow of a title that keeps a profile and numbered autosave slots:
    /// create a container, write blobs, commit, list, read, quit, start
    /// again and find it all, update part of a slot, delete one.
    #[test]
    fn a_title_flow_writes_commits_lists_and_reads_back_across_restarts() {
        let saves = tempfile::tempdir().unwrap();
        let (data, header) = (pattern(244_128, 7), pattern(296, 9));
        let profile = (pattern(2012, 1), pattern(296, 2));

        // First run: nothing is there; the profile and the slot are saved.
        let title = Title::start(saves.path());
        assert!(title.list(None).unwrap().is_empty());
        assert!(title.list(Some(c"Player")).unwrap().is_empty());
        assert_eq!(title.load(c"Slot0015", &[c"Data"]), Err(BLOB_NOT_FOUND));
        let full = title.quota();
        let changes: [(&CStr, Option<&[u8]>); 2] =
            [(c"Data", Some(&profile.0)), (c"Header", Some(&profile.1))];
        assert_eq!(title.save(c"Player", c"Player", &changes), S_OK);
        let changes: [(&CStr, Option<&[u8]>); 2] =
            [(c"Data", Some(&data)), (c"Header", Some(&header))];
        assert_eq!(title.save(c"Slot0015", c"(Autosave) Start", &changes), S_OK);
        assert_eq!(title.quota(), full - 244_128 - 296 - 2012 - 296);
        title.quit();

        // Second run: both are listed, with their sizes, and read back.
        let title = Title::start(saves.path());
        let all = title.list(None).unwrap();
        assert_eq!(
            all,
            [
                ("Player".to_string(), 2, 2012 + 296),
                ("Slot0015".to_string(), 2, 244_128 + 296)
            ]
        );
        // By name: exactly, as a prefix (every slot), and none.
        assert_eq!(title.list(Some(c"Player")).unwrap(), all[..1]);
        assert_eq!(title.list(Some(c"Slot")).unwrap(), all[1..]);
        assert_eq!(title.list(Some(c"")).unwrap(), all);
        assert!(title.list(Some(c"Slot9")).unwrap().is_empty());
        assert_eq!(title.list(Some(c"no good")), Err(INVALID_CONTAINER_NAME));
        assert_eq!(
            title.blobs(c"Slot0015", c""),
            [("Data".to_string(), 244_128), ("Header".to_string(), 296)]
        );
        assert_eq!(
            title.blobs(c"Slot0015", c"He"),
            [("Header".to_string(), 296)]
        );
        assert_eq!(
            title.load(c"Slot0015", &[c"Header", c"Data"]).unwrap(),
            [
                ("Header".to_string(), header.clone()),
                ("Data".to_string(), data)
            ]
        );
        assert_eq!(title.load(c"Slot0015", &[c"Nothing"]), Err(BLOB_NOT_FOUND));

        // The next autosave replaces the data and keeps the header; the
        // profile gains a blob and loses one.
        let newer = pattern(300_000, 3);
        let changes: [(&CStr, Option<&[u8]>); 1] = [(c"Data", Some(&newer))];
        assert_eq!(title.save(c"Slot0015", c"(Autosave) Later", &changes), S_OK);
        let changes: [(&CStr, Option<&[u8]>); 2] = [(c"Header", None), (c"Extra", Some(b"x"))];
        assert_eq!(title.save(c"Player", c"Player", &changes), S_OK);
        // A name the console refuses never reaches the disk.
        let mut container = std::ptr::null_mut();
        let name = c"bad name".as_ptr();
        let hr = create_container(&OBJECT, title.provider, name, &mut container);
        assert_eq!(hr, INVALID_CONTAINER_NAME);
        title.quit();

        // Third run: the update is what the title finds.
        let title = Title::start(saves.path());
        assert_eq!(
            title.load(c"Slot0015", &[c"Data", c"Header"]).unwrap(),
            [("Data".to_string(), newer), ("Header".to_string(), header)]
        );
        assert_eq!(
            title.list(Some(c"Player")).unwrap(),
            [("Player".to_string(), 2, 2012 + 1)]
        );
        assert_eq!(
            title.blobs(c"Player", c""),
            [("Data".to_string(), 2012), ("Extra".to_string(), 1)]
        );
        let slot = c"Slot0015".as_ptr();
        assert_eq!(delete_now(&OBJECT, title.provider, slot), S_OK);
        assert_eq!(delete_now(&OBJECT, title.provider, slot), S_OK);
        let bad = c"bad name".as_ptr();
        assert_eq!(
            delete_now(&OBJECT, title.provider, bad),
            INVALID_CONTAINER_NAME
        );
        assert_eq!(title.list(None).unwrap().len(), 1);
        title.quit();
    }

    /// The blocking calls are in the table where the public API has them.
    #[test]
    fn the_blocking_calls_are_in_the_table() {
        let at = |offset| method::<usize>(&OBJECT, offset);
        assert_eq!(at(0x18), initialize as usize);
        assert_eq!(at(0x38), quota as usize);
        assert_eq!(at(0x50), delete_now as usize);
        assert_eq!(at(0xd8), submit as usize);
        assert_eq!(at(0xe0), submit_async as usize);
    }

    /// With no storage folder there is no provider (and nothing is written
    /// to the working directory); with one, its saves folder is made.
    #[test]
    fn a_provider_needs_the_titles_storage_folder() {
        assert_eq!(
            open_at(Path::new("saves"), Some("scid")).err(),
            Some(NO_ACCESS)
        );
        assert_eq!(open_at(Path::new(""), None).err(), Some(NO_ACCESS));
        let dir = tempfile::tempdir().unwrap();
        let provider = open_at(&dir.path().join("saves"), Some("scid")).unwrap();
        assert!(provider.root.is_dir());
        assert!(provider.root.starts_with(dir.path()));
    }
}
