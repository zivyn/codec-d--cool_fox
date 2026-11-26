# DLSS and DLSS Frame Generation (NVIDIA Streamline)

Two switches, both off by default:

- `VICTORY_DLSS=1`: DLSS Super Resolution does the title's FidelityFX upscaling
  (its `ffxDispatch`), from the same colour, depth, motion vectors, exposure and
  reactive mask into the same output, at the same place of the title's command
  list.
- `VICTORY_FRAMEGEN=1`: DLSS Frame Generation (2x) on our present, from the depth
  and motion vectors of the last upscaling submitted before the present.

Nothing NVIDIA ships is in this repository. The library is loaded at run time
from a folder you name; if anything is missing (the folder, a signature, a
feature on this GPU, the device hand-over), the log says what, and the title's
own upscaling and our plain present run as before.

## Setup

1. Download NVIDIA's Streamline SDK release (NVIDIA RTX SDK license) from
   <https://github.com/NVIDIA-RTX/Streamline/releases> and unpack it outside the
   repository. Tested: `streamline-sdk-v2.14.16.zip`, unpacked to
   `C:\dev\sdk\streamline-sdk-v2.14.16`. Its `bin\x64` folder holds what is
   loaded:
   - `sl.interposer.dll` (loaded by us; its Authenticode signature is checked),
   - `sl.common.dll`, `sl.dlss.dll`, `sl.dlss_g.dll`, `sl.reflex.dll`,
     `sl.pcl.dll` (Streamline's plugins; it checks their NVIDIA signatures),
   - `nvngx_dlss.dll` (DLSS Super Resolution, 310.x), `nvngx_dlssg.dll` (DLSS
     Frame Generation).
   A newer `nvngx_dlss.dll` (the DLSS SDK, <https://github.com/NVIDIA/DLSS>,
   `lib\Windows_x86_64\rel`) can replace the one in that folder.
2. `VICTORY_STREAMLINE_DIR=<that bin\x64 folder>`; the default is
   `C:\dev\sdk\streamline-sdk-v2.14.16\bin\x64`.
3. Check the PC without the title (a few seconds):
   - `victory upscaler-check`: Streamline loads, and what this GPU supports.
   - `victory upscaler-check --device`: the host device is made and handed over
     to Streamline, DLSS's render sizes for the output are listed, and DLSS runs
     three times on stand-in textures.
   - `victory upscaler-check --present`: as `--device`, then three seconds of
     presents in a window of ours with frame generation on stand-in depth and
     motion vectors and a stand-in colour without the interface; it reports how
     many frames frame generation showed and how many presents were tagged
     with that colour. With a device, the check ends by switching frame
     generation off, letting the swap chain go and calling `slShutdown`
     (`VICTORY_CHECK_TEARDOWN=0`: not; the process then fails fast at its
     exit, code 0xC0000409, which some shells show as 9).
   - `VICTORY_STREAMLINE_PRESENT=1` with `--present`: DLSS alone, 120
     upscalings with a present after each through Streamline's proxy of our
     swap chain (no "presentCommon() was not observed" may follow).

Windows 10 2004 or later, a GeForce RTX GPU (frame generation: RTX 40 or
later), hardware-accelerated GPU scheduling on (frame generation needs it).

## Switches

| Variable | Values | Default | What |
|---|---|---|---|
| `VICTORY_DLSS` | `1` | off | DLSS in place of the title's FidelityFX upscaling |
| `VICTORY_FRAMEGEN` | `1` | off | DLSS Frame Generation on our present; needs `VICTORY_THIN=1` or `VICTORY_ASYNC_SUBMIT=1` (the line of submissions) |
| `VICTORY_DLSS_MODE` | `dlaa`, `quality`, `balanced`, `performance`, `ultraperformance` | `quality` (`dlaa` with frame generation alone) | the scene's size: 1, 2/3, 0.58, 1/2, 1/3 of the output; the DLSS mode follows the sizes the title then passes |
| `VICTORY_DLSS_PRESET` | `default`, `j`, `k`, `l`, `m` (`e`, `f` deprecated) | `default` | the DLSS model, for every mode |
| `VICTORY_STREAMLINE_DIR` | a folder | see above | where `sl.interposer.dll` and the rest are |
| `VICTORY_STREAMLINE_LOG` | a folder | none | Streamline's own log files there, verbose |
| `VICTORY_DLSS_JITTER` | `x,y` | `1,1` | multipliers for the jitter DLSS is given (`-1,1` flips x) |
| `VICTORY_DLSS_MV` | `x,y` | `1,1` | multipliers for the motion-vector scale |
| `VICTORY_DLSS_REACTIVE` | `0` | on | `0`: the title's reactive mask is not given to DLSS |
| `VICTORY_FSR_PATH` | `1` | off | the title's FidelityFX path at `VICTORY_DLSS_MODE`'s scale with its own FidelityFX (no Streamline): the comparison for DLSS |
| `VICTORY_FRAMEGEN_HUDLESS` | `0` | on (with `VICTORY_FRAMEGEN=1`) | `0`: no colour without the interface is copied or tagged on Series X (generated frames then warp the interface) |
| `VICTORY_STREAMLINE_PRESENT` | `1` | off | DLSS without frame generation: our swap chain is Streamline's proxy of it, so Streamline sees the presents (no "presentCommon() was not observed") |

## How it works

**Where the title upscales.** The title imports `ffxCreateContext`,
`ffxDestroyContext`, `ffxQuery` and `ffxDispatch` from
`amd_fidelityfx_loader_xbox.dll`, which loads `amd_fidelityfx_upscaler_xbox.dll`
itself. With a switch on, the executable's imports of the loader are bound to
ours (`victory_hle::modules::fidelityfx::Interpose`), which call the title's own
DLL for everything; only an upscale `ffxDispatch` (type `0x10001`) is taken. The
title's upscaler context stays the title's own, so its upscaling runs again on
any frame DLSS does not take.

**Series X does not use FidelityFX by itself.** Run logs: the upscaler DLL is
loaded on every Series S run and on no Series X run (stock or 1440p). The
renderer selection at title+0x7e9421 picks renderer 7 (the FidelityFX path)
only when the console is a Series S (title+0x75140 tests the device type for
6, Lockhart) and a capability bit is set; Series X takes renderer 6, the
title's own temporal pass. So with DLSS or frame generation available, the app
patches the image (`native_1440p::prepare_upscaler`, from DexNom's
`prepare_fsr_4k`): renderer 7 always, and the FidelityFX path's render size
code at title+0x28ef69a notes the FidelityFX quality mode of `VICTORY_DLSS_MODE`
and takes its scaled branch, whose scale is the mode's. The title computes the
render size itself there (scale times the output size, truncated); it does not
ask `ffxQuery` for it, so a query answer cannot change it. The temporal scene
scale (title+0x28f1e7e) is set to the same scale, as the stock Series X image
pairs them (2/3 each). Every patched instruction is checked against the stock
bytes first (the test `patches_the_upscaler_path` does so with the retail
executable). Image caches: `images\native-1440p-upscaler-<mode>-sdr` (with
`VICTORY_NATIVE_1440P=1`), `images\upscaler-<mode>` (otherwise).

**At the dispatch.** On the title's thread the dispatch's textures are checked
(host copies, states a shader can read or write); then a step is put into the
title's list (`crate::list::external`): run at once, or with `VICTORY_THIN=1`
written into the list's stream and run where it is translated, in order. The
step prepares the inputs as a compute shader's would be prepared (aliased
views, uploads of title-written textures, depth twins), moves each into the
state the title gave FidelityFX, records `slEvaluateFeature` into the host list
with the textures as local tags in those states (Streamline moves them to what
it needs and back), marks the output as written on the host, and has the
title's draw state set again before its next draw. Viewport 1, frame index =
the upscaling's number.

