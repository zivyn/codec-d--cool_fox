//! Presented frames written to PNG files, to see what reaches the screen:
//! with `VICTORY_CAPTURE=<folder>`, every frame whose number is a power of
//! two (the frames the log reports) is saved as `frame-<n>.png`, at half
//! size. Only in builds with diagnostics.

use std::path::PathBuf;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// The folder frames go to, if capturing.
pub(crate) fn folder() -> Option<&'static PathBuf> {
    static FOLDER: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    FOLDER
        .get_or_init(|| std::env::var_os("VICTORY_CAPTURE").map(PathBuf::from))
        .as_ref()
}

/// Frames listed in `VICTORY_CAPTURE_FRAMES` (comma-separated): saved
/// too, and their draws traced at debug level.
pub(crate) fn listed(n: u64) -> bool {
    static LIST: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    let named = LIST
        .get_or_init(|| {
            let list = std::env::var("VICTORY_CAPTURE_FRAMES").unwrap_or_default();
            list.split(',')
                .filter_map(|f| f.trim().parse().ok())
                .collect()
        })
        .contains(&n);
    // `VICTORY_CAPTURE_WORLD`: also the 2nd and the 6th frame after the first
    // one that ran 500 draws or more (a title's first 3D frames: their
    // numbers depend on how long the menus were shown), and the 30th and
    // the 80th (the title opens with a cutscene made in the
    // engine: later shots of it, in case the first is dark by design).
    //
    // `VICTORY_CAPTURE_WORLD_ALSO` (comma-separated): more frames counted
    // from that first one (the title's blue patches show from
    // about the 118th frame on, when the window's model has arrived and
    // the reflections hold values: none of the four above has them).
    static ALSO: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    let also = ALSO.get_or_init(|| {
        let list = std::env::var("VICTORY_CAPTURE_WORLD_ALSO").unwrap_or_default();
        list.split(',')
            .filter_map(|f| f.trim().parse().ok())
            .collect()
    });
    let first = WORLD.load(std::sync::atomic::Ordering::Relaxed);
    let after = n.wrapping_sub(first);
    named
        || in_window(n)
        || n == TRIGGERED.load(std::sync::atomic::Ordering::Relaxed)
        || first != u64::MAX && ([2, 6, 30, 80].contains(&after) || also.contains(&after))
}

/// The frame a trigger file asked for, or `u64::MAX` (no longer set: a
/// trigger opens a window, [`triggered`]; only the frames of
/// `VICTORY_CAPTURE_FRAMES` and the world's are single frames).
static TRIGGERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// The first and last present of the window a trigger opened ([`triggered`]);
/// 1 and 0 when none is open.
static WINDOW_AT: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(1), std::sync::atomic::AtomicU64::new(0)];

/// The number its files are named by: the window's last present when it
/// opened (the window may be extended; the name stays).
static WINDOW_NAME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What the open window counted.
static WINDOW: std::sync::Mutex<Option<crate::capture_window::Window>> = std::sync::Mutex::new(None);

fn in_window(n: u64) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    n >= WINDOW_AT[0].load(Relaxed) && n <= WINDOW_AT[1].load(Relaxed)
}

/// The number in an environment variable, or `default`.
fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// `VICTORY_CAPTURE_TRIGGER=<file>` (with `VICTORY_CAPTURE`): when the file
/// appears it is deleted and a window opens: from the next present, for
/// `VICTORY_CAPTURE_WINDOW` presents (4), every operation of every list of
/// every queue goes to `frame-<last present>-ops.txt` (with a `PRESENT n` line
/// at each present) and the targets and values are dumped at the last present
/// ([`dump_due`]). The window must hold whole frames (`VICTORY_CAPTURE_MIN_OPS`
/// operations, 5000, and each pass of `VICTORY_CAPTURE_REQUIRE`, the grade
/// pass `6318f669`): if not it is extended up to three times, and the dump
/// says it is partial if it still is. The file can be made again for another
/// window. Called at every present.
fn triggered(frame: u64) {
    static FILE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    let Some(file) = FILE.get_or_init(|| std::env::var_os("VICTORY_CAPTURE_TRIGGER").map(PathBuf::from)) else {
        return;
    };
    if in_window(frame + 1) || WINDOW.lock().unwrap_or_else(|e| e.into_inner()).is_some() || !file.exists() {
        return;
    }
    if std::fs::remove_file(file).is_ok() {
        use std::sync::atomic::Ordering::Relaxed;
        let required: Vec<String> = std::env::var("VICTORY_CAPTURE_REQUIRE")
            .unwrap_or_else(|_| "6318f669".to_owned())
            .split(',')
            .map(|h| h.trim().to_owned())
            .filter(|h| !h.is_empty())
            .collect();
        let window = crate::capture_window::Window::armed(frame, env_number("VICTORY_CAPTURE_WINDOW", 4), &required);
        WINDOW_AT[0].store(window.start, Relaxed);
        WINDOW_AT[1].store(window.end, Relaxed);
        WINDOW_NAME.store(window.end, Relaxed);
        tracing::info!(start = window.start, end = window.end, "capture trigger found: the window is open");
        *WINDOW.lock().unwrap_or_else(|e| e.into_inner()) = Some(window);
    }
}

/// At the present of `frame`: whether the frame's targets are dumped now: a
/// frame named by `VICTORY_CAPTURE_FRAMES` or the world's, or the last
/// present of a trigger's window if the window holds whole frames (else it
/// is extended, up to three times, and dumped as partial).
pub(crate) fn dump_due(frame: u64) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    if !in_window(frame) {
        return listed(frame);
    }
    if frame < WINDOW_AT[1].load(Relaxed) {
        return false;
    }
    let mut guard = WINDOW.lock().unwrap_or_else(|e| e.into_inner());
    let Some(window) = guard.as_mut() else {
        return false;
    };
    let min_ops = env_number("VICTORY_CAPTURE_MIN_OPS", 5000);
    match window.verdict(min_ops) {
        crate::capture_window::Verdict::Partial(why) if window.retries < 3 => {
            window.extend();
            WINDOW_AT[1].store(window.end, Relaxed);
            tracing::warn!(why, now = %window.summary(), "frame looks partial: the window is extended");
            false
        }
        verdict => {
            match verdict {
                crate::capture_window::Verdict::Complete => {
                    tracing::info!(frame, "frame complete: {}", window.summary());
                }
                crate::capture_window::Verdict::Partial(why) => {
                    tracing::warn!(frame, why, "frame PARTIAL after the extensions: {}", window.summary());
                }
            }
            true
        }
    }
}

/// The number the files of the dump at the present of `frame` are named by.
pub(crate) fn dump_name(frame: u64) -> u64 {
    if in_window(frame) { WINDOW_NAME.load(std::sync::atomic::Ordering::Relaxed) } else { frame }
}

