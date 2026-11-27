# foxhound

A fork of [VIctory](https://github.com/Marvisak/VIctory), a compatibility layer that runs an Xbox Series title
natively on Windows from files **you** supply. foxhound is tuned for **NVIDIA RTX** GPUs: DLSS Super Resolution and
DLSS Frame Generation in place of the title's own upscaling, the Series X profile at a native 2560x1440, and fewer
stalls at scene cuts.

> **This repository contains no game files, code or assets.** You need your own legally obtained copy of the
> title. foxhound is not affiliated with or endorsed by the title's developer or publisher, nor by NVIDIA.
> See [docs/asset-policy.md](docs/asset-policy.md).

## Where it stands

Tested on one PC: Windows 11, Ryzen 7 7800X3D, 32 GB RAM, **RTX 4070 SUPER 12 GB**, NVMe SSD; the Xbox Series
build **v1.0.2.1** of the title, Series X profile, 2560x1440 output, DLSS Quality. Frame times are the layer's
own count, in the opening scenes unless noted.

| | Frame time |
|---|---|
| Opening, gameplay | 34-37 ms (27-30 fps); our CPU work per frame sets it |
| Opening, cutscenes | median 36-38 ms, 95th percentile 49-58 ms, no second over 100 ms |
| Open water (first boat ride) | about 47 ms (21 fps); the GPU sets it there |
| With `-FrameGen` | every frame the title renders is shown twice (2x presented) |
| Memory | about 8 GB of VRAM; 32 GB of RAM is enough, 64 GB is more comfortable |

What is known to be wrong:

- **No ray tracing.** The title traces its indirect light, reflections and lamp shadows in its own compute
  shaders. They are off (`VICTORY_BVH_BUILDER=1` turns them on: about 3 fps and a broken picture today). An
  estimate stands in for indirect light and for reflections of opaque surfaces: rooms are lit, but reflections
  are blurry, and in cutscenes they can still show the sky's bright colour.
- **Left out:** the heat-haze / in-scattering pass (it smears blocks of pixels, also on water) and the decal
  heap's defragmentation (5.6 ms of GPU a frame). `VICTORY_LEAVE_OUT` names them.
- **Hitches:** some single frames of 70-190 ms in cutscenes while the title streams.
- **Frame rate:** the title runs at 30 fps. `-Fps 60` lets frames start every 1/60 s, but the layer itself costs
  about 33 ms a frame today, so it stays near 30. 60 fps is the goal.
- **The first run of every new build stutters** while the driver compiles pipelines (see *Warming up* below).

## What you need

- Windows 10 or 11 (x64), a CPU with AVX2, 32 GB of RAM or more, an SSD.
- An NVIDIA GeForce RTX GPU with a recent driver for DLSS (RTX 40 or later for Frame Generation, with
  hardware-accelerated GPU scheduling on). Any Direct3D 12 GPU with shader model 6.6 and mesh shaders runs the
  layer without DLSS.
- **Your own dump** of the title's Xbox Series build v1.0.2.1 (the folder with `game_scarlett_master.exe`).
  Other builds may start, but the image patches (1440p output, FidelityFX path, frame rate) check every byte
  they change and leave a differing build alone.
- [Rust](https://rustup.rs) (the toolchain is pinned in `rust-toolchain.toml`; rustup fetches it), and the
  **Visual Studio Build Tools** with "Desktop development with C++" (MSVC and a Windows SDK).
- For DLSS: NVIDIA's **Streamline SDK** (below). You download it yourself; nothing of NVIDIA's is in this
  repository.

## Build

```powershell
git clone https://github.com/readmet3xt-lab/foxhound
cd foxhound
cargo build --release --locked -p victory
```

The diagnostics build (frame timelines, captures, GPU timing) is optional:
`cargo build --locked --profile diag -p victory --features diagnostics`.

## Set up

1. **Point it at your copy.** Copy `victory.example.toml` to `victory.toml` (git-ignored) and set `game_dir` to
   your dump's folder. Keep your files **outside** this repository; the tools refuse otherwise.

   ```toml
   [paths]
   game_dir = 'D:\Games\MyDump'
   ```

2. **DLSS (optional, recommended on RTX).** Download the Streamline SDK release from
   <https://github.com/NVIDIA-RTX/Streamline/releases> (tested: `streamline-sdk-v2.14.16.zip`; it comes under
   NVIDIA's RTX SDK license, which you accept yourself), unpack it **outside** the repository, and point foxhound
   at its `bin\x64` folder:

   ```powershell
   [Environment]::SetEnvironmentVariable('VICTORY_STREAMLINE_DIR', 'D:\sdk\streamline-sdk-v2.14.16\bin\x64', 'User')
   ```

   Check it without the title: `target\release\victory.exe upscaler-check --device` (and `--present` for Frame
   Generation). Details: [docs/dlss.md](docs/dlss.md). Without Streamline the launcher runs without DLSS.

3. **Translate the shaders ahead** (a few minutes, once per new build of foxhound):

   ```powershell
   $env:VICTORY_HOST_WAVE = '32'   # NVIDIA runs 32-lane waves; leave it out on AMD
   target\release\victory.exe shaders
   ```

## Run

```powershell
.\scripts\play-foxhound.ps1 -Exe 'D:\Games\MyDump\game_scarlett_master.exe'
```

| Option | What |
|---|---|
| *(none)* | Series X profile, 2560x1440, DLSS Quality (if Streamline is set up), all fixes on |
| `-Dlss off` / `dlaa` / `balanced` / `performance` | the title's own upscaling, or another DLSS mode |
| `-FrameGen` | DLSS Frame Generation (about +1.4 GB of VRAM) |
| `-SeriesS` | the Series S profile: 1280x720 inside, faster, softer |
| `-Res 1080to1440` / `4k` | scene at 1080p upscaled by the title / the stock 3840x2160 output |
| `-Build diag` | the diagnostics build |
| `-More @{VICTORY_X='1'}` | add or override any switch |

Full screen: start the game, then run `.\scripts\fullscreen.ps1` in another window (borderless, the size of the
monitor the game is on).

Logs go to `%LOCALAPPDATA%\victory\logs`, saves to `%LOCALAPPDATA%\victory\data`. Close the game window to quit.

### Warming up (once per build or driver)

The first run of a build compiles pipelines while playing and stutters. Runs write down every pipeline the title
makes (`VICTORY_PIPELINE_BOOK=1`, set by the launcher); afterwards make them all ahead:

```powershell
$env:VICTORY_HOST_WAVE = '32'
target\release\victory.exe warm
```

The NVIDIA driver keeps its compiled pipelines **per program path**: a build run from another folder starts cold.

## Switches worth knowing

Every `VICTORY_...` switch is described in a comment where the code reads it. The launcher's full set is in
`scripts/play-foxhound.ps1`; these are the ones foxhound added or changed:

| Switch | Launcher | What |
|---|---|---|
| `VICTORY_DLSS`, `VICTORY_DLSS_MODE` | `quality` | DLSS Super Resolution through the title's FidelityFX dispatch |
| `VICTORY_FRAMEGEN` | off (`-FrameGen`) | DLSS Frame Generation, with the picture before the HUD tagged |
| `VICTORY_NATIVE_1440P` | on | Series X renders and outputs 2560x1440 (image patched) |
| `VICTORY_SOFT_VIEWS` | `copies` | the title's tiled-heap page mappings kept without Windows views: no freezes at scene cuts |
| `VICTORY_WC_NO_BACK`, `_MIN_KB` | `1`, `1024` | shader writes into big write-combined copies are not copied back |
| `VICTORY_AMBIENT`, `VICTORY_AMBIENT_SPECULAR` | `0.8`, `1` | estimates of indirect light and of reflections (no ray tracing) |
| `VICTORY_EARLY_DEPTH` | on | pixel programs keep the console's early depth test |
| `VICTORY_PRELUDE_UPLOADS`, `VICTORY_SHOW_NEW_LATER`, `VICTORY_TABLED_REREAD` | on | texture upload order and slot changes (flicker, device losses) |
| `VICTORY_SYNC_IN_PLACE`, `VICTORY_SPANS_QUICK`, `VICTORY_HEAP_SYNC_AT_SUBMIT` | on | cheaper descriptor-heap bookkeeping |
| `VICTORY_PORT_CONSTANTS` | off | the title's constants heap read in place: less CPU, but on a 12 GB card the GPU then waits (slower overall) |
| `VICTORY_FRAME_INTERVAL_MS` | off | experimental: the title's own 30 fps setting patched to a higher rate |
| `VICTORY_BVH_BUILDER` | off | the title's ray tracing (slow, broken) |

## How it works, and the findings

- [docs/architecture.md](docs/architecture.md): the layer's parts.
- [docs/findings.md](docs/findings.md): measurements behind the choices.
- [docs/dlss.md](docs/dlss.md): DLSS and Frame Generation.
- [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md): the title's ray tracing and the light estimate.
- [docs/replay.md](docs/replay.md): recording a frame and replaying it.
- [docs/asset-policy.md](docs/asset-policy.md): what may and may not be in this repository.

## Contributing

Issues and pull requests are welcome. Never attach or commit anything from the game: files, dumps, shader dumps
or extracted data. Describe faults technically. `cargo xtask policy` (also the pre-commit hook in `.githooks/`)
checks the asset policy.

## Credits

A tree of other people's work with more on top; the history is kept, and commits taken from elsewhere name the
commit and its author.

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its contributors: the loader, the runtime
  services, the Direct3D 12 back end and the shader recompiler.
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)** and the people whose pull requests it carries.
- **[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)**: the submission line, write
  guards, relay and much of the speed work this fork starts from.
- **[codec-d/cool_fox](https://github.com/codec-d/cool_fox)**: the base this fork was taken from.
- **[DexNom/VIctory](https://github.com/DexNom/VIctory)**: native 1440p output, the FidelityFX path on Series X,
  ray tracing fixes.
- **[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)**: the light estimate, renderer
  ports (constants heap in place, heap records), texture upload order, the frame rate switch; ported commits name
  their source.
- **[FQY55kai/ZJOU-VICTORY](https://github.com/FQY55kai/ZJOU-VICTORY)**, **[myokex/victory-amd](https://github.com/myokex/victory-amd)**
  (with Spexivus, joroyt and tightfall), Joshua Odom, **Geckomacabre** and **cxy0072019-sketch**.
- **[XCL](https://github.com/ivall/xcl)** (GPL-2.0-only) was read for how the same title behaves under another
  layer; no code of it is here.
- NVIDIA Streamline, DLSS and Reflex are NVIDIA's; they are loaded at run time from a folder you supply.
- Much of this fork's code was written with [Claude Code](https://claude.com/claude-code).

## License

GPL-3.0-or-later, as VIctory. See [LICENSE](LICENSE); the one file that is not covered is named in
[docs/asset-policy.md](docs/asset-policy.md).
