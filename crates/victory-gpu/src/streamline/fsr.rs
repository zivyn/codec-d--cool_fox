//! AMD FSR 3 frame generation (the FidelityFX API's frame generation and its
//! frame interpolation swap chain), for cards DLSS Frame Generation does not
//! run on. Off by default; loaded at run time from a folder of the user's.
//!
//! - `VICTORY_FSR_FRAMEGEN=1`: our swap chain is FidelityFX's frame
//!   interpolation swap chain, and every present is told the depth and motion
//!   vectors of the last upscaling submitted before it (the same inputs DLSS
//!   Frame Generation is given), so that one frame is generated between each
//!   two the title renders. With `VICTORY_FRAMEGEN=1` as well, DLSS Frame
//!   Generation is used where it runs, this where it does not; never both.
//! - `VICTORY_FIDELITYFX_DIR`: the folder of `amd_fidelityfx_loader_dx12.dll`
//!   and `amd_fidelityfx_framegeneration_dx12.dll` (AMD's signed DLLs of the
//!   FidelityFX SDK's `bin`; default: a `fidelityfx` folder beside our exe).
//!
//! Anything missing (the folder, a signature, the swap chain) leaves our plain
//! present as it was, with a warning in the log. Like DLSS Frame Generation,
//! it needs the line of submissions (`VICTORY_THIN=1` or
//! `VICTORY_ASYNC_SUBMIT=1`), and the title's upscaling has to run (its own
//! FidelityFX at the Series S sizes, or DLSS).