Constants from the dispatch: jitter, motion-vector scale (FidelityFX's scale
over the render size), near, far, vertical field of view, reset; from the
context's flags: HDR, inverted depth, auto exposure (no exposure texture),
display-resolution motion vectors, jittered motion vectors. The title passes
no camera matrices: the projection is made of the field of view, the render
aspect and the depth range; camera motion is in the motion vectors, so the
clip-to-previous-clip matrices are the identity and the camera stands at the
origin. DLSS Super Resolution reads none of the matrices; frame generation may.

**Frame generation.** Streamline is used with manual hooking: nothing of D3D12
or DXGI is hooked globally. The host device is handed over (`slSetD3DDevice`);
the direct queue is made through Streamline's device proxy (the native queue
behind it runs everything); our swap chain is made through its factory proxy
on the queue proxy. A list that recorded an upscaling is noted; when it is
submitted, its depth and motion vectors become the latest. Each present (on
the line's thread, `VICTORY_THIN=1`) then gets a frame token of the title's
frame number, the latency markers (simulation, render submit, present), the
constants and the depth and motion-vector tags in the states the submissions
left them in (viewport 0), and frame generation is switched on; a present
without a new upscaling (menus, loads) switches it off and clears the tags.
Reflex low latency is on (frame generation needs it); `slReflexSleep` is not
called. On Series S, whose interface is a second plane, the scene plane is the
colour without the interface and the interface plane its colour and alpha
(premultiplied, as our composer blends them).

**The colour without the interface on Series X.** There the title keeps the
interface in a target of its own (3840x2160, RGBA8, with its own depth) and
blends it over the frame itself: one triangle with the shaders vertex
`0602c2384acbb3b2`, pixel `a78d088fa55aa495` (blend one, 1 - source alpha: the
interface target is premultiplied), the last draw into the texture it presents,
after the scene's composite into that texture. Every frame listing of
2026-11-26 has it once a frame (stock 3840x2160 output), and the 1440p runs
with DLSS bind the same pipeline from frame 3 on. With frame generation
(`crate::streamline::before_target_draw`, called by `draw::State::draw`): in
front of that draw, when its one target is a texture the title presented
before, the target is copied into a texture of ours (two, in turn; the back
buffer's size and format, always a copy's destination), recorded into the
title's list at that place. A list makes one copy. When the list is submitted
the copy is noted, and the present of that texture tags the first copy
submitted since its last present as `kBufferTypeHUDLessColor`. Other pipelines
that draw into a presented texture are named in the log once each: an interface
element drawn there before the composite would be in the copy, one drawn after
it is warped as before. The interface target is not tagged as the interface's
colour and alpha: its size is not the back buffer's at a 1440p output.

**The camera's planes.** The title names the far plane first (near 10000, far
0.15, inverted depth), which FidelityFX takes in either order. Frame
generation's constants get them in order (near 0.15, far 10000), and the
projection made of them; DLSS Super Resolution's are as before.

