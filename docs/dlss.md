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
     motion vectors; it reports how many frames frame generation showed.

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
(premultiplied, as our composer blends them). On Series X the interface is
drawn into the scene's output: no interface-free colour is tagged yet.

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

## Limits

- Motion vectors: blocks of wrong motion inside character silhouettes (likely
  the heat-haze pass `731a39171b568298`) show as smearing with DLSS and as
  warped shapes in generated frames.
- No interface-free colour on Series X: generated frames warp the interface
  with the scene.
- Camera matrices are made, not the title's (see above).
- Video memory: the title's FidelityFX context stays allocated beside DLSS's;
  frame generation needs about 0.5 GB more at 1440p.
- Latency: frame generation holds a frame back; it is meant for a base frame
  rate of 60 and up.
- `slShutdown` is not called; the process ends with the title.

Credits: the FidelityFX description layouts and the image patch of the
FidelityFX path are DexNom's (DexNom/VIctory, GPL-3.0-or-later).
