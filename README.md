# VIctory on NVIDIA

> **Before you play**
>
> - **You must install DLSS yourself:** run `Get-NVIDIA-DLSS.cmd` once (it is
>   in this folder; a double click does it), or have your AI assistant run it
>   for you. It downloads NVIDIA's DLSS and frame generation files, about 70 MB,
>   from NVIDIA's own GitHub; they are not part of this repository. Without
>   them the game still runs, with its own upscaling and no frame generation.
> - **F1 in the game brings up the settings menu** (arrow keys pick and
>   change, Enter does it).
>
> The steps are under [How to run it](#how-to-run-it).

A fork of [VIctory](https://github.com/Marvisak/VIctory), a compatibility layer
that runs an Xbox Series title natively on Windows from files **you** supply,
tuned for **NVIDIA RTX** cards.

> **This repository contains no game files, code or assets.** You need your
> own legally obtained copy of the title. It is not affiliated with or endorsed
> by the title's developer or publisher, nor by NVIDIA. See
> [docs/asset-policy.md](docs/asset-policy.md).

## Built from many projects, and from running every one of them

This repository did not come from one place, and the base it stands on today
is the end of a long comparison, not the start of the work. Over several weeks
we built, ran and measured many projects on the same NVIDIA PC: fork-l,
[Marvisak's VIctory](https://github.com/Marvisak/VIctory),
[fxceless's fork](https://github.com/fxceless/victory-fork),
[the 7900 XTX fork](https://github.com/avatarmanz/VIctory-7900XTX),
[Belobug's](https://github.com/Belobug/VIctory-Belobug-AMD),
[DexNom's](https://github.com/DexNom/VIctory),
[CGBJ's cool_fox](https://github.com/codec-d/cool_fox), Kelvaroq's, Neptune,
xcl (another layer for the same title) and then
[foxhound](https://github.com/readmet3xt-lab/foxhound). On each we found what
stopped it on an RTX card, fixed or worked around it, measured the result, and
carried what we learned to the next. Every pull request sent here was built
and run on the same card before it was merged. No single project got here
alone, and choosing the base that runs best was only one result of that work.

### What this fork brings

Not one fix but a long line of them: days of work, much of it around the clock,
across every one of those codebases, more than 300 measured runs on the same
RTX PC, and each fault chased towards its cause before the next build. In rendering, in
speed, in the picture and in simply being able to play it:

**Rendering fixes**

- **Draws no longer skipped on NVIDIA.** NVIDIA caps the view heap below what
  the title asks for; thousands of draws a frame were dropped, the screen
  rarely updated and the character jumped from place to place. The pool of
  descriptor slots was reworked until almost none were.
- **Menus, loading screens and subtitles on Series S**, black until then (the
  interface plane laid over the game), and **faces that move** (their network
  carried through as integers): ported from fxceless's pull requests 1189 and
  927 and made to work on this card.
- **The intro video without flicker**: its frames came out of order.
- **Black blobs gone**: a single bad number from a shader, spread by blur and
  the upscaler into patches on faces and in the air, is caught where it is
  written.
- **White, hazy rooms explained and tamed**: the title's ray tracing was
  running with its structure builder off, so every ray missed. Which passes
  can be left out without filling the picture with rainbow glitter was worked
  out pass by pass.
- **The ray-tracing structure builder running on NVIDIA**, on CGBJ's renderer:
  the draws dropped for it went from thousands a frame to one.
- **White-outs with black holes further into the game** followed up in frames
  captured in play: thousands of draws a frame were being dropped there for
  that same missing structure.

**Speed**

- **Memory compares moved to the card**: 11 to about 22 frames a second on the
  base of the time.
- **Settings measured, not guessed**: one switch at a time, on the same scene,
  run after run.
- **The base itself chosen by measurement**: the same rooms went from 15-17
  frames a second to 26-30, at a higher resolution.
- **The 30 frames a second were the title's own frame interval**, not the
  layer's limit: found, and lifted.

**Picture and frames**

- **DLSS in place of the title's upscaler**, with its model and mode to choose.
- **DLSS frame generation with up to six frames shown for each one rendered**,
  switched and changed while playing, without a restart (foxhound's is 2x, set
  at the start). By Streamline's own count it shows exactly 2, 4 and 6.
- **Why frame generation showed nothing on a full 12 GB card**, found and
  fixed: the card's memory is now shared out so that it has room.
- **The frame rate at the top left of the picture**: frames rendered a second
  and, with frame generation, frames shown (`FPS: 28.4  FG: 170.4`).

**Playing it**

- **The F1 settings menu**, in every build: DLSS, frame generation, frame rate
  cap, console profile, picture size, the light estimates and the main speed
  switches, saved to `victory-settings.txt` and applied at the next start
  ("SAVE AND RESTART NOW" does that for you; lines marked LIVE change at once).
- **One command to play**: the launcher builds the layer the first time and
  applies what the menu saved.
- **`Get-NVIDIA-DLSS.cmd`**: the eight NVIDIA files that are loaded, about
  70 MB, taken out of NVIDIA's 263 MB release without downloading the rest.
  NVIDIA's licence does not let a repository carry them, so they come from
  NVIDIA.

**Tested, not assumed**

- Every pull request sent here was read line by line and, where it touched the
  game, built and run on this card before it was merged: ten so far.

Where each of these is today: this fork's own code in this tree is the F1
menu, frame generation from 2x to 6x with its live switching and memory
handling, the lifted frame rate cap, the frame rate line, the launcher and the
NVIDIA file fetcher. The foxhound line carries the descriptor slots, the
compares on the card, the interface plane and the face network in its own
form. The black-blob scrub, the graphics card picker and the ray-tracing
builder path are in this repository's history, and come over as this base
turns out to need them.

### The base since 2026-11-26: foxhound

On 2026-11-26 this fork moved onto
[readmet3xt-lab/foxhound](https://github.com/readmet3xt-lab/foxhound)
(`02fc18a`, GPL-3.0-or-later). On the same PC it ran the same rooms at close to
twice the frame rate of what this fork had, at a higher resolution, so its
tree is now this one's, with its history kept, and this fork's own additions
are on top. What foxhound is and how it got there is in its own README, kept as
[docs/README-foxhound.md](docs/README-foxhound.md); its measurements are in
[docs/findings.md](docs/findings.md).

What foxhound brings:

- the Series X profile at a native 2560x1440;
- DLSS Super Resolution and DLSS Frame Generation through NVIDIA Streamline,
  loaded when the game runs (nothing of NVIDIA's is needed to build);
- an estimate of indirect light and of reflections in place of the title's ray
  tracing, and fewer stalls at scene cuts.

What this fork has on top of it is under
[What this fork brings](#what-this-fork-brings) above.

## Where it stands

Tested on one PC: Windows 11, Ryzen 7 9800X3D, 64 GB RAM, **GeForce RTX 5070
12 GB**; the title's Xbox Series build v1.0.2.1, Series X profile, 2560x1440
output, DLSS Quality. Frame rates are the layer's own count, in the opening
rooms.

| | |
|---|---|
| Opening rooms | mostly 26-30 frames a second rendered (33-38 ms a frame) |
| Frame generation | 2x to 6x on top of that: by Streamline's own count, exactly 2, 4 and 6 frames shown for each one rendered |
| Device losses, crashes | none in the measured runs |
| Before this base (Series S, 720p to 1080p by DLSS) | 15-17 frames a second in the same rooms |

foxhound's own measurements, on an RTX 4070 SUPER, are in its README.

What is known to be wrong:

- **No ray tracing.** The title's own is off (`VICTORY_BVH_BUILDER=1`, or the
  menu's RAY TRACING, turns it on: slow and with a broken picture on this
  base). An estimate stands in for indirect light and reflections.
- **Frame generation needs room in the card's memory.** The layer fills the
  card with textures up to its budget; with frame generation on from the start
  the launcher holds a fifth of the budget back (a little less while it is only
  ready), so a few more textures can come late. Switched on from the menu on a
  full 12 GB card it can still say that memory is past the budget; it then
  runs, perhaps unevenly.
- **Frames are only generated while the game's window has the focus**, and
  about a second passes after a change before they come.
- **The first run of every new build stutters** while the driver compiles
  pipelines (foxhound's README, *Warming up*, says how to make them ahead).
- "SAVE AND RESTART NOW" can leave the old game's process behind without a
  window; it goes when the PC restarts.
- What foxhound lists as wrong (the heat-haze pass left out, hitches in
  cutscenes) applies here too.

## How to run it

You need Windows 10 or 11 (x64), a CPU with AVX2, 32 GB of RAM or more, an
SSD, an NVIDIA GeForce RTX card with a recent driver (RTX 40 or later for frame
generation, with hardware-accelerated GPU scheduling on; more than 2x needs an
RTX 50), [Rust](https://rustup.rs) (the toolchain is pinned), the Visual Studio
Build Tools with "Desktop development with C++", and **your own dump** of the
title's Xbox Series build v1.0.2.1 (the folder with
`game_scarlett_master.exe`).

```powershell
git clone https://github.com/trustno1-coder/VIctory-NVIDIA
cd VIctory-NVIDIA
copy victory.example.toml victory.toml      # then set game_dir to your dump's folder, outside this one
.\Get-NVIDIA-DLSS.cmd                        # once: NVIDIA's DLSS and frame generation files, about 70 MB
scripts\play-nvidia.ps1 -Exe 'D:\Games\MyDump\game_scarlett_master.exe'
```

The first start builds the layer (a few minutes). `Get-NVIDIA-DLSS.cmd` gets
NVIDIA's files from
[NVIDIA-RTX/Streamline](https://github.com/NVIDIA-RTX/Streamline) into
`nvidia-sdk\` (git-ignored). They are NVIDIA's, under NVIDIA's licences, which
are saved beside them; this repository does not carry them and may not. Nothing
is downloaded unless you run that file; `VICTORY_STREAMLINE_DIR` names another
folder if you have the Streamline SDK already.

`scripts\play-nvidia.ps1` is `scripts\play-foxhound.ps1` under the name this
fork has always had; its options:

| Option | What |
|---|---|
| *(none)* | Series X profile, 2560x1440, DLSS Quality, no frame rate cap, frame generation ready but off |
| `-FrameGen`, `-FrameGenX 4` | frame generation on from the start, 2 to 6 frames shown for each one rendered |
| `-Dlss off` / `dlaa` / `balanced` / `performance` | the title's own upscaling, or another DLSS mode |
| `-Fps 60` | a frame rate cap (30 is the title's own) |
| `-SeriesS` | the Series S profile: 1280x720 inside, faster, softer |
| `-Res 1080to1440` / `4k` | scene at 1080p upscaled by the title / the stock 3840x2160 output |
| `-NoFrameGenReady` | frame generation only with `-FrameGen` (it is otherwise made ready at every start, so the menu can switch it) |
| `-Build diag` | the diagnostics build (slower; captures, timelines, the F3 numbers panel) |
| `-More @{VICTORY_X='1'}` | add or override any switch |

What the F1 menu saved (`victory-settings.txt`) goes over these at every start;
delete that file to go back to them. Full screen: start the game, then run
`.\scripts\fullscreen.ps1` in another window. Logs go to
`%LOCALAPPDATA%\victory\logs`, saves to `%LOCALAPPDATA%\victory\data`.

## How it works, and the findings

- [docs/README-foxhound.md](docs/README-foxhound.md): foxhound's own README
  (set-up by hand, warming up, its switches).
- [docs/architecture.md](docs/architecture.md): the layer's parts.
- [docs/findings.md](docs/findings.md): measurements behind the choices.
- [docs/dlss.md](docs/dlss.md): DLSS and Frame Generation.
- [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md): the title's ray tracing
  and the light estimate.
- [docs/asset-policy.md](docs/asset-policy.md): what may and may not be in this
  repository.

## Credits

A tree of other people's work with a little on top; the history is kept.

- **[foxhound](https://github.com/readmet3xt-lab/foxhound)** by
  readmet3xt-lab: the base of this fork since 2026-11-26, and through it:
- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors: the loader, the runtime services, the Direct3D 12 back end and
  the shader recompiler.
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)**,
  **[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)**,
  **[codec-d/cool_fox](https://github.com/codec-d/cool_fox)**,
  **[DexNom/VIctory](https://github.com/DexNom/VIctory)**,
  **[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)**
  and the others foxhound's README names.
- Everyone who sent pull requests to this fork before the move; that work is
  in the history, on the branch that was `main` until then.
- NVIDIA Streamline, DLSS and Reflex are NVIDIA's; they are loaded at run time
  from a folder you supply.
- Much of this fork's code was written with
  [Claude Code](https://claude.com/claude-code).

## License

GPL-3.0-or-later, as VIctory and foxhound. See [LICENSE](LICENSE); the one file
that is not covered is named in [docs/asset-policy.md](docs/asset-policy.md).