**Windowed and full screen.** With frame generation the system's Alt+Enter is
not taken for the window (`DXGI_MWA_NO_ALT_ENTER`): Streamline wants frame
generation off across such a change. When the presented size or format
changes, frame generation is switched off before the swap chain's buffers are
made again, and on again at the next present with an upscaling.

**Frame generation alone** (`VICTORY_FRAMEGEN=1` without `VICTORY_DLSS=1`)
leaves the upscaling to the title's own FidelityFX, which is made for the
Series S sizes only (1280x720 or 1920x1080 in, 1920x1080 out): at a 1440p
output its upscaler is not made (an error in the log says so) and the title
fails at its first upscaling. Use both switches there.

## Log lines

At start: `Streamline: DLSS Super Resolution is supported on this GPU`
(requirements), `Streamline loaded (manual hooking, no downloads)`, the `display`
line with `through the title's FidelityFX path`. At the device: `Streamline: DLSS
Super Resolution loaded streamline=… ngx=…` (and frame generation, Reflex),
`the direct queue was made through the device proxy`, `our swap chain is
Streamline's proxy`. At the title: `FidelityFX: the title's import goes through
ours`, `FidelityFX: the title made its upscaler` (flags, sizes), `FidelityFX:
the title's first upscale dispatch` (every field), `DLSS: upscaling recorded in
place of the title's FidelityFX` (render and output sizes, DLSS mode, optimal
size, formats; again when they change). Every second: `DLSS per frame: …
upscalings by DLSS, … failed, … left to the title's FidelityFX` and `DLSS Frame
Generation: … presents with it on, … off, … frames shown by it (status …)`.
`FidelityFX: the title's own upscaling runs: <why>` names each reason once.