/// After the dump at the present of `frame`: closes the window if that was its last.
pub(crate) fn window_done(frame: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if frame >= WINDOW_AT[1].load(Relaxed)
        && in_window(frame)
        && WINDOW.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    {
        WINDOW_AT[0].store(1, Relaxed);
        WINDOW_AT[1].store(0, Relaxed);
        *WINDOW.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// With a capture folder, for the frames [`listed`]: `what` is added to
/// the frame's `frame-<n>-ops.txt`, one line per draw, dispatch, copy or
/// drop in the order recorded (the title records on several threads, so
/// lines of different lists interleave; each names its list). The file
/// says which pass writes which target, to find the pass whose output
/// stays empty.
#[cfg(feature = "diagnostics")]
pub(crate) fn op(list: usize, what: impl FnOnce() -> String) {
    use std::io::Write;
    let frame = crate::queue::presented() + 1;
    if !listed(frame) {
        // Before the world: the last lines are kept, and written out when
        // it starts (`before-world-ops.txt`): what the title drew just
        // before (the title shows a title card there, whose text
        // did not show).
        if world_on() && WORLD.load(std::sync::atomic::Ordering::Relaxed) == u64::MAX {
            // At most 300 lines of a frame: the world's first frame alone
            // (thousands of operations, recorded before it is known to be
            // the world's) pushed everything before it out.
            static IN_FRAME: std::sync::Mutex<(u64, u32)> = std::sync::Mutex::new((0, 0));
            {
                let mut in_frame = IN_FRAME.lock().unwrap_or_else(|e| e.into_inner());
                if in_frame.0 != frame {
                    *in_frame = (frame, 0);
                }
                in_frame.1 += 1;
                if in_frame.1 > 300 {
                    return;
                }
            }
            let what = what();
            let mut ring = RING.lock().unwrap_or_else(|e| e.into_inner());
            if ring.len() >= RING_LINES {
                ring.pop_front();
            }
            ring.push_back(format!("frame {frame} list {list:#x} {what}"));
        }
        return;
    }
    let Some(folder) = folder() else {
        return;
    };
    let what = what();
    // In a trigger's window every operation goes to the one file of its last present.
    let name = if in_window(frame) {
        if let Some(window) = WINDOW.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            window.note(list, &what);
        }
        WINDOW_NAME.load(std::sync::atomic::Ordering::Relaxed)
    } else {
        frame
    };
    write_ops_line(folder, name, &format!("list {list:#x} {what}"));
}

/// One numbered line of `frame-<name>-ops.txt`.
#[cfg(feature = "diagnostics")]
fn write_ops_line(folder: &std::path::Path, name: u64, line: &str) {
    use std::io::Write;
    static FILE: std::sync::Mutex<Option<(u64, u64, std::fs::File)>> = std::sync::Mutex::new(None);
    let mut file = FILE.lock().unwrap_or_else(|e| e.into_inner());
    if file.as_ref().is_none_or(|f| f.0 != name) {
        let Ok(made) = std::fs::File::create(folder.join(format!("frame-{name}-ops.txt"))) else {
            return;
        };
        *file = Some((name, 0, made));
    }
    if let Some((_, n, f)) = file.as_mut() {
        *n += 1;
        let _ = writeln!(f, "{n} {line}");
    }
}

/// The last lines of [`op`] before the world.
#[cfg(feature = "diagnostics")]
static RING: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
#[cfg(feature = "diagnostics")]
const RING_LINES: usize = 6000;

/// Whether `VICTORY_CAPTURE_WORLD` is set.
fn world_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CAPTURE_WORLD").is_some())
}

/// The first frame that ran 500 draws or more (`VICTORY_CAPTURE_WORLD`).
static WORLD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// At the present of `frame`: the draws run on the host so far.
pub(crate) fn frame_drew(frame: u64, drawn: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let before = LAST.swap(drawn, Relaxed);
    triggered(frame);
    #[cfg(feature = "diagnostics")]
    if in_window(frame)
        && let Some(folder) = folder()
    {
        write_ops_line(folder, WINDOW_NAME.load(Relaxed), &format!("PRESENT {frame} draws {drawn}"));
    }
    if world_on()
        && drawn.saturating_sub(before) >= 500
        && WORLD.compare_exchange(u64::MAX, frame, Relaxed, Relaxed).is_ok()
    {
        tracing::info!(frame, "the first frame of 500 draws or more: frames {} and {} are captured", frame + 2, frame + 6);
        #[cfg(feature = "diagnostics")]
        if let Some(folder) = folder() {
            let lines: Vec<String> = std::mem::take(&mut *RING.lock().unwrap_or_else(|e| e.into_inner())).into();
            let _ = std::fs::write(folder.join("before-world-ops.txt"), lines.join("
"));
        }
    }
}

/// Whether frame `n` is saved.
pub(crate) fn wanted(n: u64) -> bool {
    // Before the world, with `VICTORY_CAPTURE_WORLD`: every 16th frame too
    // (small pictures of menus, loading screens and title cards).
    let before_world = world_on() && n.is_multiple_of(16) && WORLD.load(std::sync::atomic::Ordering::Relaxed) == u64::MAX;
    // In the world: every 8th frame's picture, a timeline of the opening
    // cutscene (its shots do not fall on the same frame numbers from run to
    // run, so single frames cannot be compared between runs).
    let first = WORLD.load(std::sync::atomic::Ordering::Relaxed);
    let in_world = world_on() && first != u64::MAX && n > first && (n - first).is_multiple_of(8);
    folder().is_some() && (n.is_power_of_two() || listed(n) || before_world || in_world)
}

/// Saves 8-bit RGBA or BGRA rows (`pitch` bytes apart) as frame `n`.
pub(crate) fn save(n: u64, width: u32, height: u32, pitch: usize, bgra: bool, rows: &[u8]) {
    save_as(&format!("frame-{n}"), width, height, pitch, bgra, rows);
}

/// Saves 8-bit RGBA or BGRA rows as `<name>.png`, at half size.
fn save_as(name: &str, width: u32, height: u32, pitch: usize, bgra: bool, rows: &[u8]) {
    let Some(folder) = folder() else { return };
    let (w, h) = ((width / 2).max(1), (height / 2).max(1));
    // Filter byte 0 per row, then RGB of every other pixel of every other row.
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        let row = &rows[(2 * y).min(height as usize - 1) * pitch..];
        for x in 0..w as usize {
            let p = &row[(2 * x).min(width as usize - 1) * 4..][..3];
            if bgra {
                raw.extend([p[2], p[1], p[0]]);
            } else {
                raw.extend_from_slice(p);
            }
        }
    }
    let png = encode(w, h, &raw);
    let path = folder.join(format!("{name}.png"));
    match std::fs::write(&path, png) {
        Ok(()) => tracing::info!(path = %path.display(), "frame saved"),
        Err(e) => tracing::warn!(error = %e, path = %path.display(), "frame not saved"),
    }
}

/// A PNG of `width * height` pixels of 8-bit RGB.
pub(crate) fn png(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let row = width as usize * 3;
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    for line in rgb.chunks_exact(row.max(1)) {
        raw.push(0);
        raw.extend_from_slice(line);
    }
    encode(width, height, &raw)
}

/// A PNG of rows of RGB, each behind its filter byte.
fn encode(w: u32, h: u32, raw: &[u8]) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend(w.to_be_bytes());
    header.extend(h.to_be_bytes());
    header.extend([8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(&mut png, b"IHDR", &header);
    let mut compressed = vec![0u8; zlib_rs::compress_bound(raw.len())];
    let (idat, rc) = zlib_rs::compress_slice(&mut compressed, raw, zlib_rs::DeflateConfig::new(3));
    debug_assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    chunk(&mut png, b"IDAT", idat);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend((data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend(kind);
    png.extend(data);
    let crc = crc32(&png[start..]);
    png.extend(crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// Records a copy of `texture` (in the copy-source state) into a
/// readback buffer.
pub(crate) fn read_back(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    read_back_of(device, list, texture, 0)
}

/// As [`read_back`], of subresource `sub` (the stencil plane of a
/// one-level, one-slice depth-stencil texture is subresource 1).
fn read_back_of(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    sub: u32,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut total = 0;
    // SAFETY: a live texture; outputs for one subresource.
    unsafe {
        let desc = texture.GetDesc();
        device.GetCopyableFootprints(
            &desc,
            sub,
            1,
            0,
            Some(&mut footprint),
            None,
            None,
            Some(&mut total),
        );
    }
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: total,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; an open list; locations naming live resources.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
        let buffer = buffer?;
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&buffer),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: sub,
            },
        };
        list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        Some((buffer, footprint))
    }
}

/// Saves a frame read back by [`read_back`] (8-bit color only).
pub(crate) fn save_read_back(
    frame: u64,
    buffer: &ID3D12Resource,
    footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) {
    save_read_back_as(&format!("frame-{frame}"), buffer, footprint);
}

/// [`save_read_back`] as `<name>.png` in the capture folder (`VICTORY_AB_SHOTS`: `ab-<phase>-<arm>`).
pub(crate) fn save_read_back_as(
    name: &str,
    buffer: &ID3D12Resource,
    footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) {
    let f = footprint.Footprint;
    let bgra = match f.Format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => false,
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => true,
        other => {
            tracing::warn!(format = other.0, "frame not saved: not 8-bit color");
            return;
        }
    };
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished (every submission is waited for); the
    // mapping spans the footprint's rows.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let len = f.RowPitch as usize * f.Height as usize;
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), len);
        save_as(name, f.Width, f.Height, f.RowPitch as usize, bgra, rows);
        buffer.Unmap(0, None);
    }
}

/// With `VICTORY_DUMP_TARGETS` set: title textures the frames listed in
/// `VICTORY_CAPTURE_FRAMES` render to or write, in first-use order, saved
/// at that frame's present as `frame-<n>-<i>-<resource>-<w>x<h>-f<format>.png`
/// (what each holds at the end of the frame; formats normalized to show
/// structure: floats scaled by their largest value, depth by its range).
/// The host copies are held: the title may release a target before the
/// frame is presented.
static TARGETS: std::sync::Mutex<Vec<(usize, std::sync::Arc<crate::resource::HostTexture>)>> =
    std::sync::Mutex::new(Vec::new());