use std::ffi::{CStr, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

use super::{Library, plan};

const LOADER: &str = "amd_fidelityfx_loader_dx12.dll";
const FRAMEGEN: &str = "amd_fidelityfx_framegeneration_dx12.dll";

// Structure types (ffx_api.h, ffx_framegeneration.h, ffx_api_dx12.h,
// ffx_api_framegeneration_dx12.h of FidelityFX SDK 2.x).
const CREATE_BACKEND_DX12: u64 = 0x0000_0002;
const CREATE_FRAMEGENERATION: u64 = 0x0002_0001;
const CONFIGURE_FRAMEGENERATION: u64 = 0x0002_0002;
const DISPATCH_PREPARE_V2: u64 = 0x0002_000c;
const CREATE_FRAMEGENERATION_VERSION: u64 = 0x0002_000e;
/// `ffxCreateContextDescFrameGenerationHudless` (a header and one format, laid
/// out as [`Version`]).
const CREATE_FRAMEGENERATION_HUDLESS: u64 = 0x0002_0009;
const CREATE_SWAPCHAIN_FOR_HWND_DX12: u64 = 0x0003_0006;
const CREATE_SWAPCHAIN_VERSION_DX12: u64 = 0x0003_000b;
const DISPATCH_WAIT_FOR_PRESENTS_DX12: u64 = 0x0003_0007;
/// The versions our structures are those of.
const FRAMEGENERATION_VERSION: u32 = (4 << 22) | 2;
const SWAPCHAIN_VERSION: u32 = 4 << 22;

// Frame generation context creation flags.
const FG_DISPLAY_RESOLUTION_MV: u32 = 1 << 1;
const FG_MV_JITTER_CANCELLATION: u32 = 1 << 2;
const FG_DEPTH_INVERTED: u32 = 1 << 3;
const FG_DEPTH_INFINITE: u32 = 1 << 4;
const FG_HIGH_DYNAMIC_RANGE: u32 = 1 << 5;
/// Dispatch flag: draws the tear and pacing lines (`VICTORY_FSR_FRAMEGEN_DEBUG=1`).
const FG_DRAW_DEBUG_TEAR_LINES: u32 = 1 << 0;
const FG_DRAW_DEBUG_PACING_LINES: u32 = 1 << 4;

// Resource states and types.
const STATE_COMMON: u32 = 1 << 0;
const STATE_UNORDERED_ACCESS: u32 = 1 << 1;
const STATE_COMPUTE_READ: u32 = 1 << 2;
const STATE_PIXEL_READ: u32 = 1 << 3;
const STATE_COPY_SRC: u32 = 1 << 4;
const STATE_COPY_DEST: u32 = 1 << 5;
const STATE_RENDER_TARGET: u32 = 1 << 8;
const STATE_DEPTH_ATTACHMENT: u32 = 1 << 9;
const TYPE_TEXTURE2D: u32 = 2;
const USAGE_UAV: u32 = 1 << 1;
const USAGE_DEPTHTARGET: u32 = 1 << 2;
const USAGE_STENCILTARGET: u32 = 1 << 5;

const OK: u32 = 0;

#[repr(C)]
struct Header {
    kind: u64,
    next: *mut Header,
}

impl Header {
    fn new(kind: u64) -> Self {
        Header { kind, next: std::ptr::null_mut() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Dims {
    width: u32,
    height: u32,
}

impl From<[u32; 2]> for Dims {
    fn from([width, height]: [u32; 2]) -> Self {
        Dims { width, height }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Floats {
    x: f32,
    y: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ResourceDescription {
    kind: u32,
    format: u32,
    width: u32,
    height: u32,
    depth: u32,
    mip_count: u32,
    flags: u32,
    usage: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Resource {
    resource: *mut c_void,
    description: ResourceDescription,
    state: u32,
}

impl Resource {
    const NONE: Resource =
        Resource { resource: std::ptr::null_mut(), description: ResourceDescription { kind: 0, format: 0, width: 0, height: 0, depth: 0, mip_count: 0, flags: 0, usage: 0 }, state: STATE_COMMON };

    /// `ffxApiGetResourceDX12` of a 2D texture in D3D12 state `state`.
    fn texture(texture: &ID3D12Resource, state: u32) -> Resource {
        // SAFETY: a live resource.
        let desc = unsafe { texture.GetDesc() };
        let usage = match desc.Format {
            DXGI_FORMAT_D16_UNORM | DXGI_FORMAT_D32_FLOAT => USAGE_DEPTHTARGET,
            DXGI_FORMAT_D24_UNORM_S8_UINT | DXGI_FORMAT_D32_FLOAT_S8X24_UINT => USAGE_DEPTHTARGET | USAGE_STENCILTARGET,
            _ => 0,
        } | if desc.Flags.contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS) { USAGE_UAV } else { 0 };
        Resource {
            resource: texture.as_raw(),
            description: ResourceDescription {
                kind: TYPE_TEXTURE2D,
                format: surface_format(desc.Format),
                width: desc.Width as u32,
                height: desc.Height,
                depth: u32::from(desc.DepthOrArraySize),
                mip_count: u32::from(desc.MipLevels),
                flags: 0,
                usage,
            },
            state: ffx_state(state),
        }
    }
}

#[repr(C)]
struct CreateSwapChainForHwnd {
    header: Header,
    swap_chain: *mut *mut c_void,
    hwnd: HWND,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    fullscreen: *const c_void,
    factory: *mut c_void,
    queue: *mut c_void,
}

#[repr(C)]
struct Version {
    header: Header,
    version: u32,
}

#[repr(C)]
struct CreateFrameGeneration {
    header: Header,
    flags: u32,
    display_size: Dims,
    max_render_size: Dims,
    back_buffer_format: u32,
}

#[repr(C)]
struct CreateBackend {
    header: Header,
    device: *mut c_void,
}

type FrameGenerationCallback = unsafe extern "C" fn(params: *mut Header, user: *mut c_void) -> u32;

#[repr(C)]
struct ConfigureFrameGeneration {
    header: Header,
    swap_chain: *mut c_void,
    present_callback: *const c_void,
    present_callback_user: *mut c_void,
    frame_generation_callback: Option<FrameGenerationCallback>,
    frame_generation_callback_user: *mut c_void,
    frame_generation_enabled: bool,
    allow_async_workloads: bool,
    hudless_color: Resource,
    flags: u32,
    only_present_generated: bool,
    generation_rect: Rect,
    frame_id: u64,
}

#[repr(C)]
struct PrepareV2 {
    header: Header,
    frame_id: u64,
    flags: u32,
    command_list: *mut c_void,
    render_size: Dims,
    jitter_offset: Floats,
    motion_vector_scale: Floats,
    frame_time_delta: f32,
    reset: bool,
    camera_near: f32,
    camera_far: f32,
    camera_fov_angle_vertical: f32,
    view_space_to_meters_factor: f32,
    depth: Resource,
    motion_vectors: Resource,
    camera_position: [f32; 3],
    camera_up: [f32; 3],
    camera_right: [f32; 3],
    camera_forward: [f32; 3],
}

type Context = *mut c_void;
type Create = unsafe extern "C" fn(context: *mut Context, desc: *mut Header, memory: *const c_void) -> u32;
type Destroy = unsafe extern "C" fn(context: *mut Context, memory: *const c_void) -> u32;
type Configure = unsafe extern "C" fn(context: *mut Context, desc: *const Header) -> u32;
type Dispatch = unsafe extern "C" fn(context: *mut Context, desc: *const Header) -> u32;

/// The loader's exports.
struct Ffx {
    _framegen: Library,
    _loader: Library,
    create: Create,
    destroy: Destroy,
    configure: Configure,
    dispatch: Dispatch,
}

static FFX: OnceLock<Ffx> = OnceLock::new();
/// Our swap chain is FidelityFX's frame interpolation swap chain.
static SWAP_CHAIN: AtomicBool = AtomicBool::new(false);
/// Frame generation ran on the last present.
static ON: AtomicBool = AtomicBool::new(false);
// Since the last frame-rate line: presents with frame generation on, off,
// and prepare dispatches that failed.
static PRESENTS_ON: AtomicU64 = AtomicU64::new(0);
static PRESENTS_OFF: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
/// Presents with frame generation on given the colour without the interface.
static HUDLESS_GIVEN: AtomicU64 = AtomicU64::new(0);

/// What is kept from present to present: the contexts (boxed, for their
/// addresses are handed out: the frame generation one to the swap chain's
/// callback), the sizes the frame generation context was made for, and the
/// frame ids.
struct Live {
    swap_chain_context: Box<Context>,
    /// The swap chain (not a reference of ours: the presenter holds it).
    swap_chain: *mut c_void,
    /// The context and what it was made for: display and largest render size,
    /// back buffer format, flags, and the format of the colour without the
    /// interface (unknown: none was known).
    framegen: Option<(Box<Context>, [u32; 2], [u32; 2], DXGI_FORMAT, u32, DXGI_FORMAT)>,
    frame_id: u64,
    /// The frame generation context could not be made for these sizes.
    refused: Option<([u32; 2], [u32; 2])>,
}

// SAFETY: FidelityFX contexts, used under the lock by one thread at a time.
unsafe impl Send for Live {}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);

fn live() -> std::sync::MutexGuard<'static, Option<Live>> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// `VICTORY_FSR_FRAMEGEN=1`.
pub(crate) fn asked_from_env() -> bool {
    std::env::var("VICTORY_FSR_FRAMEGEN").is_ok_and(|v| v.trim() == "1")
}

/// `VICTORY_FIDELITYFX_DIR`, or a `fidelityfx` folder beside our exe.
pub(crate) fn dir_from_env() -> PathBuf {
    match std::env::var_os("VICTORY_FIDELITYFX_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|d| d.join("fidelityfx")))
            .unwrap_or_else(|| PathBuf::from("fidelityfx")),
    }
}

/// Loads AMD's DLLs from `dir` (signatures checked first); whether they are in.
pub(crate) fn init(dir: &Path) -> bool {
    match load(dir) {
        Ok(ffx) => {
            let _ = FFX.set(ffx);
            tracing::info!(folder = %dir.display(), "FSR 3 frame generation: AMD's FidelityFX DLLs loaded (signatures valid)");
            true
        }
        Err(why) => {
            tracing::warn!(folder = %dir.display(), "FSR 3 frame generation: {why}; it stays off");
            false
        }
    }
}

fn load(dir: &Path) -> Result<Ffx, String> {
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };
    let open = |name: &str| -> Result<Library, String> {
        let path = dir.join(name);
        if !path.is_file() {
            return Err(format!("{} is missing", path.display()));
        }
        super::verify_signature(&path)?;
        let file = super::wide(&path);
        // Its own folder and System32 only, never the current folder.
        // SAFETY: a NUL-terminated full path.
        let module = unsafe {
            LoadLibraryExW(file.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32)
        };
        if module.is_null() {
            return Err(format!("{name} did not load ({})", std::io::Error::last_os_error()));
        }
        Ok(Library(module))
    };
    // The effect first, by its full path: the loader then finds it in.
    let framegen = open(FRAMEGEN)?;
    let loader = open(LOADER)?;
    let module = loader.0;
    let find = |name: &CStr| {
        // SAFETY: a loaded module and a NUL-terminated name.
        unsafe { GetProcAddress(module, name.as_ptr().cast()) }
            .map(|f| f as *const c_void)
            .ok_or_else(|| format!("{LOADER} has no {}", name.to_string_lossy()))
    };
    macro_rules! get {
        ($name:literal, $t:ty) => {
            // SAFETY: the export of that name has the header's signature.
            unsafe { std::mem::transmute::<*const c_void, $t>(find($name)?) }
        };
    }
    Ok(Ffx {
        create: get!(c"ffxCreateContext", Create),
        destroy: get!(c"ffxDestroyContext", Destroy),
        configure: get!(c"ffxConfigure", Configure),
        dispatch: get!(c"ffxDispatch", Dispatch),
        _loader: loader,
        _framegen: framegen,
    })
}

/// Whether the title's upscalings are to be noted for us: the DLLs are in and
/// DLSS Frame Generation does not have the presents.
pub(crate) fn noting() -> bool {
    FFX.get().is_some() && !super::FG_READY.load(Acquire)
}

/// Whether the presents go through FidelityFX's swap chain (frame generation
/// can be switched while the title runs).
pub(crate) fn live_swap_chain() -> bool {
    SWAP_CHAIN.load(Acquire)
}

/// Frames shown for each one rendered: 2 while frame generation runs.
pub(crate) fn shown_per_frame() -> Option<u32> {
    (SWAP_CHAIN.load(Relaxed) && ON.load(Relaxed)).then_some(2)
}

/// Our swap chain, made by FidelityFX for `hwnd` on `queue` (the frame
/// interpolation swap chain); `None` to make it as before (FSR 3 frame
/// generation not asked or not loaded, DLSS Frame Generation has it, or it
/// failed: then logged).
pub(crate) fn swap_chain(
    factory: &IDXGIFactory2,
    queue: &ID3D12CommandQueue,
    hwnd: HWND,
    desc: &DXGI_SWAP_CHAIN_DESC1,
) -> Option<IDXGISwapChain3> {
    let ffx = FFX.get()?;
    if super::FG_READY.load(Acquire) {
        tracing::info!("FSR 3 frame generation stays off: DLSS Frame Generation runs on this card");
        return None;
    }
    let mut made: *mut c_void = std::ptr::null_mut();
    let mut version = Version { header: Header::new(CREATE_SWAPCHAIN_VERSION_DX12), version: SWAPCHAIN_VERSION };
    let mut create = CreateSwapChainForHwnd {
        header: Header::new(CREATE_SWAPCHAIN_FOR_HWND_DX12),
        swap_chain: &mut made,
        hwnd,
        desc,
        fullscreen: std::ptr::null(),
        factory: factory.as_raw(),
        queue: queue.as_raw(),
    };
    create.header.next = (&raw mut version).cast();
    let mut context: Box<Context> = Box::new(std::ptr::null_mut());
    // SAFETY: filled descriptions that live through the call.
    let code = unsafe { (ffx.create)(&mut *context, (&raw mut create).cast(), std::ptr::null()) };
    if code != OK || made.is_null() {
        tracing::warn!(code, "FSR 3 frame generation: FidelityFX did not make the swap chain; made as before, frame generation off");
        return None;
    }
    // SAFETY: the swap chain FidelityFX made, with the reference it gave us.
    let swap_chain = unsafe { IDXGISwapChain4::from_raw(made) };
    let swap_chain = match swap_chain.cast::<IDXGISwapChain3>() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "FSR 3 frame generation: the swap chain has no IDXGISwapChain3");
            return None;
        }
    };
    *live() = Some(Live { swap_chain_context: context, swap_chain: swap_chain.as_raw(), framegen: None, frame_id: 0, refused: None });
    SWAP_CHAIN.store(true, Release);
    // The colour without the interface is copied in front of the title's interface composite, as for DLSS
    // Frame Generation (`VICTORY_FRAMEGEN_HUDLESS=0`: not).
    let hudless = std::env::var("VICTORY_FRAMEGEN_HUDLESS").map_or(true, |v| v.trim() != "0");
    super::HUDLESS_WANTED.store(hudless, Release);
    tracing::info!(
        width = desc.Width,
        height = desc.Height,
        format = desc.Format.0,
        "FSR 3 frame generation: our swap chain is FidelityFX's frame interpolation swap chain"
    );
    Some(swap_chain)
}

/// The swap chain's buffers are about to be made again (everything sent has
/// run): the frame generation context goes, and is made again for the new
/// size at the next present with an upscaling.
pub(crate) fn resizing() {
    let Some(ffx) = FFX.get() else { return };
    let mut live = live();
    if let Some(live) = live.as_mut()
        && retire(ffx, live)
    {
        tracing::info!("FSR 3 frame generation: its context goes with the swap chain's buffers");
    }
}

/// The frame generation context goes: the swap chain is told to stop calling
/// it and its presents in flight are waited for first. Whether there was one.
fn retire(ffx: &Ffx, live: &mut Live) -> bool {
    let Some((mut context, ..)) = live.framegen.take() else { return false };
    live.frame_id += 1;
    let mut off = ConfigureFrameGeneration {
        header: Header::new(CONFIGURE_FRAMEGENERATION),
        swap_chain: live.swap_chain,
        present_callback: std::ptr::null(),
        present_callback_user: std::ptr::null_mut(),
        frame_generation_callback: None,
        frame_generation_callback_user: std::ptr::null_mut(),
        frame_generation_enabled: false,
        allow_async_workloads: false,
        hudless_color: Resource::NONE,
        flags: 0,
        only_present_generated: false,
        generation_rect: Rect::default(),
        frame_id: live.frame_id,
    };
    let mut wait = Header::new(DISPATCH_WAIT_FOR_PRESENTS_DX12);
    // SAFETY: our contexts and filled descriptions; nothing of ours uses the
    // frame generation context after this.
    unsafe {
        (ffx.configure)(&mut *context, (&raw mut off).cast());
        (ffx.dispatch)(&mut *live.swap_chain_context, &raw mut wait);
        (ffx.destroy)(&mut *context, std::ptr::null());
    }
    live.refused = None;
    ON.store(false, Relaxed);
    true
}

unsafe extern "C" fn generate(params: *mut Header, user: *mut c_void) -> u32 {
    let Some(ffx) = FFX.get() else { return 1 };
    // SAFETY: `user` is the boxed frame generation context the configuration
    // named; `params` the swap chain's dispatch description.
    unsafe { (ffx.dispatch)(user.cast(), params) }
}

/// Into `list` (the last list before our Present, on the same queue): with a
/// fresh upscaling, the frame's prepare dispatch from its depth and motion
/// vectors, and the frame's configuration (frame generation on or off), for
/// the back buffer of `size` and `format`.
///
/// `base` is the texture presented and its state; `two_planes`: the interface
/// is a plane of its own composed over it (Series S), so `base` is the colour
/// without the interface. Otherwise (Series X) that is the copy taken in front
/// of the title's interface composite. FidelityFX tells the interface (and our
/// panel and menu, drawn after) from the scene by it and does not move it with
/// the scene.
pub(crate) fn before_present(
    device: &ID3D12Device,
    swap_chain: &IDXGISwapChain3,
    list: &ID3D12GraphicsCommandList,
    size: [u32; 2],
    format: DXGI_FORMAT,
    base: (&ID3D12Resource, u32),
    two_planes: bool,
) {
    if !SWAP_CHAIN.load(Acquire) {
        return;
    }
    let Some(ffx) = FFX.get() else { return };
    // (Taken at every present: the copies of a texture not presented go.)
    let hudless: Option<(ID3D12Resource, u32)> = if two_planes {
        Some((base.0.clone(), base.1))
    } else if super::hudless_wanted() {
        super::hudless_for(base.0).map(|copy| (copy, D3D12_RESOURCE_STATE_COPY_DEST.0 as u32))
    } else {
        None
    };
    // Of the back buffer's size only (FidelityFX compares the two pixel by pixel).
    let hudless = hudless.filter(|(t, _)| {
        // SAFETY: a live texture.
        let desc = unsafe { t.GetDesc() };
        let same = [desc.Width as u32, desc.Height] == size;
        if !same {
            once!(warn, hudless = ?[desc.Width as u32, desc.Height], back_buffer = ?size, "FSR 3 frame generation: the colour without the interface is not the back buffer's size; the interface moves with the scene");
        }
        same
    });
    let hudless_format = hudless.as_ref().map_or(DXGI_FORMAT_UNKNOWN, |(t, _)| {
        // SAFETY: a live texture.
        unsafe { t.GetDesc() }.Format
    });
    let mut guard = live();
    let Some(live) = guard.as_mut() else { return };
    // The textures' states are those the submissions before this present
    // left: only the line of submissions orders the two.
    let line = crate::queue::line_on();
    if !line {
        once!(warn, "FSR 3 frame generation: the line of submissions did not start; frame generation stays off");
    }
    let wanted = super::framegen_wanted() != 0 && line;
    let fresh = if wanted { super::fresh_inputs() } else { None };
    // The context, made at the first upscaling (its sizes are the title's)
    // and again when they change.
    if let Some(inputs) = &fresh {
        let render = inputs.camera.render;
        let made_for = live.framegen.as_ref().map(|(_, s, r, f, flags, h)| (*s, *r, *f, *flags, *h));
        let flags = create_flags(&inputs.camera, format);
        // (A frame without its copy keeps the context as it is.)
        let stale = made_for.is_some_and(|(s, r, f, fl, h)| {
            s != size || r[0] < render[0] || r[1] < render[1] || f != format || fl != flags || (hudless_format != DXGI_FORMAT_UNKNOWN && h != hudless_format)
        });
        if stale {
            // (Everything sent so far is waited for: what was recorded with
            // the old context has run.)
            crate::host::settle_now(crate::host::WHY_SYNC);
            retire(ffx, live);
            tracing::info!(display = ?size, max_render = ?render, "FSR 3 frame generation: its context is made again for other sizes");
        }
        if live.framegen.is_none() && live.refused != Some((size, render)) {
            match create_context(ffx, device, size, render, format, flags, hudless_format) {
                Some(context) => {
                    tracing::info!(
                        display = ?size,
                        max_render = ?render,
                        format = format.0,
                        hudless_format = hudless_format.0,
                        flags = format_args!("{flags:#x}"),
                        "FSR 3 frame generation: its context was made"
                    );
                    live.framegen = Some((context, size, render, format, flags, hudless_format));
                    live.refused = None;
                }
                None => live.refused = Some((size, render)),
            }
        }
    }
    let Some((context, ..)) = live.framegen.as_mut() else {
        PRESENTS_OFF.fetch_add(1, Relaxed);
        return;
    };
    live.frame_id += 1;
    let frame_id = live.frame_id;
    let debug = debug_flags();
    let mut prepared = false;
    if let Some(inputs) = &fresh {
        let camera = inputs.camera;
        let depth = Resource::texture(&inputs.depth.0, inputs.depth.1);
        let motion = Resource::texture(&inputs.motion.0, inputs.motion.1);
        let mut prepare = PrepareV2 {
            header: Header::new(DISPATCH_PREPARE_V2),
            frame_id,
            flags: debug,
            command_list: list.as_raw(),
            render_size: camera.render.into(),
            jitter_offset: Floats { x: camera.jitter[0], y: camera.jitter[1] },
            motion_vector_scale: Floats { x: camera.motion_scale[0], y: camera.motion_scale[1] },
            frame_time_delta: inputs.frame_ms,
            reset: camera.reset,
            // As the title gives them to its upscaler (FidelityFX's terms).
            camera_near: camera.near,
            camera_far: camera.far,
            camera_fov_angle_vertical: camera.fov_y,
            view_space_to_meters_factor: 1.0,
            depth,
            motion_vectors: motion,
            camera_position: [0.0; 3],
            camera_up: [0.0, 1.0, 0.0],
            camera_right: [1.0, 0.0, 0.0],
            camera_forward: [0.0, 0.0, 1.0],
        };
        // SAFETY: our context and a filled description of live textures in
        // the states they are in when `list` runs.
        let code = unsafe { (ffx.dispatch)(&mut **context, (&raw mut prepare).cast()) };
        if code == OK {
            prepared = true;
            once!(
                info,
                render = ?camera.render,
                output = ?camera.output,
                back_buffer = ?size,
                depth_state = format_args!("{:#x}", inputs.depth.1),
                motion_state = format_args!("{:#x}", inputs.motion.1),
                near = camera.near,
                far = camera.far,
                fov = camera.fov_y,
                jitter = ?camera.jitter,
                mvec_scale = ?camera.motion_scale,
                frame_ms = inputs.frame_ms,
                "FSR 3 frame generation: first frame prepared from depth and motion vectors"
            );
        } else {
            FAILED.fetch_add(1, Relaxed);
            once!(warn, code, "FSR 3 frame generation: the prepare dispatch failed");
        }
    }
    let on = prepared && wanted;
    let mut configure = ConfigureFrameGeneration {
        header: Header::new(CONFIGURE_FRAMEGENERATION),
        swap_chain: swap_chain.as_raw(),
        present_callback: std::ptr::null(),
        present_callback_user: std::ptr::null_mut(),
        frame_generation_callback: Some(generate),
        frame_generation_callback_user: (&raw mut **context).cast(),
        frame_generation_enabled: on,
        allow_async_workloads: false,
        hudless_color: match (&hudless, on) {
            (Some((texture, state)), true) => Resource::texture(texture, *state),
            _ => Resource::NONE,
        },
        flags: debug,
        only_present_generated: false,
        generation_rect: Rect { left: 0, top: 0, width: size[0] as i32, height: size[1] as i32 },
        frame_id,
    };
    // SAFETY: our context and a filled description.
    let code = unsafe { (ffx.configure)(&mut **context, (&raw mut configure).cast()) };
    if code != OK {
        once!(warn, code, "FSR 3 frame generation: the configuration failed");
    }
    let on = on && code == OK;
    if ON.swap(on, Relaxed) != on {
        tracing::info!(on, frame_id, "FSR 3 frame generation switched");
    }
    if on {
        PRESENTS_ON.fetch_add(1, Relaxed);
        if hudless.is_some() {
            HUDLESS_GIVEN.fetch_add(1, Relaxed);
            once!(info, two_planes, "FSR 3 frame generation: first frame with the colour without the interface");
        }
    } else {
        PRESENTS_OFF.fetch_add(1, Relaxed);
    }
}

fn create_context(
    ffx: &Ffx,
    device: &ID3D12Device,
    size: [u32; 2],
    render: [u32; 2],
    format: DXGI_FORMAT,
    flags: u32,
    hudless_format: DXGI_FORMAT,
) -> Option<Box<Context>> {
    let mut backend = CreateBackend { header: Header::new(CREATE_BACKEND_DX12), device: device.as_raw() };
    // The colour without the interface in a format of its own (else the back buffer's is assumed).
    let mut hudless = Version { header: Header::new(CREATE_FRAMEGENERATION_HUDLESS), version: surface_format(hudless_format) };
    if hudless_format != DXGI_FORMAT_UNKNOWN && hudless_format != format {
        hudless.header.next = (&raw mut backend).cast();
    }
    let first_after_version: *mut Header =
        if hudless_format != DXGI_FORMAT_UNKNOWN && hudless_format != format { (&raw mut hudless).cast() } else { (&raw mut backend).cast() };
    let mut version = Version { header: Header::new(CREATE_FRAMEGENERATION_VERSION), version: FRAMEGENERATION_VERSION };
    let mut create = CreateFrameGeneration {
        header: Header::new(CREATE_FRAMEGENERATION),
        flags,
        display_size: size.into(),
        max_render_size: render.into(),
        back_buffer_format: surface_format(format),
    };
    version.header.next = first_after_version;
    create.header.next = (&raw mut version).cast();
    let mut context: Box<Context> = Box::new(std::ptr::null_mut());
    // SAFETY: filled descriptions that live through the call.
    let code = unsafe { (ffx.create)(&mut *context, (&raw mut create).cast(), std::ptr::null()) };
    if code != OK || context.is_null() {
        tracing::warn!(code, display = ?size, max_render = ?render, format = format.0, "FSR 3 frame generation: its context was not made; presents stay plain");
        return None;
    }
    Some(context)
}

/// The frame generation context's flags from the title's upscaler's (the
/// same meanings, other bits) and our back buffer.
fn create_flags(camera: &plan::Camera, format: DXGI_FORMAT) -> u32 {
    let mut flags = 0;
    if camera.flags & plan::FFX_DISPLAY_RESOLUTION_MOTION_VECTORS != 0 {
        flags |= FG_DISPLAY_RESOLUTION_MV;
    }
    if camera.flags & plan::FFX_MOTION_VECTORS_JITTER_CANCELLATION != 0 {
        flags |= FG_MV_JITTER_CANCELLATION;
    }
    if camera.flags & plan::FFX_DEPTH_INVERTED != 0 {
        flags |= FG_DEPTH_INVERTED;
    }
    if camera.flags & plan::FFX_DEPTH_INFINITE != 0 {
        flags |= FG_DEPTH_INFINITE;
    }
    // (The title's upscaler takes the scene before its tone mapping; what we
    // present is after it: high dynamic range only for a float back buffer.)
    if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
        flags |= FG_HIGH_DYNAMIC_RANGE;
    }
    flags
}

/// `VICTORY_FSR_FRAMEGEN_DEBUG=1`: tear and pacing lines drawn.
fn debug_flags() -> u32 {
    static FLAGS: OnceLock<u32> = OnceLock::new();
    *FLAGS.get_or_init(|| {
        if std::env::var("VICTORY_FSR_FRAMEGEN_DEBUG").is_ok_and(|v| v.trim() == "1") {
            FG_DRAW_DEBUG_TEAR_LINES | FG_DRAW_DEBUG_PACING_LINES
        } else {
            0
        }
    })
}

/// A D3D12 state as FidelityFX names it.
fn ffx_state(state: u32) -> u32 {
    let s = D3D12_RESOURCE_STATES(state as i32);
    if s == D3D12_RESOURCE_STATE_COMMON {
        return STATE_COMMON;
    }
    let mut out = 0;
    if s.contains(D3D12_RESOURCE_STATE_UNORDERED_ACCESS) {
        out |= STATE_UNORDERED_ACCESS;
    }
    if s.contains(D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE) {
        out |= STATE_COMPUTE_READ;
    }
    if s.contains(D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE) {
        out |= STATE_PIXEL_READ;
    }
    if s.contains(D3D12_RESOURCE_STATE_COPY_SOURCE) {
        out |= STATE_COPY_SRC;
    }
    if s.contains(D3D12_RESOURCE_STATE_COPY_DEST) {
        out |= STATE_COPY_DEST;
    }
    if s.contains(D3D12_RESOURCE_STATE_RENDER_TARGET) {
        out |= STATE_RENDER_TARGET;
    }
    if s.contains(D3D12_RESOURCE_STATE_DEPTH_WRITE) || s.contains(D3D12_RESOURCE_STATE_DEPTH_READ) {
        out |= STATE_DEPTH_ATTACHMENT;
    }
    if out == 0 { STATE_COMMON } else { out }
}

/// `ffxApiGetSurfaceFormatDX12`.
fn surface_format(format: DXGI_FORMAT) -> u32 {
    match format {
        DXGI_FORMAT_R32G32B32A32_TYPELESS => 1,
        DXGI_FORMAT_R32G32B32A32_UINT => 2,
        DXGI_FORMAT_R32G32B32A32_FLOAT => 3,
        DXGI_FORMAT_R16G16B16A16_FLOAT => 4,
        DXGI_FORMAT_R32G32_FLOAT => 6,
        DXGI_FORMAT_R8_UINT | DXGI_FORMAT_X32_TYPELESS_G8X24_UINT | DXGI_FORMAT_X24_TYPELESS_G8_UINT => 7,
        DXGI_FORMAT_R32_UINT | DXGI_FORMAT_R24G8_TYPELESS | DXGI_FORMAT_D24_UNORM_S8_UINT | DXGI_FORMAT_R24_UNORM_X8_TYPELESS => 8,
        DXGI_FORMAT_R8G8B8A8_TYPELESS => 9,
        DXGI_FORMAT_R8G8B8A8_UNORM => 10,
        DXGI_FORMAT_R8G8B8A8_SNORM => 11,
        DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => 12,
        DXGI_FORMAT_B8G8R8A8_TYPELESS => 13,
        DXGI_FORMAT_B8G8R8A8_UNORM => 14,
        DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => 15,
        DXGI_FORMAT_R11G11B10_FLOAT => 16,
        DXGI_FORMAT_R10G10B10A2_UNORM => 17,
        DXGI_FORMAT_R16G16_FLOAT => 18,
        DXGI_FORMAT_R16G16_UINT => 19,
        DXGI_FORMAT_R16_FLOAT => 21,
        DXGI_FORMAT_R16_UINT => 22,
        DXGI_FORMAT_R16_UNORM | DXGI_FORMAT_D16_UNORM => 23,
        DXGI_FORMAT_R16_SNORM => 24,
        DXGI_FORMAT_R8_UNORM | DXGI_FORMAT_A8_UNORM => 25,
        DXGI_FORMAT_R8G8_UNORM => 26,
        DXGI_FORMAT_R8G8_UINT => 27,
        DXGI_FORMAT_R32_FLOAT | DXGI_FORMAT_D32_FLOAT | DXGI_FORMAT_R32G8X24_TYPELESS | DXGI_FORMAT_D32_FLOAT_S8X24_UINT | DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS => 28,
        DXGI_FORMAT_R9G9B9E5_SHAREDEXP => 29,
        DXGI_FORMAT_R16G16B16A16_TYPELESS => 30,
        DXGI_FORMAT_R32G32_TYPELESS => 31,
        DXGI_FORMAT_R10G10B10A2_TYPELESS => 32,
        DXGI_FORMAT_R16G16_TYPELESS => 33,
        DXGI_FORMAT_R16_TYPELESS => 34,
        DXGI_FORMAT_R8_TYPELESS => 35,
        DXGI_FORMAT_R8G8_TYPELESS => 36,
        DXGI_FORMAT_R32_TYPELESS => 37,
        DXGI_FORMAT_R8_SNORM => 39,
        _ => 0,
    }
}

/// For the frame-rate line: presents with FSR 3 frame generation on and off
/// since the last one, and prepare dispatches that failed; `None` if it is
/// not in use.
pub(crate) fn line_counts() -> Option<(u64, u64, u64, u64)> {
    // (Every tenth frame-rate line: about every ten seconds.)
    static LINES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let due = LINES.fetch_add(1, Relaxed) % 10 == 9;
    (SWAP_CHAIN.load(Relaxed) && due).then(|| {
        (PRESENTS_ON.swap(0, Relaxed), PRESENTS_OFF.swap(0, Relaxed), FAILED.swap(0, Relaxed), HUDLESS_GIVEN.swap(0, Relaxed))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_the_headers() {
        // x64 sizes of the C structures (FidelityFX SDK 2.x headers).
        assert_eq!(size_of::<Header>(), 16);
        assert_eq!(size_of::<ResourceDescription>(), 32);
        assert_eq!(size_of::<Resource>(), 48);
        assert_eq!(size_of::<CreateFrameGeneration>(), 40);
        assert_eq!(size_of::<CreateSwapChainForHwnd>(), 64);
        assert_eq!(std::mem::offset_of!(ConfigureFrameGeneration, hudless_color), 64);
        assert_eq!(std::mem::offset_of!(ConfigureFrameGeneration, generation_rect), 120);
        assert_eq!(std::mem::offset_of!(ConfigureFrameGeneration, frame_id), 136);
        assert_eq!(size_of::<ConfigureFrameGeneration>(), 144);
        assert_eq!(std::mem::offset_of!(PrepareV2, command_list), 32);
        assert_eq!(std::mem::offset_of!(PrepareV2, frame_time_delta), 64);
        assert_eq!(std::mem::offset_of!(PrepareV2, camera_near), 72);
        assert_eq!(std::mem::offset_of!(PrepareV2, depth), 88);
        assert_eq!(std::mem::offset_of!(PrepareV2, camera_position), 184);
        assert_eq!(size_of::<PrepareV2>(), 232);
    }

    fn facts(dlss: bool, dlss_fg_supported: bool, fsr_loaded: bool, series_s: bool) -> super::super::Facts {
        super::super::Facts {
            adapter: "NVIDIA GeForce RTX 3080".into(),
            series_s,
            no_dlss: if dlss { None } else { Some(super::super::NoDlss::Card) },
            files_missing: false,
            dlss_fg_asked: true,
            dlss_fg_supported,
            fsr_asked: true,
            fsr_loaded,
            fsr_needs_series_s: !series_s && !dlss,
        }
    }

    /// The start's notice: what runs and why, in lines the picture's box and font show.
    #[test]
    fn the_notice_says_what_the_card_does() {
        use super::super::notice_of;
        let shown = |lines: &[String]| {
            for l in lines {
                assert!(l.len() <= 52, "{l}");
                assert!(l.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || b" .,:()/->+!?_=*".contains(&c)), "{l}");
            }
        };
        let lines = |f, dlss_live, fsr_live| {
            let l = notice_of(&f, dlss_live, fsr_live).into_iter().map(|l| l.to_ascii_uppercase()).collect::<Vec<_>>();
            shown(&l);
            l.join("|")
        };
        // RTX 40/50: DLSS frame generation.
        let mut rtx40 = facts(true, true, true, false);
        rtx40.adapter = "NVIDIA GeForce RTX 4070".into();
        let rtx40 = lines(rtx40, true, false);
        assert!(rtx40.contains("FRAME GEN: DLSS, SUPPORTED") && !rtx40.contains("MOD"), "{rtx40}");
        // RTX 20/30 with DLSS frame generation running: a mod unlocked it (MFG Unlocker).
        let unlocked = lines(facts(true, true, true, false), true, false);
        assert!(unlocked.contains("UNLOCKED BY A MOD") && !unlocked.contains("SUPPORTED HERE"), "{unlocked}");
        // RTX 20/30 on Series X with DLSS: FSR 3 instead, 2X.
        let rtx30 = lines(facts(true, false, true, false), false, true);
        assert!(rtx30.contains("NOT ON THIS CARD") && rtx30.contains("FSR 3") && rtx30.contains("2X"));
        // No DLSS at all, Series X: FSR 3 needs Series S.
        let amd_x = lines(facts(false, false, false, false), false, false);
        assert!(amd_x.contains("HAS NO DLSS") && amd_x.contains("SERIES S"), "{amd_x}");
        // A card with DLSS, only turned off: say so, not "this card".
        let mut off = facts(false, true, false, false);
        off.no_dlss = Some(super::super::NoDlss::TurnedOff);
        let off = lines(off, false, false);
        assert!(off.contains("DLSS UPSCALING IS OFF") && !off.contains("THIS CARD"), "{off}");
        let mut missing = facts(false, false, false, false);
        missing.no_dlss = Some(super::super::NoDlss::FilesMissing);
        missing.files_missing = true;
        let missing = lines(missing, false, false);
        assert!(missing.contains("FILES NOT FOUND") && !missing.contains("THIS CARD"), "{missing}");
        // No DLSS, Series S: FSR 3 runs.
        assert!(lines(facts(false, false, true, true), false, true).contains("RUNS WITHOUT DLSS"));
        // Nothing asked: nothing said.
        let mut quiet = facts(true, true, true, false);
        (quiet.dlss_fg_asked, quiet.fsr_asked) = (false, false);
        assert!(notice_of(&quiet, false, false).is_empty());
    }

    #[test]
    fn states_and_formats() {
        assert_eq!(ffx_state(0), STATE_COMMON);
        assert_eq!(ffx_state(0x8), STATE_UNORDERED_ACCESS);
        assert_eq!(ffx_state(0xC0), STATE_COMPUTE_READ | STATE_PIXEL_READ);
        assert_eq!(ffx_state(0x20), STATE_DEPTH_ATTACHMENT);
        assert_eq!(surface_format(DXGI_FORMAT_R8G8B8A8_UNORM), 10);
        assert_eq!(surface_format(DXGI_FORMAT_R10G10B10A2_UNORM), 17);
        assert_eq!(surface_format(DXGI_FORMAT_R32_TYPELESS), 37);
    }
}