Frame generation: `DLSS Frame Generation switched frame=… on=… code=…` at each
change; `DLSS Frame Generation: first frame with depth and motion vectors
tagged` (sizes, the states of the two textures, near, far, jitter,
motion-vector scale, `interface_plane`, `interface_free_copy`); `video memory
as the system counts it, right before the first present with frame generation
on` and `… 240 presents with frame generation on later`; `DLSS Frame
Generation status changed` (anything but 0 is a fault); `DLSS Frame Generation:
a present failed hresult=…`. Series X: `DLSS Frame Generation: the colour
without the interface is copied in front of the title's interface composite
(first time)` (size, format, pipeline), `DLSS Frame Generation: this pipeline
draws into the texture the title presents` (once per pipeline), and every ten
frame-rate lines `DLSS Frame Generation, colour without the interface, last ten
lines: … copies in front of the title's interface composite, … presents tagged
with one, … presents with frame generation on had none; … interface composite
draws into a presented texture (… after a copy in their list), … draws of other
pipelines into one`.

## The first run with frame generation in the title

Switches: `VICTORY_DLSS=1 VICTORY_DLSS_MODE=quality VICTORY_FRAMEGEN=1
VICTORY_THIN=1 VICTORY_NATIVE_1440P=1`, Series X (the launcher: `-FrameGen`).
Before it, once: `victory upscaler-check --present` (below, line 1).

1. Without the title: `frame generation: 180 presents, 180 with it on, 360
   frames shown by it, 180 presents tagged with a colour without the
   interface, status 0x0`, then `Streamline shut down: ok (0)`, exit code 0.
   (Fewer than 360 shown, 291 in one run of 2026-11-27, when the PC is busy:
   frame generation drops a generated frame that comes too late.)
2. It started: `Streamline loaded … dlss=true framegen=true`, `Streamline: DLSS
   Frame Generation loaded`, `Streamline: Reflex low latency on … code=0`,
   `the direct queue was made through the device proxy`, `our swap chain is
   Streamline's proxy`. `frame generation stays off` or `not running (see
   above)` anywhere: it does not run; the line before says why.
3. It switched on in the world: `DLSS Frame Generation switched … on=true
   code=0` at the first frame with an upscaling, then `first frame with depth
   and motion vectors tagged` with `near=0.15 far=10000`, `depth_inverted=true`,
   `interface_free_copy=true`.
4. Generated frames are shown: each second `DLSS Frame Generation: N presents
   with it on, 0 off, 2N frames shown by it (status 0x0)`. `frames shown`
   equal to `presents`: none was generated. Our caption and frame-rate line
   count the title's frames only; NVIDIA's overlay or PresentMon shows twice
   that. (`DLSSG_IndicatorText`, a DWORD of 2 under
   `HKLM\SOFTWARE\NVIDIA Corporation\Global\NGXCore`, makes frame generation
   write its state on the picture.)
5. No fault: no `DLSS Frame Generation status changed`, no `a present failed`,
   no line of target `streamline` at ERROR, no `presentCommon() was not
   observed`. A pink tint over the picture is frame generation's own sign of
   an invalid state.
6. Off and on: at the pause menu, a load or a video, `DLSS Frame Generation
   switched … on=false` when the title stops upscaling and `on=true` when it
   upscales again; the per-second line then counts presents `off`. If the
   pause menu keeps upscaling, frame generation stays on behind it.