fn dump_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| folder().is_some() && crate::switch::on("VICTORY_DUMP_TARGETS"))
}

/// Notes a title texture the frame being built renders to or writes.
pub(crate) fn note_target(resource: crate::object::This) {
    if resource.is_null() || !dump_on() || !listed(crate::queue::presented() + 1) {
        return;
    }
    let mut targets = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    if !targets.iter().any(|&(r, _)| r == resource as usize)
        && let Some(copy) = crate::resource::host_copy(resource)
    {
        targets.push((resource as usize, copy));
    }
}

/// The buffers (title address, bytes) the captured operations write through
/// UAVs, up to 4 MiB each ([`dump_uavs`]).
static UAV_BUFFERS: std::sync::Mutex<Vec<(u64, u64)>> = std::sync::Mutex::new(Vec::new());

/// Notes a buffer a listed operation writes through a UAV, for
/// `VICTORY_CAPTURE_UAVS=1`.
pub(crate) fn note_uav_buffer(range: Option<(u64, u64)>) {
    let Some((at, len)) = range else { return };
    if len == 0 || len > 4 << 20 || !crate::switch::on("VICTORY_CAPTURE_UAVS") {
        return;
    }
    let mut buffers = UAV_BUFFERS.lock().unwrap_or_else(|e| e.into_inner());
    if buffers.len() < 256 && !buffers.contains(&(at, len)) {
        buffers.push((at, len));
    }
}

/// `VICTORY_CAPTURE_UAVS=1` (with a capture): at the dump, one line per buffer
/// the captured operations write through UAVs, to `frame-<n>-uavs.tsv`: what
/// the title's memory holds of it now (all the submissions have run and
/// landed: what the title's CPU would read), and what its copy is. The
/// terrain's feedback list (`VICTORY_CAPTURE_UAVS` and the ops file name it by
/// address) is read this way: a list that is all zeros with the feedback
/// passes running means nothing was asked for, or never landed.
/// Columns: address, bytes, mapped, nonzero dwords, nonzero dwords in each
/// 64 KiB, the first 12 nonzero dwords as `index=hex`, the copy.
fn dump_uavs(frame: u64, folder: &std::path::Path) {
    let buffers = std::mem::take(&mut *UAV_BUFFERS.lock().unwrap_or_else(|e| e.into_inner()));
    if buffers.is_empty() {
        return;
    }
    let mut text = String::new();
    for (at, len) in buffers {
        let (p, n) = (at as *const u8, len as usize);
        let mapped = crate::texture::mapped_bytes(p, n);
        let mut line = format!("{at:#x}	{len}	{mapped}");
        if mapped == len && n.is_multiple_of(4) {
            // SAFETY: `n` committed bytes of title memory.
            let words = unsafe { std::slice::from_raw_parts(at as *const u32, n / 4) };
            let per_block: Vec<usize> =
                words.chunks(16 << 10).map(|c| c.iter().filter(|&&w| w != 0).count()).collect();
            let first: Vec<String> = words
                .iter()
                .enumerate()
                .filter(|&(_, &w)| w != 0)
                .take(12)
                .map(|(i, w)| format!("{i}={w:x}"))
                .collect();
            line += &format!("	{}	{:?}	[{}]", per_block.iter().sum::<usize>(), per_block, first.join(" "));
        } else {
            line += "	not all mapped	-	-";
        }
        line += &format!("	{}
", crate::shadow::describe_copy(at, len));
        text += &line;
    }
    let _ = std::fs::write(folder.join(format!("frame-{frame}-uavs.tsv")), text);
}

/// At the present of frame `frame`: saves the noted targets.
pub(crate) fn dump_targets(frame: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if crate::switch::on("VICTORY_CAPTURE_UAVS")
        && let Some(folder) = folder()
    {
        dump_uavs(frame, folder);
    }
    if crate::switch::on("VICTORY_CAPTURE_LEVELS")
        && let Some(folder) = folder()
    {
        let lines = crate::tiles::levels_report();
        crate::tiles::status();
        let _ = std::fs::write(folder.join(format!("frame-{frame}-levels.tsv")), lines.join("
"));
        tracing::info!(frame, rows = lines.len().saturating_sub(1), "levels report written");
    }
    let targets = std::mem::take(&mut *TARGETS.lock().unwrap_or_else(|e| e.into_inner()));
    let Some(host) = crate::host::host() else { return };
    for (i, (resource, copy)) in targets.iter().enumerate() {
        let texture = &copy.texture;
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        if d.SampleDesc.Count > 1
            || !matches!(
                d.Dimension,
                D3D12_RESOURCE_DIMENSION_TEXTURE1D | D3D12_RESOURCE_DIMENSION_TEXTURE2D | D3D12_RESOURCE_DIMENSION_TEXTURE3D
            )
        {
            continue;
        }
        // Every submission has run; the dump's list moves it there and back.
        let state = copy.state.load(Relaxed);
        dump_one(host, texture, state, frame, i, *resource as crate::object::This, &d);
    }
}

fn dump_one(
    host: &crate::host::Host,
    texture: &ID3D12Resource,
    state: u32,
    frame: u64,
    i: usize,
    resource: crate::object::This,
    d: &D3D12_RESOURCE_DESC,
) {
    let Ok(list) = host.new_list() else { return };
    let state = D3D12_RESOURCE_STATES(crate::host::legal_state(texture, state) as i32);
    let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
    // SAFETY: an open list of ours and a live texture.
    let read = unsafe {
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, state, copy)]);
        }
        let read = read_back(&host.device, &list.list, texture);
        // The stencil plane of a depth-stencil target, which the title's
        // lighting reads (material class in the low bits; 0x08 "inside a
        // room", which the lamps of a room are multiplied by): the plane
        // after the depth plane of a one-level, one-slice texture.
        let stencil = (matches!(d.Format.0, 19..=22 | 44..=47)
            && d.MipLevels == 1
            && d.DepthOrArraySize == 1)
            .then(|| read_back_of(&host.device, &list.list, texture, 1))
            .flatten();
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, copy, state)]);
        }
        (read, stencil)
    };
    let (read, stencil) = read;
    list.close();
    host.execute(&[&list]);
    if let Some((buffer, footprint)) = stencil {
        stencil_values(&buffer, &footprint, frame, resource);
    }
    let Some((buffer, footprint)) = read else { return };
    let f = footprint.Footprint;
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished; the mapping spans the footprint's rows.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let pitch = f.RowPitch as usize;
        // A volume's slices follow each other (a slice is `Height` rows): the volume is read as
        // `Depth * Height` rows, so its census (a color-grading table's, say) covers every slice.
        let slices = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { usize::from(d.DepthOrArraySize) } else { 1 };
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), pitch * f.Height as usize * slices);
        // A 24-bit depth plane comes back as 32-bit words: which depth values
        // (low 24 bits) and which top bytes it holds, the most common first.
        if matches!(d.Format.0, 44..=47) {
            let mut depths = std::collections::HashMap::<u32, u32>::new();
            let mut tops = std::collections::HashMap::<u32, u32>::new();
            for y in 0..f.Height as usize {
                for x in 0..f.Width as usize {
                    let at = y * pitch + x * 4;
                    let w = u32::from_le_bytes(rows[at..at + 4].try_into().unwrap_or([0; 4]));
                    *depths.entry(w & 0xff_ffff).or_default() += 1;
                    *tops.entry(w >> 24).or_default() += 1;
                }
            }
            let most = |m: std::collections::HashMap<u32, u32>| {
                let mut v: Vec<(u32, u32)> = m.into_iter().collect();
                v.sort_by_key(|e| std::cmp::Reverse(e.1));
                (v.len(), v.into_iter().take(12).collect::<Vec<_>>())
            };
            tracing::info!(?resource, depths = ?most(depths), top_bytes = ?most(tops), "24-bit depth plane values (value, texels)");
        }
        let height = f.Height as usize * slices;
        let rgba = to_rgba(f.Format.0 as u32, f.Width as usize, height, pitch, rows);
        target_stats(frame, i, resource, d, f.Format.0 as u32, f.Width as usize, height, pitch, rows, rgba.as_deref());
        if stats_only() {
            buffer.Unmap(0, None);
            return;
        }
        if let Some(rgba) = rgba {
            let name = format!(
                "frame-{frame}-{i:02}-{resource:?}-{}x{}-f{}",
                d.Width, d.Height, d.Format.0
            );
            save_as(&name, f.Width, height as u32, f.Width as usize * 4, false, &rgba);
        } else {
            tracing::info!(format = f.Format.0, ?resource, "target not dumped: format");
        }
        buffer.Unmap(0, None);
    }
}

