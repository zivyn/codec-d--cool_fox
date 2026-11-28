# VIctory for an 8 GB NVIDIA card

A fork of [VIctory](https://github.com/Marvisak/VIctory), a compatibility layer
that runs an Xbox Series title natively on Windows from files the user
supplies. It is built on [avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)
and on codec_d's RTX and 32 GB RAM work. The goal of this fork is the title
playable on an 8 GB NVIDIA card (RTX 4060 class, the Series S profile, 1080p),
aiming at 60 frames a second. It is tested on one PC with an 8 GB RTX 4060 and
is not there yet.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's developer
or publisher.

## Download

Builds are made by GitHub Actions on a Windows runner, started by hand. Two kinds of builds
are kept as rolling prereleases (each is replaced by the next published build of its branch):

- [`build-main`](https://github.com/zivyn/codec-d--cool_fox/releases/tag/build-main):
  the `main` branch, what has been checked in the game.
- `build-integ-*` (see the [releases](https://github.com/zivyn/codec-d--cool_fox/releases)):
  the integration branches, newer and still under test.

Each release holds `victory.exe` (release build), `victory-diag.exe` (the same
with the `diagnostics` switches), `dxil.dll` (the DXIL validator, only for
`--validate` and `--pipelines`) and `Play-VIctory.cmd`. Put
them in one folder. The game itself is not included: you supply your own dump.

## Run

Open PowerShell in that folder and point the launcher at your own complete dump:

```powershell
$env:VICTORY_GAME_DIR = 'D:\Games\MyDump'
$env:VICTORY_CACHE_DIR = 'D:\VIctory\cache'   # recommended: keeps the shader cache off the system drive
.\Play-VIctory.cmd pareto-residency
```

The launcher selects the Series S profile. The settings the title is played with
(the ones checked in the game on the RTX 4060: relay, card steps, thin lists,
joined parts, upload ahead, ambient light and the rest) are built into the
executable, so `victory.exe run <game> --console series-s --execute` with no
`VICTORY_*` variable in the environment behaves as the launcher does; they are in
`crates/victory-gpu/src/defaults.rs`. A variable you set wins over them and `=0` turns
one off (`VICTORY_THIN=0`; `VICTORY_JOIN_PARTS=1` and `VICTORY_MAP_THREADS=1` take a
number; `VICTORY_AMBIENT=0` and `VICTORY_AMBIENT_AIR=0` leave the title's own light).
Other `VICTORY_*` variables are for comparing and for diagnostics. Without `VICTORY_CACHE_DIR` the cache goes to
`%LOCALAPPDATA%\VIctory\cache-pareto`, and it grows large. Requirements:
Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with shader model 6.6 and
mesh shader support. The first run builds shader pipelines and stutters;
`victory warm --used` makes them ahead.

## Since the last `main` (integrations #14 to #18, 2026-11-27 and 28)

This is `integ/validation-18` (`build-integ-validation-18`), checked on the same PC. What it adds to the
list below, in the order it matters:

- **Ray tracing passes are on by default** (`VICTORY_ACTIVATE_RAYS`, `VICTORY_RT_RESOLVE`, `=0` turns each off). With
  them the poster in the cell is the console's pale score again in every run, where the default build showed it
  purple. It costs about 10 ms a frame on the RTX 4060 and is kept on for correctness: it prevents the white-outs
  and fixes the poster. The ray programs that shade hits also make the first warm-up longer (about 2,700 pipelines,
  23 minutes on the test PC with `victory warm`). The rare white screen was not seen with it in the runs made, which
  are too few to say it is gone.
- **The loop fixes are on by default** (`VICTORY_HALF_EXIT`, `VICTORY_KEEP_LOOP_SKIPS`, `=0` turns each off): the two host
  waves of a console wave leave a loop of exchanges together (the half that leaves clears its exchange dword at every way
  out of the loop). A wave with no lane left skips its loop as the console does when `VICTORY_KEEP_LOOP_SKIPS` is on and
  `VICTORY_KEPT_SKIPS_WIDE=1` takes that skip wave-wide. On the RTX the defragmentation pass's loop cut went from 7.78 ms
  to under 0.5 ms, an alias copy from 19.1 to 1.9 ms and all GPU commands from 166.7 to 140.2 ms (instrumented), with no
  errors and the same picture. That measured win is the kept skip taken wave-wide, and after a device removal seen on
  `integ/validation-18` while loading the wave-wide branches are the suspect, so `integ/validation-18b` keeps the
  forms a program had without the switch (all exact, no group barrier at a branch): under the defaults
  `VICTORY_KEEP_LOOP_SKIPS` no longer moves any program out of an exact form or into the wave-wide-barrier form (36
  programs did on 18), and the two defragmentation programs have to be asked for the cut with
  `VICTORY_KEPT_SKIPS_WIDE=1` (off; it is in the shader cache key) until that form is cleared. The shader cache key
  now holds every switch the translator reads (a test guards it), and the translated programs change, so the cache is
  made again on the first run. Checked against the shader corpus: 0 regressions, and the forms of the defaults equal
  those with both switches off for all 321 compute programs that are retried in another form. `VICTORY_LOCAL_EXIT` (aimed at the
  blocky hair) stays off until its fix is validated. Also: `FullAdaptive` now validates (2,680 of 2,682 compute programs).
- **The launcher's settings are code defaults** (`crates/victory-gpu/src/defaults.rs`): `Play-VIctory.cmd` only sets
  the cache and the log. Rule of this fork: the best known setting is the default, a switch is for comparing, and
  the winner of a comparison becomes the default in the next integration.
- **A/B inside one process** (`VICTORY_AB=<seconds>,<switch>[=<value>]`, `docs/ab-switches.md`): the run alternates a
  switch between phases and reports the difference with its interval (`bin/bench1-ab.py`, `VICTORY_AB_SHOTS=N` saves
  a picture of both arms). Many switches can be flipped live; the ones that change the translation cannot, and are
  compared between processes (the table at the end of that file lists the ones now on).
- **New switches, all off** (each described in `docs/ab-switches.md` and `docs/README-fxceless.md`): the lean and
  one-pass looks at a submission's steps, closed idle lists, asides for the write watch and the snapshots
  (`VICTORY_LEAN_STEPS`, `ONE_PASS`, `CLOSED_IDLE`, `LAND_ASIDE`, `ASK_ASIDE`, `ASK_MERGE`, `SNAP_ASIDE`), the copy
  queue for sends (`SEND_COPYQ`), folded indirect draws (`INDIRECT_FOLD`), the card's touched-copies queue
  (`LEAN_CARD`), guard stripes (`GUARD_SHARDS`), early drop of discarded dispatches (`DROP_EARLY`), pipeline boost
  (`PIPELINE_BOOST`, `PIPELINE_WAIT_ADAPT`), real occlusion (`REAL_OCCLUSION`), the frame-rate target
  (`TARGET_FPS`, rewrites the title's settings file with a backup), key files for navigation runs, RAM switches
  (`HEAP_SPARSE`, `RAM_LEAN`) and the counters that go with them.
- **The upstream sync of 2026-11-27** is in. Every change to the translator is checked against the whole shader
  corpus (on the Linux build) for regressions before it is merged.

Not in `main` yet, because they are waiting for a run in the game (`integ/validation-19`, all off): the guided page
refresh, the precise write footprints, view indexing for folded draws, the first stage of asynchronous compute and
the fix for `VICTORY_LOCAL_EXIT`.

## Where it stands (2026-11-27, before these)

Measured on `integ/validation-3` to `-10`, in the prologue cell, one PC with an 8 GB
RTX 4060. A frame took about 410 ms at the start of the day; it now takes
88-91 ms looking at the window (about 11 frames a second), with the card
working about 55 ms of it (95-113 ms before the card-copy changes, 67 ms before the
lean pair meetings) and the CPU
about 770 ms of thread time a frame (940-990 before).

On `main` (integrations #2, #3, #6, #8 and #10 are merged, so `main` and the `build-main`
download have all of this):

- FSR4's shared working buffer lives in video memory: its passes went from
  about 215 ms to about 24 ms in a microbenchmark, close to free on the card.
- Pipelines are made ahead, and compute dispatches wait for their own pipeline
  instead of being dropped; cold-start waits are shorter.
- The title's own hang watchdog no longer ends a session during our long stalls.
- Wave64 programs are lowered for NVIDIA's 32-lane waves.
- Compute programs with several console waves in a group (the defragmentation and
  triangle-collection passes among them) now translate on 32-lane hosts: the two
  host waves of a console wave meet by counters of their own. The 195 geometry
  programs (and their export stages) translate, and the 71 mesh programs whose
  output passes 32 KiB create pipelines on NVIDIA (40 KiB of mesh output). Only
  three compute programs still do not translate (one is the adaptive
  shadow-ray variant).
- The descriptor heap stays under NVIDIA's 1,000,000-descriptor limit.
- The heap sync and the refreshes of copies skip work that has nothing to do
  (heap wait 26 to 16-22 ms, refreshes 52 to 40-45 ms). A copy on the card of
  a buffer the title's CPU cannot write is not taken whole again until a write
  is announced (window spot 158 to 142.7 ms, the submit thread 153.6 to 135.8 ms).
- The ambient stand-in for the title's indirect light: the dark cell now
  matches the console frames, and the window shows the rippled glass and the
  chain-link.
- A lower mip is shown instead of a black texture when the card is short of
  room, and textures are made level by level on demand.
- More shader programs validate (`shaders/validator-clean`,
  `shaders/rt-shadow-view-addresses`); 25,582 of 26,096 validated with the DXIL
  validator before these two.
- The helper threads a submission waits for run above the title's threads, and a
  submission's parts and lists are joined (16 parts, 96 lists): the window spot
  went from about 152 to 114-135 ms. Threads that wait for new card memory at the
  same time share one `MakeResident` (peaks of 11.3 s a frame became 1.7 s), a
  title buffer's copy is made with the buffer map free, and big copies of title
  buffers get their card room first.
- `victory warm --used --compute` makes the pipelines before playing (476 s on the
  test PC): launching with it first removes the frozen minutes of a cold cache.
- The shader translator also builds on Linux (`victory-translate`, with the
  validator as `libdxil.so`); 26,515 programs came out identical to the Windows
  build's.
- The defragmentation pass takes 8.4 ms a frame instead of 13.2 (the two host waves of
  a console wave meet by lean counters), and all GPU commands 54.6 ms instead of 59.7.
  The frame time is the same within noise, because the submit thread is the limit.
  Switches for heap sync, join ramp, merged step reads, a holder index and landing
  threads exist (all off); they showed no gain.
- Card copies of title buffers of 1 MiB and more get the big copies' margin
  (the "medium" buffers), which took about 29 ms (23 %) off a frame; the AVX2
  compare of copies against their snapshots and the re-marking of kept snapshot
  units are on, about 6 and 7.5 ms. Mesh output and lens-flare scan programs
  (the exact wave-wide form) were fixed, and the ambient air light has a cap
  (`VICTORY_AMBIENT_AIR=<mult>,max:<level>`).

## Known wrong

- Hair is blocky in some frames, intermittently (`VICTORY_LOCAL_EXIT` is the fix in progress; off by default).
- Colours band on some machines, and glass is grainy in some runs with a warm cache;
  being investigated.
- Bright rooms show an orange fog.
- A full-screen white-out was seen about once in a few first runs on a fresh cache;
  it recovers by itself. A rare defect that predates the multi-wave work; being tracked.
- The CPU thread that submits the frame is still a large share of the frame.
- The title caps itself at 30 frames a second; lifting that without breaking
  physics, animation and cutscenes is the second half of the goal.

## Build from source

Install Rust (the toolchain is pinned in `rust-toolchain.toml`), the MSVC C++
build tools and a Windows SDK, then:

```powershell
cargo build --release --locked -p victory

# with the diag build, to have the pipelines made ahead:
target\diag\victory.exe shaders <the shader archives>   # every shader translated, minutes
target\diag\victory.exe warm --used --compute --now       # the pipelines earlier runs drew with (VICTORY_PIPELINE_BOOK=1 writes
                                                          # them down) and every compute pipeline of shaders.rpf, into the
                                                          # pipeline library; `warm --verify` asks the library for them
```

`Play-VIctory.cmd` finds `target\release\victory.exe`. The diagnostics build is
`cargo build --profile diag --locked -p victory --features diagnostics`. CI
builds both and runs the tests on demand
([workflow](.github/workflows/build.yml)). Commands and debugging switches:
[docs/README-fxceless.md](docs/README-fxceless.md). The history of the AMD
RX 7900 XTX work is in avatarmanz's README.

## Credits and license

This is a tree of other people's work with more on top; the history is kept.

- [VIctory](https://github.com/Marvisak/VIctory) by Marvisak and its
  contributors: the loader, the runtime services, the Direct3D 12 back end and
  the shader recompiler.
- [avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX):
  the AMD-tuned fork this one comes from, and the people it credits
  (fxceless, DexNom, Belobug, myokex and others).
- codec_d: the RTX fidelity and 32 GB RAM residency work in the `pareto-residency` build.
- Changes in this fork were made with [Claude Code](https://claude.com/claude-code).

GPL-3.0-or-later, as VIctory. See [LICENSE](LICENSE); the one file that is not
covered is named in [docs/asset-policy.md](docs/asset-policy.md).
