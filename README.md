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
`--validate` and `--pipelines`), `Play-VIctory.cmd` and `get-nvidia-dlss.ps1` (the DLSS fetcher, below). Put
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
Other switches that won their comparison are on in the code without an entry in that file (the ray-tracing pair, the
loop fixes, the seven switches of the submit thread's lane, `VICTORY_CLEAR_UAV` and more), and the rest of the
`VICTORY_*` variables are for comparing and for diagnostics: [docs/switches.md](docs/switches.md) lists every one with its
default. Without `VICTORY_CACHE_DIR`, `Play-VIctory.cmd` puts the cache in `cache-pareto` beside the exe (on its drive; a
`%LOCALAPPDATA%\VIctory\cache-pareto` an earlier version made stays in use), and it grows large (the pipeline library
alone is 0.5 to 1 GB): set it to a folder on a drive with room, as above. Requirements:
Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with shader model 6.6 and
mesh shader support. The first run builds shader pipelines and stutters (a first launch on an empty cache took about
21 minutes to reach the world); `victory warm --used` makes them ahead. Since `integ/validation-28` every start also
compiles, in the process, the pipelines whose root signature has static samplers (about 1,300 of 2,100; see the
white-out below), which adds roughly 110 to 130 s to a start with a warm cache.

### DLSS (NVIDIA RTX): on by default once NVIDIA's files are in place

On a GeForce RTX, with the Series S profile that `Play-VIctory.cmd` selects, DLSS Super Resolution does the title's upscaling
by itself (a 720p scene to 1080p, quality mode; on the test RTX 4060 it replaces the title's own FSR passes, the 21 MB
arena copy-back goes away, and the picture is equal or slightly crisper). Two things must hold, and nothing else is asked:

1. **NVIDIA's files are in place.** They are NVIDIA's, under NVIDIA's licences, and this project does not carry them and may
   not: they come from NVIDIA's own release (github.com/NVIDIA-RTX/Streamline, about 70 MB). The first
   `Play-VIctory.cmd` on an RTX PC where they are missing asks once, in the console, whether to download them
   (`y` downloads, anything else is a no and is not asked again); `Play-VIctory.cmd get-dlss` downloads without asking.
   Nothing is ever downloaded without one of the two. They go to `<home>\nvidia-sdk\Streamline\bin\x64`, where `<home>` is,
   in order: `VICTORY_HOME`; else `victory-home` beside the folder of `VICTORY_CACHE_DIR` (the cache's drive); else
   `victory-home` beside `victory.exe` (what `Play-VIctory.cmd` uses without a cache folder). Never the user profile: nothing
   here is written to the system drive (the fetcher's temporary files go into `<home>\tmp`). `VICTORY_STREAMLINE_DIR` names
   the folder of `sl.interposer.dll` itself if you have NVIDIA's SDK already (then nothing is fetched).
2. **Streamline says this GPU has DLSS** (its own feature check, not the card's name). Otherwise, or on AMD or Intel,
   or without the files, the title's own upscaler runs, with one line at the start saying why
   (`upscaling      DLSS Super Resolution is off: ...`), and no warning.

`VICTORY_DLSS=0` forces it off, `VICTORY_DLSS=1` forces it on (also on the Series X profile and with the files elsewhere; what
is missing is then a warning). With DLSS on, the title renders through its FidelityFX path, which is another game image
(cached in `images\upscaler-quality`): the first start with DLSS builds it, as a new build does. DLSS frame generation stays off
(`VICTORY_FRAMEGEN=1` asks for it, with `VICTORY_DLSS=1` and `VICTORY_THIN=1`; it needs about 0.5 GB of video memory more,
which an 8 GB card does not have to spare yet). Asking for frame generation without `VICTORY_DLSS=1` leaves Super Resolution
off, as before.

## Where it stands (2026-11-28)

This describes `integ/validation-28`, the build the next `main` takes. Its parts were checked in the game in builds
of integrations #16 to #27, and the white-out fix in pairs of runs of one build on one cache (below), on one PC with an
8 GB RTX 4060, in the prologue cell; the pilot runs #28 as one build as the gate before the merge. #22 to #28 add the
cold-start and asynchronous-compute switches (off), the fixes that came out of the review of the parts nobody had read,
the NaN tools, the split of `host.rs` and `lower.rs` into submodules (moves of code: the shader corpus gives 0
regressions and the same emitted DXIL as the build before), the NVIDIA DLSS port (off) and the pipeline library fix.

A frame takes about 110 to 125 ms looking at the window (8 to 9 frames a second), against 88 to 94 ms on the last
`main`. Part of that difference is the session, not the builds: the older `integ/validation-16`, run again in the same
session as `integ/validation-18`, took 109 ms like it. The state of a process alone moves a frame by about 4 ms
(standard deviation, between processes), and the state of a session by 15 to 20 ms, so the changes below are compared
as arms inside one process (`VICTORY_AB`, [docs/ab-switches.md](docs/ab-switches.md)), where a few milliseconds show.
The card works about 55 to 62 ms of a frame and waits for the CPU the rest; the CPU thread that prepares the frame (the
lane) is the limit, and the whole process keeps about 8.8 of 12 logical processors busy.

### Since the last `main` (integrations #14 to #28)

Every switch, with its default, whether it changes the shader cache key and whether `VICTORY_AB` can flip it, is in
[docs/switches.md](docs/switches.md). The rule of this fork: the best measured option is the default and a switch is
for comparing; the winner of a comparison becomes the default in the next integration.

- **The white-out, the red spot and the orange hair were one defect, now fixed (2026-11-28): the pipeline library kept
  pipelines with static samplers.** The NVIDIA driver writes into a stored pipeline the place of each static sampler
  in its own table, and that table differs between processes. A pipeline stored by one process (`victory warm` seeds
  about 1,000 of them) and loaded by the game sampled through the wrong samplers. The result was a red spot on the
  character's hair that grew into a red glow, a NaN in the title's anti-aliasing pass that blacked the character out,
  and the whole picture going white about a minute later: in about 7 % of runs until integration #16 (a small library)
  and in nearly every run from #20 (the warm seeds the library). The hair was orange where the console's is dark brown,
  for the same reason. Evidence: on one build and one copy of the cache, the old behaviour
  (`VICTORY_LIBRARY_STATIC_SAMPLERS=1`) gave a red bloom at 60 s and 36 to 39 % white from 130 s, and the fix gave 5 of
  5 clean pictures out to 300 s with brown hair (one process each; a second process with the fix is pending); the same
  cache whited out in 3 of 3 runs of the build before, and runs with the library removed or empty were clean in 15 of
  15 pictures, with brown hair. **The fix is on
  by default:** pipelines with static samplers are neither loaded from nor stored in the library, so each start compiles
  them (the cost is above). `VICTORY_SAMPLER_TABLE` (**on since integration 30**: two processes on a full library were clean, with brown hair, no
  refusal, about 100 s less to the world; `=0` restores the static samplers) makes the static samplers one-descriptor
  tables of the sampler heap, so that those pipelines go back in the library. The library needs one `victory warm` made with it.
- **16-bit rounding and flushed denormals are on as console-matching precision, not as the white-out cure**
  (`VICTORY_F16_NEAREST=2`, `VICTORY_F32_FTZ=1`, on since `integ/validation-24`; `=0` turns each off). The card
  truncates the conversion from 32 to 16 bits where the console rounds to nearest (24 of 144 cases, one step), and the
  console flushes 32-bit denormals. Two clean runs with them on first suggested they cured the white-out; with them on
  the white-out came back in 3 of 5 runs, and with them off it was seen in 14 of 14 on the same day (p about 0.06: at
  most a small part), and the real cause was found elsewhere (above). They stay on because they match the console.
- **The ray-tracing pair is on** (`VICTORY_ACTIVATE_RAYS`, `VICTORY_RT_RESOLVE`; `=0` turns each off). With it the
  poster in the cell is the console's pale score in every run; with it off the poster is purple, on the PC and on a
  cloud GPU. It costs about 10 ms a frame on the RTX 4060 (101.7 ms with it off and 111.7 ms with it on in two and
  three processes of one session) and makes the first warm-up longer (about 2,700 pipelines). It is kept on because
  the picture is wrong without it, not because it fixes the white-out: that was seen with it on (above). A later
  breakdown found the pair's own dispatches cost only about 0.45 ms and the rest comes from the chain that
  `VICTORY_ACTIVATE_RAYS` turns on (traces about 2 ms, miss shading, bins, gather); the poster needs only
  `VICTORY_ACTIVATE_RAYS`, so `VICTORY_RT_RESOLVE=0` is the likely default once its measurement rows have run (the
  estimate: 2 to 4 ms of the lane, about 1,000 pipelines and 13 to 15 minutes of a cold start less); it stays on until
  then.
- **The loop fixes are on** (`VICTORY_KEEP_LOOP_SKIPS`, `VICTORY_HALF_EXIT`): the two host waves of a console wave leave
  a loop of exchanges together, and a wave with no lane left skips its loop as the console does. With the wave-wide
  form of the kept skips the defragmentation pass's loop cut went from 7.78 ms to under 0.5 ms, an alias copy from
  19.1 to 1.9 ms and all GPU commands from 166.7 to 140.2 ms (instrumented), with no error and the same picture.
  That form (`VICTORY_KEPT_SKIPS_WIDE`) is **off** because of the load-time hangs below, so with the defaults no program
  leaves the exact form it had without the two switches (checked against the whole shader corpus: 0 regressions), and
  the 7.8 ms stay until a subset proven safe is found. `VICTORY_LOCAL_EXIT`, the fix aimed at the blocky hair, now
  passes its test on the RTX (64 of 64 lanes store, 40 of 64 without it); it stays off because the hair is no better
  with it. The shader cache key now holds every switch the translator reads (a test guards it), so the cache is made
  again on the first run. `FullAdaptive` now validates (2,680 of 2,682 compute programs).
- **The seven switches of the submit thread's lane are on** (`VICTORY_LEAN_STEPS`, `CLOSED_IDLE`, `LAND_ASIDE`,
  `LEAN_CARD`, `SNAP_ASIDE`, `ASK_ASIDE`, `ASK_MERGE`): all seven against none in one process gave -4.1 +-0.9 and
  -3.8 +-0.9 ms a frame in two processes (95 % interval), with no stale byte in 211 checked lines. Less than their
  estimates added up: the CPU is saturated, so moving work to other threads costs CPU.
- **Correctness fixes, on:** the upscaler's `ClearUnorderedAccessView` calls are implemented (they were "not
  implemented" in 186 of 212 runs; it did not stop the white-out); textures in the XOR tile modes are uploaded and
  read back (the water; the console's pipe and packer counts are an estimate, `VICTORY_XOR_UPLOAD=0` and
  `VICTORY_XOR_READBACK=0` turn them off); views past the host's title slots live in dynamic slots
  (`VICTORY_TITLE_TAIL`; the 1,000,000-view limit is the driver's, and no view was created past it); the page-write
  guards, merges, landings and the feeding of copies of the new parts were reviewed in three rounds, and the cases
  found (a lost write, a stale byte, forms that moved from exact to approximate) are fixed; the last round found
  nothing that blocks this build.
- **Services and saves.** Saves already worked (a profile, settings and the autosave of the start of the prologue are
  written under the data folder); the save calls (initialize, quota, delete, submit) that answered `E_NOTIMPL` are served;
  text entry, the pad's trigger rumble and speech (silent, with the right duration) have stand-ins. A later save and
  `Continue`/`Load` have not been tried.
- **Audio.** The Opus decoder ignores the pitch of the process command (a wrong note, not a crackle):
  `VICTORY_OPUS_PITCH=1` applies it (off until compared with a capture); the reverb stand-in has A/B switches.
- **The upscaler's `DeserializeComputePipelineStateX` is implemented**; the 29 programs translate to the same DXIL by
  both paths, so nothing is expected to change (`VICTORY_PRECOMPILED`, off).
- **Cold start.** A first launch with an empty cache took 21 minutes from the launch to the world (the warm of an
  integration took 29 minutes on the PC and 23 minutes on a cloud GPU). 61 % of a cold start is one frame of about 175
  large compute programs (the ray-tracing resolves) made one at a time; `VICTORY_PIPELINE_AHEAD`,
  `VICTORY_PIPELINE_WIDE`, `VICTORY_RESOLVE_ASYNC` and `VICTORY_PIPELINE_NOW_FIRST` (off) queue them ahead, add workers
  or make them in a lane of their own, with an estimate of 6 to 9 minutes that has not been measured;
  `victory warm --skip-resolves` leaves the resolves out. The children of `victory warm` read the pipeline library
  (except the entries with static samplers, see above). Copying the cache folder of one
  integration into the next and running `victory warm` is safe (pipelines are keyed by DXIL, root signature and state;
  translations by the translator's sources and settings).
- **In-process A/B.** `VICTORY_AB=<seconds>,<switch>` flips a switch between phases of one run and reports the
  difference with its interval; `VICTORY_AB_SHOTS=N` saves a picture of each arm. A census of the CPU time of every
  thread of the process is logged every 5 s; the diagnostics build also has a scan of buffers for NaN and infinities
  (`VICTORY_BUFFER_NAN`) and a file that skips passes while the title runs (`VICTORY_LIVE_FILE`).
- **Audio and NVIDIA features.** The audio DSP's convolutions (TVs, telephones, reverb returns, silent before) are
  computed from the title's own impulse responses with `VICTORY_DSP_CONVOLVE=1` (off). DLSS upscaling and frame generation
  come from the VIctory-NVIDIA fork (`VICTORY_DLSS`, `VICTORY_FRAMEGEN`, `VICTORY_HOTKEYS`); they need NVIDIA's own
  files, which are not in the repository ([docs/switches.md](docs/switches.md)). DLSS Super Resolution is on by default
  on an RTX with the files in place (see "DLSS" above); frame generation is off.
- **New and off, waiting for a measurement or a validation:** the video-memory pool compaction (`VICTORY_POOL_COMPACT`,
  `BETWEEN_FIT`), the look-ahead of stage A (`STAGE_A_LOOKAHEAD`), the audited form of the kept skips
  (`VICTORY_KEPT_SKIPS_WIDE=audited`), the automatic wave-wide form (`VICTORY_WAVE_WIDE_AUTO`), the guided page refresh (`VICTORY_PAGE_REFRESH`), one-pass
  looks at a submission's steps (`ONE_PASS`), the precise write footprints and barriers (`PRECISE_*`, `JOIN_WAITS`),
  folded indirect draws and views (`INDIRECT_FOLD`, `FOLD_VIEWS` with `VIEW_INDEX`), stage A of the pipeline and its
  relaxed and deeper forms (`STAGE_A*`), copy-queue sends (`SEND_COPYQ`), a second compute queue
  (`VICTORY_ASYNC_COMPUTE=2`), thread core and priority switches (`LANE_CORE`, `LANE_HIGH`, `LIST_NICE`, `WAIT_NICE`), the
  frame-rate target (`TARGET_FPS`), the builder of the ray-tracing structures (`BVH_BUILDER`, `BVH_SIZES`), and the
  float-mode experiments below.

Every change to the translator is checked against the whole shader corpus (on the Linux build) for regressions before
it is merged, and an integration is published only after the CI (builds, tests, policy) passes.

### On `main` since 2026-11-27 (integrations #2 to #13)

The list as of 2026-11-27, measured on `integ/validation-3` to `-13`, in the prologue cell, one PC with an 8 GB
RTX 4060. A frame took about 410 ms at the start of 2026-11-26 and 88-94 ms on `main` then
(about 11 frames a second, looking at the window), with the card
working about 55 ms of it (95-113 ms before the card-copy changes, 67 ms before the
lean pair meetings) and the CPU
about 770 ms of thread time a frame (940-990 before).

On `main` before the integrations above (integrations #2, #3, #6, #8, #10 and #13 are merged):

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

(Fixed since the last list: the grainy glass no longer reproduces: about 100 clean pictures of the window in 123 runs
since `integ/validation-3`, warm caches included.)

- **Blocky hair, intermittent.** Posterised patches on the top of the hair in some processes; in others the hair is
  smooth in every picture. The loop fix thought to cause it (`VICTORY_LOCAL_EXIT=1`) was run in two arms on the RTX
  and did not change it. The cause is not found.
- **White-out, red spot and orange hair: fixed (2026-11-28), by keeping static-sampler pipelines out of the pipeline
  library** (above). Confirmed on one build and one cache by switching the old behaviour back on (white) and off (clean), one
  process each, with the library-free runs agreeing. The 16-bit rounding and the flushed denormals were first credited
  with it; they are not the cure. Processes of the validation build with the fix on are the gate before the merge.
- **The picture is darker than the console.** At the cell window, the mean luma is 67 to 71 where the console's frame is
  92 (everything about 0.65 to 0.7 of the console's, in every run with or without the library fix; the yard is ochre
  where the console's lawn is green). The owner of the brightness (indirect light, exposure) is being traced.
- **Orange fog in bright rooms** is most likely the empty ray-tracing structure: the title's builder stages are
  dropped, so every ray misses and the sun and sky are visible from inside the buildings. That is a hypothesis; the
  indirect-light stand-in (`VICTORY_AMBIENT`) was ruled out as the cause. `VICTORY_BVH_BUILDER=1` runs the real
  builder, but the picture is still not right with it (the suspect is overlapping dynamic structures whose compacted
  size the title reads a frame or two before the card writes it; `VICTORY_BVH_SIZES` is the experiment) and it costs
  about 16 ms of card, 18 ms of lane and 3 GB, so it is off.
- **Load-time GPU hangs.** Five times in builds of integrations #18 to #21 the card hung (`0x887A0006`) while the
  title was loading, before the world appeared. All five were in builds that took the kept loop skips in a wave-wide
  form (the first form on #18 and #19, the safe-classes form on #21); none in about seven starts with that form off.
  An audit found a group barrier behind a divergent branch in 91 of the 116 programs that take the form; that this
  hangs NVIDIA is not proven. The form is off by default (`VICTORY_KEPT_SKIPS_WIDE`). A detector that limits barriers
  and writes a marker instead of hanging is on the branch `fix/hang1` and not in this build.
- **Colours band on some processes: not seen at the cell window in recent runs, cause not settled.** The bands were
  the state of a process: the volume that holds the lighting probes (a grid of 32x16x64 cells) had blocks of zero values
  past the depth range, which grew with depth in some processes and not in others. None of the last runs shows
  them (with or without the library fix), but nobody has shown why they went away.
- **Glass.** The window glass of the cell matches the console (the ripple and the chain-link; the grainy glass of the
  early integrations has not come back), but that is the only glass compared; glass elsewhere in the world is not
  checked and is not considered right.
- **Dark blocks on the water.** The water's tiled light programs end their loops differently on 32-lane cards than on
  the console's 64-lane waves, so some tiles come out empty and show as dark blocks. The fix
  (`VICTORY_LOOPS_WAVE`, `VICTORY_LDS_FIRST_INIT`, in work) is not in this build. Seen in runs that walked out of the
  cell; the cell window spot, where the measuring runs stay, does not show the water.
- **Cold start of about 21 minutes** on an empty cache (above); the measurement of the switches meant to shorten it
  (and of `VICTORY_RT_RESOLVE=0`) is pending.
- **The ray-tracing chain costs about 7 to 10 ms a frame** and is kept for the picture (see above).
- **Slow and capped.** About 110 to 125 ms a frame on this PC, and the CPU thread that submits the frame is still the limit.
  The estimates for the cuts in progress (not measurements) bring that thread from about 85 ms to 35 to 40 ms, while
  the card works about 55 to 62 ms, so the card would be next; 60 frames a second needs less work per frame, not only
  moving it to other threads. The title caps itself at 30 frames a second; lifting that without breaking physics,
  animation and cutscenes is the second half of the goal.
- **Not checked or not done:** `Continue`/`Load` after the first autosave; audio (the reverb returns are dry, the Opus
  pitch is ignored unless asked, 290 to 540 dropouts a file on the cloud runs); the console's tiling for the water
  read-back uses estimated pipe and packer counts; the title's HDR, speech and screenshot services are stand-ins; 6
  programs still have a loop cut at its iteration limit; two compute programs still fail validation.

## Build from source

Install Rust (the toolchain is pinned in `rust-toolchain.toml`), the MSVC C++
build tools and a Windows SDK, then:

```powershell
cargo build --release --locked -p victory

# with the diag build, to have the pipelines made ahead:
target\diag\victory.exe shaders <the shader archives>   # every shader translated, minutes
target\diag\victory.exe warm --used --compute --now       # the pipelines earlier runs drew with (VICTORY_PIPELINE_BOOK=1 writes
                                                          # them down) and every compute pipeline of the shader archives, into the
                                                          # pipeline library; `warm --verify` asks the library for them
```

`Play-VIctory.cmd` finds `target\release\victory.exe`. The diagnostics build is
`cargo build --profile diag --locked -p victory --features diagnostics`. CI
builds both and runs the tests on demand
([workflow](.github/workflows/build.yml)). Commands and debugging switches:
[docs/README-fxceless.md](docs/README-fxceless.md); every switch with its default: [docs/switches.md](docs/switches.md). The history of the AMD
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