/// `VICTORY_DUMP_TARGETS=stats`: no pictures of the targets, only
/// `frame-<n>-targets.tsv`.
fn stats_only() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_DUMP_TARGETS").is_ok_and(|v| v == "stats"))
}

/// Bytes a texel takes in a color or single-channel format the dump reads
/// (`None`: not handled).
fn texel_bytes(format: u32) -> Option<usize> {
    Some(match format {
        1..=4 => 16,
        5..=8 => 12,
        9..=22 => 8,
        23..=47 | 67 | 87..=93 => 4,
        48..=59 => 2,
        60..=65 => 1,
        _ => return None,
    })
}

/// The most distinct values counted in a target.
const DISTINCT_CAP: usize = 1 << 20;

/// How many different texels (`bytes` each) the rows hold, counted up to
/// [`DISTINCT_CAP`] (then `true`), and the most common one with its count.
fn distinct_values(rows: &[u8], pitch: usize, w: usize, h: usize, bytes: usize) -> (usize, bool, u128, u32) {
    let mut counts = std::collections::HashMap::<u128, u32>::new();
    let mut capped = false;
    for y in 0..h {
        let row = &rows[y * pitch..y * pitch + w * bytes];
        for t in row.chunks_exact(bytes) {
            let mut v = [0u8; 16];
            v[..bytes].copy_from_slice(t);
            let key = u128::from_le_bytes(v);
            if let Some(n) = counts.get_mut(&key) {
                *n += 1;
            } else if counts.len() < DISTINCT_CAP {
                counts.insert(key, 1);
            } else {
                capped = true;
            }
        }
    }
    let (top, top_n) = counts.iter().max_by_key(|e| *e.1).map_or((0, 0), |(k, n)| (*k, *n));
    (counts.len(), capped, top, top_n)
}

/// How a format's texels are told alike: exactly, or within a few steps
/// of the float (`Half`/`Float`: that many channels of 16 or 32 bits).
#[derive(Clone, Copy)]
enum Alike {
    Exact,
    Half(usize),
    Float(usize),
}

fn alike_of(format: u32) -> Alike {
    match format {
        1..=2 => Alike::Float(4),
        39..=41 => Alike::Float(1),
        9..=10 => Alike::Half(4),
        33..=34 => Alike::Half(2),
        53..=54 => Alike::Half(1),
        _ => Alike::Exact,
    }
}

/// A float's bits as an integer that grows with the value (sign aside).
fn ordered(bits: i64, sign: i64) -> i64 {
    if bits & sign != 0 { sign - (bits & (sign - 1)) } else { sign + bits }
}

fn texels_alike(a: &[u8], b: &[u8], how: Alike) -> bool {
    match how {
        Alike::Exact => a == b,
        Alike::Half(n) => (0..n).all(|i| {
            let (x, y) = (
                i64::from(u16::from_le_bytes([a[2 * i], a[2 * i + 1]])),
                i64::from(u16::from_le_bytes([b[2 * i], b[2 * i + 1]])),
            );
            (ordered(x, 0x8000) - ordered(y, 0x8000)).abs() <= 4
        }),
        Alike::Float(n) => (0..n).all(|i| {
            let word = |t: &[u8]| i64::from(u32::from_le_bytes(t[4 * i..4 * i + 4].try_into().unwrap_or([0; 4])));
            (ordered(word(a), 0x8000_0000) - ordered(word(b), 0x8000_0000)).abs() <= 4096
        }),
    }
}

/// How flat the picture of a target is, in percent: of horizontally
/// adjacent texel pairs, of vertically adjacent pairs, and of the 8x8
/// blocks that lie whole in it, the share whose texels are alike (equal; for
/// floats within a few steps, 0.05%). Smooth light, with the noise of a
/// render, scores low; flat patches of one value (a posterized picture) high.
/// A volume is read as `Depth * Height` rows.
fn flatness(rows: &[u8], pitch: usize, w: usize, h: usize, bytes: usize, format: u32) -> (f64, f64, f64) {
    let how = alike_of(format);
    let at = |x: usize, y: usize| &rows[y * pitch + x * bytes..][..bytes];
    let share = |same: usize, all: usize| if all == 0 { 0.0 } else { same as f64 * 100.0 / all as f64 };
    let (mut h_same, mut v_same) = (0usize, 0usize);
    for y in 0..h {
        for x in 0..w {
            if x + 1 < w && texels_alike(at(x, y), at(x + 1, y), how) {
                h_same += 1;
            }
            if y + 1 < h && texels_alike(at(x, y), at(x, y + 1), how) {
                v_same += 1;
            }
        }
    }
    let (bw, bh) = (w / 8, h / 8);
    let mut flat = 0usize;
    for by in 0..bh {
        for bx in 0..bw {
            let first = at(bx * 8, by * 8);
            if (0..8).all(|j| (0..8).all(|i| texels_alike(first, at(bx * 8 + i, by * 8 + j), how))) {
                flat += 1;
            }
        }
    }
    (
        share(h_same, h * w.saturating_sub(1)),
        share(v_same, w * h.saturating_sub(1)),
        share(flat, bw * bh),
    )
}

/// What the float texels of a target hold, over the color channels (the first three; the
/// alpha of four is left out), and in every channel how many are no number or infinite.
/// `over` counts the color channels whose finite value is larger than the limit
/// ([`bad_limit`]); `first` is the first texel (rows from the top) with a channel that is no
/// number or infinite or a color channel over the limit, `bbox` the box of all of them
/// (x0, y0, x1, y1) and `at_max` the texel of the largest finite color value: where to look
/// with `victory replay --pixel <id>:x,y` (`VICTORY_NONFINITE_LIMIT`).
#[derive(Debug, Default, PartialEq)]
struct Range {
    /// The largest and the smallest finite value.
    max: f32,
    min: f32,
    /// The mean of the finite values.
    mean: f64,
    nan: u64,
    inf: u64,
    over: u64,
    first: Option<(u32, u32)>,
    bbox: [u32; 4],
    at_max: (u32, u32),
}

/// A value from which a channel is "extreme" (`VICTORY_NONFINITE_LIMIT`, 60000: a half
/// float's largest is 65504, so what a half target saturates at counts).
fn bad_limit() -> f32 {
    static LIMIT: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("VICTORY_NONFINITE_LIMIT")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(60000.0)
    })
}

/// The channels a float format has and how a channel is read: 32-bit (32), 16-bit (16),
/// R11G11B10 (11) or R9G9B9E5 (9); `None` for any other format.
fn float_layout(format: u32) -> Option<(usize, u32)> {
    Some(match format {
        2 => (4, 32),
        6 => (3, 32),
        16 => (2, 32),
        41 => (1, 32),
        10 => (4, 16),
        34 => (2, 16),
        54 => (1, 16),
        26 => (3, 11),
        67 => (3, 9),
        _ => return None,
    })
}

/// Channel `c` of a texel `t` read as `kind` ([`float_layout`]): (a value; whether it is no
/// number; whether it is an infinity).
fn channel(kind: u32, c: usize, t: &[u8]) -> (f32, bool, bool) {
    match kind {
        32 => {
            let f = f32::from_le_bytes(t[4 * c..4 * c + 4].try_into().unwrap_or([0; 4]));
            (f, f.is_nan(), f.is_infinite())
        }
        9 => {
            // Three 9-bit mantissas and one shared exponent: no NaN, no infinity.
            let w = u32::from_le_bytes(t.try_into().unwrap_or([0; 4]));
            let m = (w >> (9 * c)) & 511;
            (m as f32 * 2f32.powi((w >> 27) as i32 - 24), false, false)
        }
        16 => {
            let b = u16::from_le_bytes([t[2 * c], t[2 * c + 1]]);
            let special = (b >> 10) & 0x1f == 0x1f;
            (half(b), special && b & 0x3ff != 0, special && b & 0x3ff == 0)
        }
        _ => {
            let w = u32::from_le_bytes(t.try_into().unwrap_or([0; 4]));
            let (bits, man) = [(w & 0x7ff, 6), ((w >> 11) & 0x7ff, 6), (w >> 22, 5)][c];
            let special = (bits >> man) & 0x1f == 0x1f;
            (small_float(bits, man), special && bits & ((1 << man) - 1) != 0, special && bits & ((1 << man) - 1) == 0)
        }
    }
}