7. The interface: the ten-second line has as many `copies` as `presents tagged
   with one`, about one per present with frame generation on, and `had none`
   near 0 in gameplay; the map, the subtitles and the prompts stand still and
   sharp while the camera turns. `this pipeline draws into the texture the
   title presents` should name one pipeline, the scene's composite (blend
   false); any other with `blend true` is an interface element drawn outside
   the interface target. Compare with `VICTORY_FRAMEGEN_HUDLESS=0`: the
   interface wobbles at the edges of moving things behind it.
8. Motion vectors (`VICTORY_DLSS_MV`, default `1,1`): turn the camera slowly
   and evenly past upright edges (poles, door frames) and look at a still
   interface element's neighbourhood and at the edges. Right: edges stay
   single in every frame. Wrong sign or scale: every other frame shows the
   edge doubled or pulled the wrong way (a judder at half the shown rate), and
   DLSS itself smears a trail behind moving edges. Try `-1,1` (x flipped; turn
   left and right), `1,-1` (y flipped; look up and down), then `2,2` and
   `0.5,0.5` (scale): if `1,1` is right, each of them is visibly worse. The
   `DLSS: upscaling recorded` line has `mvec_scale` (1/1706, 1/960 at Quality:
   the title's vectors are in render pixels).
9. Jitter (`VICTORY_DLSS_JITTER`, default `1,1`): stand still and look at fine
   detail (wire, lettering on signs, distant railings). Right: it settles
   within a second to a sharp, steady picture. Wrong sign: it stays soft and
   crawls. Try `-1,1`, `1,-1`, `-1,-1`; take the sharpest steady one. The
   jitter changes nothing for frame generation itself.
10. Video memory: the two `video memory … frame generation on` lines; the
   difference of `in use` is what frame generation took (NVIDIA: about 489 MB
   at 1440p). `in use` over the budget of the card's own memory, or the PC's
   shared memory growing by hundreds of MB in the memory line, means it does
   not fit: use a smaller DLSS mode.
11. A removed device: `host GPU device removed after …`, `; device removed:
   …`, `DLSS Frame Generation: a present failed hresult=0x887a0005` (removed),
   `0x887a0006` (hung) or `0x887a0007` (reset), the window gone with exit code
   0x887a0005. With `VICTORY_STREAMLINE_LOG=<folder>` Streamline's own log has
   the plugin's last words.

## Limits

- Motion vectors: blocks of wrong motion inside character silhouettes (likely
  the heat-haze pass `731a39171b568298`) show as smearing with DLSS and as
  warped shapes in generated frames.
- The colour without the interface on Series X is a copy in front of one
  known draw (the interface composite by its shaders, this build's); not yet
  seen running in the title. Interface drawn into the frame by another
  pipeline is not told apart. The interface's own colour and alpha are not
  tagged there.
- Camera matrices are made, not the title's (see above).
- "presentCommon() was not observed" (Streamline, once a run, with DLSS and no
  frame generation): our swap chain is not Streamline's then, so its common
  plugin sees no present; it frees nothing it retired (what DLSS let go of when
  the title's upscaler is destroyed stays allocated until the process ends).
  `VICTORY_STREAMLINE_PRESENT=1` gives it the presents.
- HDR output (a 16-bit float swap chain) is not supported by frame generation
  (status 4).
- Video memory: the title's FidelityFX context stays allocated beside DLSS's;
  frame generation needs about 0.5 GB more at 1440p.
- Latency: frame generation holds a frame back; it is meant for a base frame
  rate of 60 and up.
- `slShutdown` is not called in the title; the process ends with the title.
  Where the title leaves through the C runtime's exit with frame generation
  on, the process may fail fast at its very end as the check did without its
  shutdown (exit code 0xC0000409): no fault of the run before it.

Credits: the FidelityFX description layouts and the image patch of the
FidelityFX path are DexNom's (DexNom/VIctory, GPL-3.0-or-later).