/// [`Range`] of the rows of a float format (`None` for any other).
fn float_range(format: u32, w: usize, h: usize, pitch: usize, rows: &[u8], limit: f32) -> Option<Range> {
    let (channels, kind) = float_layout(format)?;
    let bytes = match kind {
        32 => 4 * channels,
        16 => 2 * channels,
        _ => 4,
    };
    let mut r = Range { max: f32::MIN, min: f32::MAX, bbox: [u32::MAX, u32::MAX, 0, 0], ..Range::default() };
    let (mut sum, mut n) = (0f64, 0u64);
    let mut largest = f32::MIN;
    for y in 0..h {
        let row = rows.get(y * pitch..y * pitch + w * bytes)?;
        for (x, t) in row.chunks_exact(bytes).enumerate() {
            let mut bad = false;
            for c in 0..channels {
                let (v, nan, inf) = channel(kind, c, t);
                r.nan += u64::from(nan);
                r.inf += u64::from(inf);
                bad |= nan || inf;
                if c < 3 && v.is_finite() {
                    r.max = r.max.max(v);
                    r.min = r.min.min(v);
                    sum += f64::from(v);
                    n += 1;
                    if v.abs() > limit {
                        r.over += 1;
                        bad = true;
                    }
                    if v > largest {
                        largest = v;
                        r.at_max = (x as u32, y as u32);
                    }
                }
            }
            if bad {
                let (x, y) = (x as u32, y as u32);
                r.first.get_or_insert((x, y));
                r.bbox = [r.bbox[0].min(x), r.bbox[1].min(y), r.bbox[2].max(x), r.bbox[3].max(y)];
            }
        }
    }
    if n == 0 {
        r.max = 0.0;
        r.min = 0.0;
    }
    if r.first.is_none() {
        r.bbox = [0; 4];
    }
    r.mean = if n == 0 { 0.0 } else { sum / n as f64 };
    Some(r)
}

/// The texels of a float target with a channel that is no number, infinite or (color) over
/// `limit`: the first `first` in reading order and then the `largest` with the largest
/// finite color value, as (x, y, the four channels as read; unused ones 0).
#[allow(clippy::too_many_arguments)]
fn bad_texels(
    format: u32,
    w: usize,
    h: usize,
    pitch: usize,
    rows: &[u8],
    limit: f32,
    first: usize,
    largest: usize,
) -> Vec<(u32, u32, [f32; 4])> {
    let Some((channels, kind)) = float_layout(format) else { return Vec::new() };
    let bytes = match kind {
        32 => 4 * channels,
        16 => 2 * channels,
        _ => 4,
    };
    let mut firsts: Vec<(u32, u32, [f32; 4])> = Vec::new();
    // The largest ones: (magnitude as ordered bits, texel), kept sorted, at most `largest`.
    let mut top: Vec<(u32, (u32, u32, [f32; 4]))> = Vec::new();
    for y in 0..h {
        let Some(row) = rows.get(y * pitch..y * pitch + w * bytes) else { break };
        for (x, t) in row.chunks_exact(bytes).enumerate() {
            let mut values = [0f32; 4];
            let (mut bad, mut magnitude) = (false, 0f32);
            for c in 0..channels {
                let (v, nan, inf) = channel(kind, c, t);
                values[c] = v;
                bad |= nan || inf || c < 3 && v.is_finite() && v.abs() > limit;
                if c < 3 && v.is_finite() {
                    magnitude = magnitude.max(v.abs());
                }
            }
            if !bad {
                continue;
            }
            let texel = (x as u32, y as u32, values);
            if firsts.len() < first {
                firsts.push(texel);
            }
            let key = magnitude.to_bits();
            if largest > 0 && (top.len() < largest || key > top[0].0) {
                let at = top.partition_point(|e| e.0 < key);
                top.insert(at, (key, texel));
                if top.len() > largest {
                    top.remove(0);
                }
            }
        }
    }
    firsts.extend(top.into_iter().rev().map(|e| e.1));
    firsts
}

/// One line per target in `frame-<frame>-targets.tsv` (in the order the
/// frame first used them, which the `-ops.txt` lines name by resource):
/// how many different texel values the target holds as the card wrote them
/// (`raw`, counted up to a million) and as the 8-bit picture of it
/// (`rgba8`), the most common value and its share. A pass whose output
/// collapses to a few values where the run before had thousands is the one
/// that lost the detail.
#[allow(clippy::too_many_arguments)]
fn target_stats(
    frame: u64,
    i: usize,
    resource: crate::object::This,
    d: &D3D12_RESOURCE_DESC,
    format: u32,
    w: usize,
    h: usize,
    pitch: usize,
    rows: &[u8],
    rgba: Option<&[u8]>,
) {
    use std::io::Write;
    let Some(folder) = folder() else { return };
    let Some(bytes) = texel_bytes(format) else { return };
    if w == 0 || h == 0 || rows.len() < pitch * (h - 1) + w * bytes {
        return;
    }
    let (distinct, capped, top, top_n) = distinct_values(rows, pitch, w, h, bytes);
    let (eqh, eqv, flat8) = flatness(rows, pitch, w, h, bytes, format);
    let rgba8 = rgba.map_or(0, |p| {
        p.chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], 0]))
            .collect::<std::collections::HashSet<u32>>()
            .len()
    });
    // Float targets: the range of their values, the columns after the flatness ones
    // (`max`, `min` and `mean` over the color channels; `nan` and `inf` count channels).
    // (An R32_UINT target may be the title's diffuse light, which it packs as R9G9B9E5: its
    // `e5max` and `e5mean` read the words that way, and mean nothing for any other counter.)
    let range = if format == 42 {
        float_range(67, w, h, pitch, rows, bad_limit())
            .map_or(String::new(), |r| format!("	e5max{:.4e}	e5mean{:.4e}", r.max, r.mean))
    } else {
        // After `inf`: where to look (`VICTORY_NONFINITE_LIMIT`): the channels over the limit,
        // the first texel that is no number, infinite or over it, the box of them and the
        // texel of the largest value; the worst ones are listed in `frame-<n>-bad.tsv`.
        float_range(format, w, h, pitch, rows, bad_limit()).map_or(String::new(), |r| {
            let at = |p: (u32, u32)| format!("{},{}", p.0, p.1);
            let bad = match r.first {
                Some(p) => format!("{}	bbox{},{},{},{}", at(p), r.bbox[0], r.bbox[1], r.bbox[2], r.bbox[3]),
                None => "-	bbox-".to_owned(),
            };
            format!(
                "	max{:.4e}	min{:.4e}	mean{:.4e}	nan{}	inf{}	over{}	first{bad}	maxat{}",
                r.max,
                r.min,
                r.mean,
                r.nan,
                r.inf,
                r.over,
                at(r.at_max)
            )
        })
    };
    if range.contains("	first") && !range.contains("	first-") {
        let listed = bad_texels(format, w, h, pitch, rows, bad_limit(), 16, 16);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(folder.join(format!("frame-{frame}-bad.tsv")))
        {
            for (x, y, v) in listed {
                let _ = writeln!(
                    file,
                    "{i}	{resource:?}	{}x{}	f{format}	{x},{y}	{:e} {:e} {:e} {:e}",
                    d.Width, d.Height, v[0], v[1], v[2], v[3]
                );
            }
        }
    }
    let line = format!(
        "{i}	{resource:?}	{}x{}{}	f{}	mips{}	texels{}	raw{}{}	top{top:#x}	topshare{:.1}%	rgba8_{rgba8}	eqh{eqh:.1}%	eqv{eqv:.1}%	flat8_{flat8:.1}%{range}",
        d.Width,
        d.Height,
        if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { format!("x{}", d.DepthOrArraySize) } else { String::new() },
        format,
        d.MipLevels,
        w * h,
        distinct,
        if capped { "+" } else { "" },
        top_n as f64 * 100.0 / (w * h) as f64,
    );
    // Small targets (exposure and tone tables, a 4096-entry curve, light parameters): every
    // texel as the card holds it, in hex, in `frame-<n>-values.tsv`, to compare processes.
    if w * h <= 65536 && bytes <= 16 {
        let mut values = String::new();
        for y in 0..h {
            for x in 0..w {
                let t = &rows[y * pitch + x * bytes..][..bytes];
                for b in t.iter().rev() {
                    values.push_str(&format!("{b:02x}"));
                }
                values.push(' ');
            }
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(folder.join(format!("frame-{frame}-values.tsv")))
        {
            let _ = writeln!(file, "{i}	{resource:?}	{}x{}	f{format}	{}", d.Width, d.Height, values.trim_end());
        }
    }
    tracing::debug!(frame, "target {line}");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(folder.join(format!("frame-{frame}-targets.tsv")))
    {
        let _ = writeln!(file, "{line}");
    }
}

/// Logs which values a stencil plane read back by [`read_back_of`] holds
/// (the most common first), how many texels have each of its bits, and the
/// box of the texels with bit 3 (0x08).
fn stencil_values(
    buffer: &ID3D12Resource,
    footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    frame: u64,
    resource: crate::object::This,
) {
    let f = footprint.Footprint;
    let (w, h, pitch) = (f.Width as usize, f.Height as usize, f.RowPitch as usize);
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished; the mapping spans the footprint's rows of
    // one byte a texel, the last row without its padding.
    unsafe {
        if h == 0 || w == 0 || buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), pitch * (h - 1) + w);
        let mut values = [0u32; 256];
        let mut bits = [0u32; 8];
        // Left, top, right, bottom of the texels with bit 3.
        let mut inside = [u32::MAX, u32::MAX, 0, 0];
        for y in 0..h {
            for (x, &v) in rows[y * pitch..y * pitch + w].iter().enumerate() {
                values[usize::from(v)] += 1;
                for (bit, count) in bits.iter_mut().enumerate() {
                    *count += u32::from(v >> bit & 1);
                }
                if v & 8 != 0 {
                    inside = [
                        inside[0].min(x as u32),
                        inside[1].min(y as u32),
                        inside[2].max(x as u32),
                        inside[3].max(y as u32),
                    ];
                }
            }
        }
        let mut most: Vec<(usize, u32)> = values
            .iter()
            .copied()
            .enumerate()
            .filter(|v| v.1 != 0)
            .collect();
        most.sort_by_key(|v| std::cmp::Reverse(v.1));
        most.truncate(16);
        tracing::info!(
            frame,
            ?resource,
            width = w,
            height = h,
            values = ?most,
            texels_with_bit = ?bits,
            bit3_box = ?inside,
            "stencil plane values (value, texels)"
        );
        buffer.Unmap(0, None);
    }
}

fn half(h: u16) -> f32 {
    let (sign, exp, man) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
    let v = match exp {
        0 => man * 2f32.powi(-24),
        31 if man != 0.0 => f32::NAN,
        31 => f32::INFINITY,
        e => (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    };
    if sign == 1 { -v } else { v }
}

/// A float packed in `bits` with a 5-bit exponent and `man` mantissa bits
/// (the 11- and 10-bit floats of R11G11B10).
fn small_float(bits: u32, man: u32) -> f32 {
    let (exp, m) = ((bits >> man) & 0x1f, (bits & ((1 << man) - 1)) as f32);
    match exp {
        0 => m / (1 << man) as f32 * 2f32.powi(-14),
        31 if m != 0.0 => f32::NAN,
        31 => f32::INFINITY,
        e => (1.0 + m / (1 << man) as f32) * 2f32.powi(e as i32 - 15),
    }
}

/// Rows of a host format as 8-bit RGBA: 8-bit color as is, everything else
/// as channel values scaled so the largest finite one is white (single
/// channels: lowest black, highest white, as grey). `None` for formats not
/// handled.
fn to_rgba(format: u32, w: usize, h: usize, pitch: usize, rows: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(w * h * 4);
    let px = |x: usize, y: usize, n: usize| &rows[y * pitch + x * n..][..n];
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[2 * i], b[2 * i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap_or([0; 4]));
    // Channel values as floats, then scaled.
    type Decode = Box<dyn Fn(&[u8]) -> [f32; 3]>;
    let (n, decode): (usize, Decode) = match format {
        27..=32 => {
            // The PNG drops alpha: its range and how many texels have each
            // channel zero are logged.
            let (mut low, mut high, mut zero) = ([255u8; 4], [0u8; 4], [0usize; 4]);
            for y in 0..h {
                for x in 0..w {
                    let p = px(x, y, 4);
                    for c in 0..4 {
                        (low[c], high[c]) = (low[c].min(p[c]), high[c].max(p[c]));
                        zero[c] += usize::from(p[c] == 0);
                    }
                    out.extend([p[0], p[1], p[2], 255]);
                }
            }
            tracing::info!(format, ?low, ?high, ?zero, texels = w * h, "target values (8-bit, RGBA)");
            return Some(out);
        }
        // B8G8R8A8 and B8G8R8X8 (89 is R10G10B10_XR_BIAS_A2, below).
        87 | 88 | 90..=93 => {
            for y in 0..h {
                for x in 0..w {
                    let p = px(x, y, 4);
                    out.extend([p[2], p[1], p[0], 255]);
                }
            }
            return Some(out);
        }
        23..=25 | 89 => (4, Box::new(move |b| {
            let v = u32_at(b, 0);
            [(v & 0x3ff) as f32, (v >> 10 & 0x3ff) as f32, (v >> 20 & 0x3ff) as f32]
        })),
        26 => (4, Box::new(move |b| {
            let v = u32_at(b, 0);
            [small_float(v & 0x7ff, 6), small_float(v >> 11 & 0x7ff, 6), small_float(v >> 22, 5)]
        })),
        9..=10 => (8, Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), half(u16_at(b, 2))])),
        11..=14 => (8, Box::new(move |b| [0, 1, 2].map(|i| f32::from(u16_at(b, i))))),
        33..=34 => (4, Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), 0.0])),
        1..=2 => (16, Box::new(move |b| {
            [0, 1, 2].map(|i| f32::from_bits(u32_at(b, i)))
        })),
        15..=16 => (8, Box::new(move |b| [f32::from_bits(u32_at(b, 0)), f32::from_bits(u32_at(b, 1)), 0.0])),
        35..=38 => (4, Box::new(move |b| [f32::from(u16_at(b, 0)), f32::from(u16_at(b, 1)), 0.0])),
        48..=52 => (2, Box::new(move |b| [f32::from(b[0]), f32::from(b[1]), 0.0])),
        // Single channels (and depth planes): shown as grey.
        39..=43 => (4, Box::new(move |b| [f32::from_bits(u32_at(b, 0)); 3])),
        19..=22 => (8, Box::new(move |b| [f32::from_bits(u32_at(b, 0)); 3])),
        44..=47 => (4, Box::new(move |b| [(u32_at(b, 0) & 0xff_ffff) as f32; 3])),
        53..=54 => (2, Box::new(move |b| [half(u16_at(b, 0)); 3])),
        55..=59 => (2, Box::new(move |b| [f32::from(u16_at(b, 0)); 3])),
        60..=65 => (1, Box::new(move |b| [f32::from(b[0]); 3])),
        _ => return None,
    };
    let values: Vec<[f32; 3]> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| decode(px(x, y, n)))
        .collect();
    let grey = matches!(format, 19..=22 | 39..=47 | 53..=65);
    let finite = values.iter().flatten().copied().filter(|v| v.is_finite());
    let (low, high) = if grey {
        finite.fold((f32::MAX, f32::MIN), |(l, h), v| (l.min(v), h.max(v)))
    } else {
        (0.0, finite.fold(0.0f32, f32::max))
    };
    let range = (high - low).max(1e-20);
    // How much of the target holds anything: a lit scene and one bright
    // lamp on black have the same highest value.
    let above = values.iter().filter(|v| v.iter().any(|c| c.is_finite() && *c > low)).count();
    let nonzero_percent = above as f32 * 100.0 / values.len().max(1) as f32;
    tracing::info!(format, low, high, nonzero_percent, "target values");
    // Small float colour targets (the 1x1 luminance and exposure targets):
    // every texel's four channels as numbers. The picture has no alpha and
    // eight bits a channel.
    if matches!(format, 1..=2 | 9..=10) && w * h <= 16 {
        for y in 0..h {
            for x in 0..w {
                let b = px(x, y, n);
                let texel: [f32; 4] = if n == 16 {
                    [0, 1, 2, 3].map(|i| f32::from_bits(u32_at(b, i)))
                } else {
                    [0, 1, 2, 3].map(|i| half(u16_at(b, i)))
                };
                tracing::info!(format, x, y, ?texel, "target texel");
            }
        }
    }
    // R32_UINT targets hold packed light (the title reads them back as
    // R9G9B9E5): as floats their low, high and share above the lowest say
    // nothing. How many texels hold any bit, and the highest channel value
    // of the packed reading.
    if format == 42 {
        let (mut set, mut packed_high) = (0usize, 0f32);
        for y in 0..h {
            for x in 0..w {
                let v = u32_at(px(x, y, 4), 0);
                set += usize::from(v != 0);
                let scale = 2f32.powi((v >> 27) as i32 - 24);
                for k in 0..3 {
                    packed_high = packed_high.max((v >> (9 * k) & 0x1ff) as f32 * scale);
                }
            }
        }
        let nonzero_bits_percent = set as f32 * 100.0 / (w * h).max(1) as f32;
        tracing::info!(format, nonzero_bits_percent, packed_high, "target values as packed light");
        // The same words as plain numbers, by class. Tables of indices and
        // masks say nothing as floats or as packed light: in the title's
        // shadow-mask table 0 is a block in full shadow, a word up to 255
        // one value for the block, above that a block of the atlas, and all
        // ones a texel never written.
        let mut classes = [0usize; 6];
        let mut highest_below_ones = 0u32;
        for y in 0..h {
            for x in 0..w {
                let v = u32_at(px(x, y, 4), 0);
                classes[match v {
                    0 => 0,
                    1..=15 => 1,
                    16..=255 => 2,
                    256..=0x00ff_ffff => 3,
                    u32::MAX => 5,
                    _ => 4,
                }] += 1;
                if v != u32::MAX {
                    highest_below_ones = highest_below_ones.max(v);
                }
            }
        }
        tracing::info!(
            format,
            width = w,
            height = h,
            zero = classes[0],
            one_to_15 = classes[1],
            to_255 = classes[2],
            to_24_bits = classes[3],
            larger = classes[4],
            all_ones = classes[5],
            highest_below_ones,
            "target values as words"
        );
    }
    // Float colours (scene light) go through a cube root: scaled linearly,
    // a night scene under one lamp of 50 units came out all black.
    let curved = matches!(format, 1..=2 | 9..=10 | 26);
    for v in values {
        let c = v.map(|c| {
            let t = ((c - low) / range).clamp(0.0, 1.0);
            ((if curved { t.cbrt() } else { t }) * 255.0) as u8
        });
        out.extend([c[0], c[1], c[2], 255]);
    }
    Some(out)
}

/// Pixels per side of the grid [`probe`] reads.
const GRID: u32 = 4;
/// Bytes between the grid's pixels in the readback buffer (placement
/// alignment).
const SLOT: u64 = 512;

/// Records copies of a grid of pixels of `texture` (8-bit color, in the
/// copy-source state) into a readback buffer, to tell black frames.
pub(crate) fn probe(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<ID3D12Resource> {
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    if !matches!(d.Format.0, 27..=29 | 87 | 90 | 91) {
        return None;
    }
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: SLOT * u64::from(GRID * GRID),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; an open list; locations naming live resources.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
        let buffer = buffer?;
        for i in 0..GRID * GRID {
            let (x, y) = (
                (2 * (i % GRID) + 1) * d.Width as u32 / (2 * GRID),
                (2 * (i / GRID) + 1) * d.Height / (2 * GRID),
            );
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&buffer),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: SLOT * u64::from(i),
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: d.Format,
                            Width: 1,
                            Height: 1,
                            Depth: 1,
                            RowPitch: 256,
                        },
                    },
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            let pixel = D3D12_BOX {
                left: x,
                top: y,
                front: 0,
                right: x + 1,
                bottom: y + 1,
                back: 1,
            };
            list.CopyTextureRegion(&to, 0, 0, 0, &from, Some(&pixel));
        }
        Some(buffer)
    }
}

/// Whether every pixel [`probe`] read is (near) black.
pub(crate) fn black(buffer: &ID3D12Resource) -> bool {
    let mut data = std::ptr::null_mut();
    // SAFETY: the copies finished (every submission is waited for).
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return false;
        }
        let bytes =
            std::slice::from_raw_parts(data.cast::<u8>(), (SLOT * u64::from(GRID * GRID)) as usize);
        let level = bytes
            .chunks(SLOT as usize)
            .flat_map(|pixel| pixel[..3].iter().copied())
            .max()
            .unwrap_or(0);
        buffer.Unmap(0, None);
        let mut levels = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
        if levels.len() == 12 {
            levels.pop_front();
        }
        levels.push_back(level);
        level < 8
    }
}

/// The brightest probed channel of the last frames, oldest first: a fade
/// darkens over several, a glitch drops at once.
static LEVELS: std::sync::Mutex<std::collections::VecDeque<u8>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

pub(crate) fn recent_levels() -> Vec<u8> {
    let levels = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
    levels.iter().copied().collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn half_classifies_special_and_finite_values() {
        use super::half;

        assert!(half(0x7c01).is_nan());
        assert!(half(0xfe00).is_nan());
        assert_eq!(half(0x7c00), f32::INFINITY);
        assert_eq!(half(0xfc00), f32::NEG_INFINITY);
        assert_eq!(half(0x0000).to_bits(), 0.0f32.to_bits());
        assert_eq!(half(0x8000).to_bits(), (-0.0f32).to_bits());
        assert_eq!(half(0x0001), 2f32.powi(-24));
        assert_eq!(half(0xbe00), -1.5);
    }

    #[test]
    fn packed_float_classifies_special_and_finite_values() {
        use super::small_float;

        for mantissa_bits in [5, 6] {
            let special = 31 << mantissa_bits;
            assert!(small_float(special | 1, mantissa_bits).is_nan());
            assert!(small_float(special | (1 << (mantissa_bits - 1)), mantissa_bits).is_nan());
            assert_eq!(small_float(special, mantissa_bits), f32::INFINITY);
            assert_eq!(small_float(0, mantissa_bits).to_bits(), 0.0f32.to_bits());
            assert_eq!(small_float(1, mantissa_bits), 2f32.powi(-14 - mantissa_bits as i32));
            assert_eq!(small_float(15 << mantissa_bits, mantissa_bits), 1.0);
        }
    }

    #[test]
    fn hdr_targets_render_nan_and_infinity_as_distinct_channel_values() {
        // R16G16B16A16_FLOAT: [NaN, +Inf, NaN], [1, 0.5, 0].
        let half = [0x7c01u16, 0x7c00, 0xfe00, 0, 0x3c00, 0x3800, 0, 0];
        let half: Vec<_> = half.into_iter().flat_map(u16::to_le_bytes).collect();
        // R11G11B10_FLOAT: the same values with 6-bit and 5-bit mantissas.
        let packed = [
            0x7c1u32 | (0x7c0 << 11) | (0x3e1 << 22),
            0x3c0 | (0x380 << 11),
        ];
        let packed: Vec<_> = packed.into_iter().flat_map(u32::to_le_bytes).collect();
        let expected = [0, 255, 0, 255, 255, 127, 0, 255];
        for (format, row) in [(10, half), (26, packed)] {
            assert_eq!(
                super::to_rgba(format, 2, 1, row.len(), &row).as_deref(),
                Some(&expected[..]),
                "format {format}"
            );
        }
    }

    #[test]
    fn crc_matches_the_png_check_value() {
        assert_eq!(super::crc32(b"IEND"), 0xae42_6082);
    }

    /// R16G16B16A16_UNORM is integers, not half floats; 89 is 10:10:10:2,
    /// not B8G8R8A8.
    #[test]
    fn unorm16_and_xr_bias_targets_decode_as_their_formats() {
        // Red 0x8000 (a half-float -0), green 0x4000, blue 0.
        let unorm16 = [0x00, 0x80, 0x00, 0x40, 0, 0, 0xff, 0xff];
        assert_eq!(super::to_rgba(11, 1, 1, 8, &unorm16), Some(vec![255, 127, 0, 255]));
        // Red 0x3ff, green 0x1ff, blue 0, alpha 3.
        let xr = (0x3ffu32 | 0x1ff << 10 | 3 << 30).to_le_bytes();
        assert_eq!(super::to_rgba(89, 1, 1, 4, &xr), Some(vec![255, 127, 0, 255]));
        // B8G8R8A8 still swaps to RGBA.
        assert_eq!(super::to_rgba(87, 1, 1, 4, &[1, 2, 3, 4]), Some(vec![3, 2, 1, 255]));
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;

    #[test]
    fn distinct_texels_are_counted_without_the_row_padding() {
        // 3x2 texels of 4 bytes, rows 16 bytes apart (4 bytes of padding that differ).
        let mut rows = vec![0u8; 16 * 2];
        for (y, row) in [[1u32, 1, 2], [1, 3, 1]].iter().enumerate() {
            for (x, v) in row.iter().enumerate() {
                rows[y * 16 + x * 4..][..4].copy_from_slice(&v.to_le_bytes());
            }
            rows[y * 16 + 12..][..4].copy_from_slice(&(99 + y as u32).to_le_bytes());
        }
        let (distinct, capped, top, top_n) = distinct_values(&rows, 16, 3, 2, 4);
        assert_eq!((distinct, capped, top, top_n), (3, false, 1, 4));
        assert_eq!(texel_bytes(28), Some(4));
        assert_eq!(texel_bytes(10), Some(8));
        assert_eq!(texel_bytes(61), Some(1));
        assert_eq!(texel_bytes(71), None);
    }

    #[test]
    fn flat_patches_score_high_and_noise_low() {
        // 16x16 texels of 4 bytes (R8G8B8A8): the left half one value, the right half another.
        let mut rows = vec![0u8; 16 * 16 * 4];
        for y in 0..16 {
            for x in 0..16 {
                rows[(y * 16 + x) * 4..][..4].copy_from_slice(&[if x < 8 { 1 } else { 2 }, 0, 0, 255]);
            }
        }
        let (eqh, eqv, flat) = flatness(&rows, 64, 16, 16, 4, 28);
        assert!((eqh - 224.0 * 100.0 / 240.0).abs() < 1e-9 && eqv == 100.0 && flat == 100.0);
        // The same size with a different value in every texel.
        for (i, t) in rows.chunks_exact_mut(4).enumerate() {
            t.copy_from_slice(&(i as u32).to_le_bytes());
        }
        let (eqh, eqv, flat) = flatness(&rows, 64, 16, 16, 4, 28);
        assert_eq!((eqh, eqv, flat), (0.0, 0.0, 0.0));
    }

    #[test]
    fn float_ranges_count_what_is_no_number_or_infinite() {
        // Two texels of R16G16B16A16_FLOAT, rows padded to 16 bytes: (1, 2, 3, NaN-alpha)
        // and (65504, +Inf, 0.5, 1).
        let half = |v: f32| -> u16 {
            match v {
                1.0 => 0x3c00,
                2.0 => 0x4000,
                3.0 => 0x4200,
                0.5 => 0x3800,
                65504.0 => 0x7bff,
                _ => unreachable!(),
            }
        };
        let words = [half(1.0), half(2.0), half(3.0), 0x7e00, half(65504.0), 0x7c00, half(0.5), half(1.0)];
        let mut rows = vec![0u8; 16];
        for (i, w) in words.iter().enumerate() {
            rows[2 * i..2 * i + 2].copy_from_slice(&w.to_le_bytes());
        }
        let r = float_range(10, 2, 1, 16, &rows, 60000.0).unwrap();
        assert_eq!((r.nan, r.inf), (1, 1));
        assert_eq!((r.max, r.min), (65504.0, 0.5));
        // R11G11B10_FLOAT: red 1.0 (exponent 15), green infinity, blue NaN (exponent 31, mantissa 1).
        let packed = 15u32 << 6 | (0x1f << 6) << 11 | (0x1f << 5 | 1) << 22;
        let r = float_range(26, 1, 1, 4, &packed.to_le_bytes(), 60000.0).unwrap();
        assert_eq!((r.nan, r.inf, r.max, r.min), (1, 1, 1.0, 1.0));
        // Single 32-bit channels, and a format that is not a float.
        let rows: Vec<u8> = [4.0f32, f32::NAN, f32::NEG_INFINITY, -2.0].iter().flat_map(|f| f.to_le_bytes()).collect();
        let r = float_range(41, 4, 1, 16, &rows, 60000.0).unwrap();
        assert_eq!((r.nan, r.inf, r.max, r.min, r.mean), (1, 1, 4.0, -2.0, 1.0));
        assert!(float_range(28, 1, 1, 4, &[0; 4], 60000.0).is_none());
        // R9G9B9E5: red 511 x 2^(31-24) = 65408 (the largest), green 1 x 2^7, blue 0.
        let w = 511u32 | 1 << 9 | 31 << 27;
        let r = float_range(67, 1, 1, 4, &w.to_le_bytes(), 60000.0).unwrap();
        assert_eq!((r.nan, r.inf, r.max, r.min), (0, 0, 65408.0, 0.0));
        let one = 256u32 | 15 << 27; // red 256 x 2^(15-24) = 0.5; green and blue 0
        let r = float_range(67, 1, 1, 4, &one.to_le_bytes(), 60000.0).unwrap();
        assert_eq!((r.max, r.min), (0.5, 0.0));
    }

    #[test]
    fn extreme_and_non_finite_texels_are_located() {
        // R16G16B16A16_FLOAT, 3 x 2 texels, rows padded to 32 bytes: one texel with a channel at
        // the half maximum (65504, over a limit of 60000) at (2, 0), a NaN in the green channel
        // at (1, 1), and a NaN in the alpha only at (0, 1) (alpha is no color, still no number).
        let put = |rows: &mut Vec<u8>, x: usize, y: usize, v: [u16; 4]| {
            for (c, w) in v.iter().enumerate() {
                rows[y * 32 + x * 8 + 2 * c..][..2].copy_from_slice(&w.to_le_bytes());
            }
        };
        let one = 0x3c00u16;
        let mut rows = vec![0u8; 64];
        for y in 0..2 {
            for x in 0..3 {
                put(&mut rows, x, y, [one, one, one, one]);
            }
        }
        put(&mut rows, 2, 0, [0x7bff, one, one, one]);
        put(&mut rows, 1, 1, [one, 0x7e00, one, one]);
        put(&mut rows, 0, 1, [one, one, one, 0x7e00]);
        let r = float_range(10, 3, 2, 32, &rows, 60000.0).unwrap();
        assert_eq!((r.nan, r.inf, r.over), (2, 0, 1));
        assert_eq!(r.first, Some((2, 0)));
        assert_eq!(r.bbox, [0, 0, 2, 1]);
        assert_eq!(r.at_max, (2, 0));
        // A clean target has no first texel and an empty box; a higher limit lets 65504 pass.
        let r = float_range(10, 3, 2, 32, &rows, 70000.0).unwrap();
        assert_eq!((r.over, r.first), (0, Some((0, 1))));
        let clean = vec![0u8; 64];
        let r = float_range(10, 3, 2, 32, &clean, 60000.0).unwrap();
        assert_eq!((r.over, r.first, r.bbox), (0, None, [0; 4]));
        // The list: the first one in reading order (2,0) then (0,1), (1,1); the largest by value
        // last-first: (2,0) is the only finite extreme one, the NaN texels rank by their finite colors.
        let list = bad_texels(10, 3, 2, 32, &rows, 60000.0, 2, 1);
        assert_eq!(list.iter().map(|t| (t.0, t.1)).collect::<Vec<_>>(), [(2, 0), (0, 1), (2, 0)]);
        assert_eq!(list[0].2[0], 65504.0);
        assert!(bad_texels(10, 3, 2, 32, &clean, 60000.0, 4, 4).is_empty());
        // R11G11B10: red at the largest finite exponent (30) is 65024-ish, over a limit of 60000.
        let packed = (30u32 << 6 | 63) | (15 << 6) << 11 | (15 << 5) << 22;
        let r = float_range(26, 1, 1, 4, &packed.to_le_bytes(), 60000.0).unwrap();
        assert_eq!((r.over, r.first, r.at_max), (1, Some((0, 0)), (0, 0)));
    }

    #[test]
    fn floats_a_few_steps_apart_are_alike() {
        let f = |v: f32| v.to_le_bytes();
        assert!(texels_alike(&f(1.0), &f(1.0 + 1.0 / 4096.0), Alike::Float(1)));
        assert!(!texels_alike(&f(1.0), &f(1.01), Alike::Float(1)));
        assert!(!texels_alike(&f(1.0), &f(-1.0), Alike::Float(1)));
        // Half floats: 1.0 is 0x3c00, one step is 0x3c01, 1.01 is about 0x3c0a.
        let h = |v: u16| v.to_le_bytes();
        assert!(texels_alike(&h(0x3c00), &h(0x3c03), Alike::Half(1)));
        assert!(!texels_alike(&h(0x3c00), &h(0x3c0a), Alike::Half(1)));
    }
}
