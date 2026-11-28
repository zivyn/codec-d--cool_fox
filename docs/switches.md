# Switches: every `VICTORY_*` variable, its default and what it is

The census of 2026-11-28: every `VICTORY_*` variable the code reads (547 names, 40 of them only in tests or comments), with the default the code gives it
when it is unset, whether it changes the translation of shaders (and so is in the shader cache key), whether
`VICTORY_AB` can flip it inside one running process, and what it is for. It was made by reading the place where each is
read, on `integ/validation-22` (`b181327a4`), and the few differences of `integ/validation-23` are marked. The defaults in the "What is on" table below are those of `integ/validation-28`, the main candidate, and are unchanged in `integ/validation-29` (the fix for the white-out is the one default that changed after `integ/validation-24`; every switch added from `integ/validation-25` to `-29` is off, except `VICTORY_LAND_AHEAD` of `integ/validation-29`, which is on and bit-exact with the old loop (`=0` restores it); `VICTORY_AMBIENT_GAIN` defaults to 1, which changes nothing). The other
documents say more about some of them: [README-fxceless.md](README-fxceless.md) (commands and the debugging table),
[ab-switches.md](ab-switches.md) (the A/B inside one process, and why each live switch may change at any instant).

How to read it:

- **Default** is what the executable does with no variable in its environment. `on (=0 turns off)` is a switch that is on
  unless it is `0`; `off` is one that is only on when asked for (mostly `=1`; some read other values, see the purpose).
  A number is the value used when none is named. The settings of [`defaults.rs`](../crates/victory-gpu/src/defaults.rs)
  (relay, card steps, thin lists, joined parts, upload ahead, the ambient light and the rest, 19 in all) are on by default
  with the value listed there.
- **Rule of this fork (rule 14): the best measured option is the default; a switch is for comparing.** After a comparison
  (in the process by `VICTORY_AB`, or in pairs of runs) the winner becomes the default in the next integration and the
  loser is removed or stays off with a note. A switch that is new and not measured is off.
- **Cache key**: a switch that changes the translated shader is part of the key of the shader cache
  (`crates/victory-gpu/src/shader_cache.rs`), so a cache made with one value is never served for another. They cannot flip
  inside a process; they are compared between processes.
- **A/B live**: the switches `VICTORY_AB=<s>,<switch>` can flip while the title runs (diagnostics build,
  `victory-diag.exe`). The release build reads them once, from the environment.
- `diag` marks a variable that does something only in the diagnostics build.

## What is on and what is off, and why (decisions of 2026-11-28)

**On by default** (each was measured or needed for a correct picture, on the 8 GB RTX 4060 test PC unless said otherwise):

| switch | default | evidence |
|---|---|---|
| `VICTORY_ACTIVATE_RAYS`, `VICTORY_RT_RESOLVE` (the ray-tracing pair) | on (`=0` leaves the pass out) | With them the poster in the cell is the console's pale score in every run, and purple with them off (on the cloud GPU the same in the 3 runs with them and the 3 without). They cost about 10 ms a frame (101.7 ms with them off, 111.7 ms on, 2 and 3 processes of one session) and make the first warm-up longer (about 2,700 pipelines). Kept on for correctness. They do not stop the white-out (see the README). |
| `VICTORY_KEEP_LOOP_SKIPS`, `VICTORY_HALF_EXIT` | on (`=0` off), in the cache key by value | In-process comparison on the RTX: the loop cut of the defragmentation pass 7.78 ms -> under 0.5 ms, an alias copy 19.1 -> 1.9 ms, all GPU commands 166.7 -> 140.2 ms (instrumented), no error and the same picture. That gain needs the wave-wide form of the kept skips, which is off (below): with the defaults alone no program leaves the exact form it had without them. |
| the seven of the submit thread's lane: `VICTORY_LEAN_STEPS`, `CLOSED_IDLE`, `LAND_ASIDE`, `LEAN_CARD`, `SNAP_ASIDE`, `ASK_ASIDE`, `ASK_MERGE` | on (`=0` off) | All seven against none in one process, two processes: -4.11 +-0.85 and -3.79 +-0.92 ms a frame (95 % interval), a win in 2 of 2; no stale copy in 211 checked lines (`VICTORY_KEEP_CHECK`). Less than the sum of the estimates: the CPU is saturated, so moving work to threads costs CPU. |
| `VICTORY_CLEAR_UAV` | on (`=0` off) | `ClearUnorderedAccessViewUint` and `...Float`, which the upscaler uses, were "not implemented" in 186 of 212 runs. A correctness fix. It did not remove the white-out (a run with it on and one with it off both whited out). |
| the pipeline library keeps no pipeline with static samplers (`VICTORY_LIBRARY_STATIC_SAMPLERS=1` restores the old behaviour) | **on** (integ/validation-28) | The cause of the white-out, the red spot on the hair and the orange hair. The NVIDIA driver bakes the place of each static sampler in its own table into a stored pipeline, and the table differs between processes, so a pipeline stored by one process (`victory warm` seeds about 1,000) and loaded by the game sampled through the wrong samplers. On one build and one cache copy: old behaviour white (red bloom at 60 s, 36 to 39 % white from 130 s), fix clean in 5 of 5 pictures out to 300 s with brown hair; runs with an empty library or the library file removed clean in 15 of 15 pictures; the cache with its library whited out in 3 of 3 runs of the build before. Cost: those pipelines (about 1,300 of 2,100, 62 % of the compute and 70 % of the pixel shaders have static samplers) are compiled at every start, about 110 to 130 s of CPU-time spread over the workers; `VICTORY_SAMPLER_TABLE=1` (off until measured) removes the cost by making the static samplers sampler-heap tables. |
| `VICTORY_TITLE_TAIL` | on (`=0` off) | A view the title makes at a slot past the host's title slots is kept in a dynamic slot of the same index. The limit of 1,000,000 views is the driver's; the old warning that views past it read null was a misreading (no view was created past the limit), and is an info line now. |
| `VICTORY_XOR_UPLOAD` (and `VICTORY_XOR_READBACK`, integ/validation-23) | on (`=0` off) | Textures in the XOR tile modes (the water read-back among them) are uploaded and read back through the layout tables; the pipe and packer counts of the console are an estimate (the log prints the pair in use). Tested on the software adapter for 8 modes; the refusal "XOR mode not uploaded" did not appear in 885 runs of the prologue, so it matters later in the game. |

**Off by default**:

| switch | default | why |
|---|---|---|
| `VICTORY_KEPT_SKIPS_WIDE` | **on (safe classes) in the code of integ/validation-22, off in integ/validation-23** (`=1` the safe classes, `=all` every class) | Reverted after load-time device removals: with the wave-wide form of the kept skips on (116 programs in the form) the title lost the card while loading in 2 of about 4 starts, against none in about 7 starts of the same builds with it off. An audit found a group barrier behind a divergent branch in 91 of the 116 programs; whether that hangs NVIDIA is not proven. Cost of leaving it off: the defragmentation pass keeps its loop (7.8 ms) until a subset proven safe is found. It is in the cache key. |
| `VICTORY_LOCAL_EXIT` | off (`=1`, `=2`) | After its fix the RTX test passes (64 of 64 lanes store, 40 of 64 before), but the hair arms with it on were no different from those with it off: it does not fix the blocky hair. `=2` is too broad (it includes a particle program class that took the card down once). |
| `VICTORY_F16_NEAREST` (`=2`), `VICTORY_F32_FTZ`, `VICTORY_NAN_SAFE` | **`F16_NEAREST` 2 and `F32_FTZ` on since integ/validation-24 (2026-11-28; `=0` turns each off)**, `NAN_SAFE` off | Console-matching precision, kept; **not the white-out cure** (the cause was the pipeline library, next row). The RTX tests found the compare and min/max instructions of NaN and infinity exact on the card (0 of 144 differ); the one difference is `v_cvt_f16_f32`, which the card truncates where the console rounds to nearest (24 of 144 cases, one step), and the console flushes 32-bit denormals. `=1` fixes the convert, `=2` also the mixed and packed 16-bit ops. They were made the default at integ/validation-24 because 2 of 2 processes with them were clean, but later the white-out came back with them on in 3 of 5 processes (and was seen in 14 of 14 with them off on the same day; Fisher p about 0.06, at most a small part). On the card level 2 takes the f16 rounding from 4 of 11 cases wrong to 0, and FTZ alone changes nothing measurable. |
| `VICTORY_PIPELINE_AHEAD`, `VICTORY_PIPELINE_WIDE` | off | Cold start: 61 % of it is one frame of about 175 compute programs made one at a time. These two queue them ahead and add workers; estimated 21 -> 7-9 minutes, not measured yet. |
| `VICTORY_RESOLVE_ASYNC`, `VICTORY_PIPELINE_NOW_FIRST` | off | Cold start (COLD2): the ~175 big programs of that frame are the ray-tracing resolves; with the first switch a dispatch of one is dropped while it is made in a lane of its own (all but a quarter of the workers), as the title's own async-compile model drops a draw; the second makes what the frame on screen asked for go before older requests. Not measured yet. `victory warm --skip-resolves` leaves the resolves to the title. |
| experiments | off | `ONE_PASS`, `PAGE_REFRESH`, `PRECISE_*`, `JOIN_WAITS`, `INDIRECT_FOLD`, `FOLD_VIEWS` with `VIEW_INDEX`, `STAGE_A*`, `SEND_COPYQ`, `QUIET_COMPARE`, `ASYNC_COMPUTE` (`=2` is a second compute queue), `LANE_CORE`, `LANE_HIGH`, `LIST_NICE`, `WAIT_NICE` (live in the A/B since cpu3), `VIEW_AT_CACHE`, `KEEP_FAST`, `STATE_FILTER` (live since cpu3), `OPUS_PITCH`, `BVH_BUILDER` with `BVH_SIZES`, `R11G11B10_AS_F16`, `PRECOMPILED`: new or not yet measured (rule 14). |

## What the older documents and comments said wrong (corrected in this change)

- `ab-switches.md`: `VICTORY_LOCAL_EXIT` was "fails its RTX unit test; being fixed" (it passes since the fix, and is off because it does not fix the hair); the table of switches was cut in two by a paragraph; a control character had replaced `bin\ab-verdict.py`; `VICTORY_ALIAS_FAST` was "not in this branch" (it is, read once, off); 23 switches that `VICTORY_AB` can flip had no row (listed there now).
- `README-fxceless.md` (the 2026-11-25 status, kept as it was written): `VICTORY_LEVELS_ON_DEMAND` is on by default (`defaults.rs`), `VICTORY_PAGE_WRITES` unset reads as `guards` (on), `VICTORY_CARD_ROOM_FOR_BUFFERS` is on unless `=0`, and `VICTORY_MERGE_RACE` no longer exists. Marked in place.
- The README draft for integration 24 (and the row of this table above): it said the white-out was fixed by `VICTORY_F16_NEAREST=2` and `VICTORY_F32_FTZ=1`. It was not: it was seen again with both on, and the cause is the pipeline library's static samplers (fixed in integ/validation-28); the two switches stay on as console-matching precision.
- The README draft for integration 18: it said the ray-tracing pair prevents the white-out (white-outs and red blooms were seen with it on, in 5 of the 10 runs of that family), and that `VICTORY_LOCAL_EXIT` waits for its fix (fixed, and not the cure for the hair).
- `bug-journal.md` named the folder the game was installed in (three times; replaced). One comment in `crates/victory-hle/src/modules/files.rs` still names the game's executable in an example (code, not changed here).
- In the code, comments that still say a switch is off by default while it is on (the code was not changed): `VICTORY_ASK_ASIDE` (host.rs), `ASK_ONCE`, `CARD_AWAKE`, `CARD_ROOM_FOR_BUFFERS`, `CARD_STEPS`, `INDIRECT_WINDOW`, `JOIN_COPIES`, `LATE_WAIT`, `RELAY`, `SNAP_ASIDE`, `TEXTURE_NO_WAIT`, `WIDE_BACK`, `AMBIENT`; and `VICTORY_REFRESH_FRONT`, which is on through the default `VICTORY_JOIN_PARTS=8` though its comment says `=1`. Some of the `=1` switches turn on only for the exact value `1` (`JOIN_COPIES`, `INDIRECT_WINDOW`, `CARD_REHASH_RUNS`, `TEXTURE_NO_WAIT`, `PENDING_FLIGHT`) and `VICTORY_ROOT_SRV_DIRECT` and `VICTORY_STRICT_JOINS` turn on with any value, `0` included.

## Switches that change the translation (the shader cache key)

These are read by the translator (`crates/victory-shader`) or hand it a value, so each is in the key of the translated-shader
cache (`crates/victory-gpu/src/shader_cache.rs`, `build()`), and a build whose translator sources changed starts with a new
cache anyway (the key holds a hash of them). The two that are on by default (`VICTORY_HALF_EXIT`,
`VICTORY_KEEP_LOOP_SKIPS`) are keyed by the value in force, not by the text of the variable, as is
`VICTORY_KEPT_SKIPS_WIDE` in integ/validation-23. `VICTORY_HOST_*`, `VICTORY_RAW_VIEWS` and `VICTORY_VIEW_LIMIT` reach the
translator as the host's facts (`host_settings()`): the translate tools take them from the environment, a run takes them from
the card.


| switch | default | keyed as | purpose |
|---|---|---|---|
| `VICTORY_BINDLESS_BUFFERS` | off | cache key (text) | A program reads the SRV of a descriptor table as raw dwords (`buffer_load_dword*` without an index or format, `s_buffer_load_dword`) through the host's window of its heap slot (copy number, byte, length, in the root SRV `t249`) when the host made one, else through its view; `HostShader::bindless` names the entries only windows read. Needs `VICTORY_BINDLESS_HOST` on the host. |
| `VICTORY_BVH_BOUNDS` | on (=0 turns off) | cache key (text) | Keeps the ray-structure builder's view accesses near their views and ignores impossible header sizes. |
| `VICTORY_BVH_FENCES` | on (=0 turns off) | cache key (text) | Puts the structure builder's buffer atomics between two fences. |
| `VICTORY_BVH_MISS` | off | cache key (text) | Every ray-tracing node test answers "nothing hit" without reading the node (a box has no child, anything else is the triangle miss), so a walk ends where it began and reads no title memory; never while `VICTORY_BVH_BUILDER=1`. |
| `VICTORY_F16_NEAREST` | **2** (`=0` off, `=1` scalar ops only) since integ/validation-24; off before | cache key (value in force) | Rounds 16-bit float results to nearest, as the console does (2 also covers mixed and packed ops). Console-matching precision, not the white-out cure. |
| `VICTORY_F32_FTZ` | **on** (`=0` off) since integ/validation-24; off before | cache key (value in force) | Asks the driver for the console float mode: 32-bit denormals flushed to zero (shader model 6.2). |
| `VICTORY_FIRSTLANE_OWN` | off | cache key (text) | =1 takes the host wave's own first lane in wave-wide first-lane reads. |
| `VICTORY_FIRST_HALF` | on (=0 turns off) | cache key (text) | Removes first-half interpolants only the second half reads before lowering. |
| `VICTORY_FOLD_VIEWS` | off | cache key (text) | Folds view heap indexing into raw user data in draws and shaders. |
| `VICTORY_FUSE_DISPATCH` | off | cache key (text) | Compute programs translated as members of a run of dispatches (integ/validation-26); with it unset the emitted code is the one of integ/validation-25. |
| `VICTORY_GROUP_SYNC` | 2 (every barrier waits) | cache key (value) | Which group barriers wait for the whole group: 0 none, 1 title's only, 2 all. |
| `VICTORY_HALF_EXIT` | on (=0 turns off) | cache key (value) | Exchanging host waves clear their exchange dword when leaving a loop. |
| `VICTORY_HELPER_OWN` | on (=0 turns off) | cache key (text) | Pixel-shader helper lanes read their own value where the console reads the first lane's. |
| `VICTORY_HOST_LDS_KIB` | off | cache key (value) | Tells the translator the host allows this many KiB of group shared memory (default 32). |
| `VICTORY_HOST_MAX_WAVE` | off | cache key (value) | Translate as for a host whose waves have at most this many lanes (for example 32). |
| `VICTORY_HOST_MESH_OUTPUT_KIB` | off | cache key (value) | Tells the translator mesh shaders may output this many KiB (default 32). |
| `VICTORY_HOST_RDNA` | off | cache key (value) | Translate as for an AMD RDNA host (only the exact value 1 turns it on). |
| `VICTORY_HOST_WAVE` | off | cache key (value) | Same as the host max wave setting: translate for a GPU with waves of at most this many lanes. |
| `VICTORY_KEEP_LOOP_SKIPS` | on (=0 turns off) | cache key (value) | Keeps the early-outs and skips over loops in the translated shader. |
| `VICTORY_KEPT_SKIPS_WIDE` | on, safe classes at integ/validation-22 (=0 off, =all every class); off unless =1 or =all in integ/validation-23 | cache key (text at integ/validation-22, value in force at integ/validation-23) | Takes the kept loop skips in their wave-wide form (with VICTORY_KEEP_LOOP_SKIPS); a hang suspect, off in integ/validation-23. `=audited` (integ/validation-27, fix/defrag1, with the hang guard on): the form is audited as the program has it, then made wide. |
| `VICTORY_KEPT_SKIPS_ONLY` | off | cache key (text) | (integ/validation-27, fix/defrag1) With `VICTORY_KEPT_SKIPS_WIDE=audited`: the comma list of program tags (seven hex digits) the audited kept skips' form is taken for; the others keep the old form. Env only. |
| `VICTORY_LEGACY_ASSUME` | off | cache key (text) | Writes legacy multiplies as the host product where no factor can be infinity or NaN. |
| `VICTORY_LOCAL_EXIT` | off | cache key (text) | Translates console wave64 loops as two host waves of 32 that leave together (=1 screen programs, =2 broader). |
| `VICTORY_KEEP_STALLING_SKIPS` | off | cache key (text) | `=1`: in the forms without skips, a skip over a loop that never ends once no lane is left in it stays a branch (the water's tiled light on 32-lane hosts: tiles with no output). |
| `VICTORY_LDS_FIRST_INIT` | off | cache key (text) | `=1`: a thread group's shared-memory set-up by its first thread (constants under a test of the thread's place) is done whether that thread's lane runs there or not (the water's tiled light on 32-lane hosts). |
| `VICTORY_LOOPS_WAVE` | off | cache key (text) | `=1` (`=2` every compute program): a console wave64 program that needs the host's waves is translated again with the branches that close or leave its loops taken by each host wave together (the water's tile generation on 32-lane hosts). |
| `VICTORY_LOOPS_WAVE_ONLY` | off | cache key (text) | With `VICTORY_LOOPS_WAVE`: only the programs a word of whose container is one of these comma-separated names (for example the water library's) take the loops together. |
| `VICTORY_LOOP_LIMITS` | off | cache key (text) | Table of per-program, per-stage iteration limits for translated loops. |
| `VICTORY_LOOP_NOTES` | off | cache key (text) | Translated loops that hit their iteration limit leave a note in the faults buffer. |
| `VICTORY_LOOP_WHY` | off | cache key (text) | Prints one stderr line per translated loop, saying why it has its limit. |
| `VICTORY_MASK_NUMBERS` | on (=0 turns off) | cache key (text) | In the retry translation of a program refused for a mask read as data, allow mask numbers. |
| `VICTORY_MASK_WHY` | off | translator reads it, does not change the output | Prints why a mask has no value of its own, for finding mask refusals. |
| `VICTORY_NAN_SAFE` | off (0 = no bits) | cache key (text) | Set of bits (decimal or 0x hex) choosing where min/max and compares keep NaN-safe behaviour. |
| `VICTORY_PAIR_CLAMP` | on (=0 turns off) | cache key (text) | Bound the shared-memory access of paired host waves with a clamp. |
| `VICTORY_PAIR_LEAN` | on (=0 turns off) | cache key (text) | Use the cheaper wave-pair meetings instead of two meetings per access. |
| `VICTORY_PAIR_SYNC` | on (=0 turns off) | cache key (text) | Allow wave-pair meeting waits with own counters; off refuses the forms needing them. |
| `VICTORY_PEEK` | off | cache key (text) | Program hash and code offsets whose values are left as notes for debugging. |
| `VICTORY_RAW_VIEWS` | on (=0 turns off) | cache key (value) | Keep a raw twin of every buffer view so shaders picking buffers by computed index can read them. |
| `VICTORY_RDNA_FLOAT` | on, all bits | cache key (value) | Set of bits (decimal or 0x hex) of RDNA float behaviour used when the host is an RDNA card. |
| `VICTORY_SHADER_CENSUS` | off | translator reads it, does not change the output | Appends a line of instruction counts per emitted program to the given file. |
| `VICTORY_SHADER_LOOP_LIMIT` | none (the stages' own limits) | cache key (value) | Forces one iteration limit on every translated loop; decimal or 0x hex, at least 1. |
| `VICTORY_SHADER_OPT` | 63 (every clean-up pass but bit 64) | cache key (value) | Picks which clean-up passes run over translated shader code; 0 none, else a pass bitmask. |
| `VICTORY_SHADER_SOURCE_HASH` | n/a | translator reads it, does not change the output | Compile-time hash of the translator source set by build.rs, embedded with env!. |
| `VICTORY_SHADER_SWITCHES` | n/a | translator reads it, does not change the output | Compile-time list of the VICTORY_ names found in translator source, set by build.rs. |
| `VICTORY_STRICT_JOINS` | off | cache key (text) | Marks joins read through other joins strictly when lowering, instead of the shortcut. |
| `VICTORY_UNIFORM_LOADS` | 0 | cache key (text) | Treats loads at lane-uniform addresses as alike in every lane (1 LDS reads, 2 buffer loads too). |
| `VICTORY_VIEW_INDEX` | off | cache key (text) | Root SRVs are read through the view heap by an index placed in the user data. |
| `VICTORY_VIEW_LIMIT` | n/a (host probes the heap limit) | cache key (value) | Tool parameter: translates as on a host whose view heap has this null slot. |
| `VICTORY_WAVE_DEEP_SKIPS` | on (=0 turns off) | cache key (text) | Wave-wide form that drops skips with scalars followed, so more code runs in every lane. |
| `VICTORY_WAVE_LOCAL` | on (=0 turns off) | cache key (text) | Gives each host wave its own wave for programs not run by every lane; =idiom refuses approximate ones. |
| `VICTORY_WAVE_OWN_BITS` | on (=0 turns off) | cache key (text) | Allows a wave OR where not every lane runs, using each lane's own bits. |
| `VICTORY_WAVE_WHY` | off (any value prints) | translator reads it, does not change the output | Prints to stderr why a program keeps 64-lane waves and why waterfall checks failed. |
| `VICTORY_WAVE_WIDE` | off for most programs, on for two named kernels (=1 forces on, =0 off) | cache key (text) | Translates console waves run as two host waves in the wave-wide form with barriers in branches. |
| `VICTORY_WAVE_WIDE_AUTO` | off (=1 every program, or a comma list of program names) | cache key (text) | Takes the wave-wide form before "without skips, scalars followed" where the forms before it stopped at an exchange of the two host waves that only a deep-only skip keeps from some lane (the cascade-shadow rasters); refused where it puts a translator group barrier in a loop. |
| `VICTORY_WAVE_WIDE_FIRST` | a built-in list of one program | cache key (text) | Comma list of program names that take the wave-wide form before any approximate local form. |

**What the guard test sees and what it does not.** `every_switch_the_translator_reads_is_in_the_cache_key` (in
`shader_cache.rs`, run by `cargo test` in CI) reads every `.rs` file of `crates/victory-shader/src` whose name has no `test`
in it, and fails when a string literal `"VICTORY_..."` is neither in the text of `shader_cache.rs` before its tests nor in
its two hand lists (`NOT_THE_TRANSLATION`, `ELSEWHERE`). On this tree it reports nothing missing: the 45 names the
translator reads are all keyed or listed, and none passes only because the name appears somewhere else in `shader_cache.rs`.
What it cannot see:

- A switch read in another crate that changes what the translator is given. Today these are the host's facts
  (`set_host_max_wave`, `set_host_rdna`, `set_host_lds_bytes`, `set_host_mesh_output_bytes`, `set_raw_views`,
  `set_view_heap_limit`), which `host_settings()` keys by value; a seventh setter added without a slot in `host_settings()`
  would not be noticed.
- The four names in `ELSEWHERE` (`VICTORY_GROUP_SYNC`, `VICTORY_RDNA_FLOAT`, `VICTORY_SHADER_LOOP_LIMIT`,
  `VICTORY_SHADER_OPT`) are trusted by the list alone: the test does not check that `group_sync()`, `shader_opt()`,
  `loop_limit()` and `host_settings()` are still in the key. The twelve of `NOT_THE_TRANSLATION` are trusted the same way
  (a switch put there by mistake is never keyed).
- The two switches keyed by value are paired by `zip` between a name list in `shader_cache.rs` and
  `default_on_switches()`: a third switch added to one list and not the other is silently dropped from the key, and the
  test still passes (the name is in the file).
- Names built at run time, a prefix scan of the environment, and files of the translator crate with `test` in their name.
  There is none of the first two today; the test files read only test variables.
- `victory-shader/build.rs` writes `VICTORY_SHADER_SWITCHES` (every name the crate reads) and `emit::translation_fingerprint()`
  would hash their values, but nothing calls it: the cache uses the hand list and the guard test instead. Using it would make
  the list automatic.
- A switch that changes which programs run but not their translation (`VICTORY_BVH_BUILDER`, `VICTORY_RT_RESOLVE`,
  `VICTORY_ACTIVATE_RAYS`) is correctly not in the key; `victory warm` keeps a signature of what it left out instead.

## Switches `VICTORY_AB` can flip while the title runs

Read through `live_flag!`, `live_value!`, `toggled!` or `ab::pick` (diagnostics build). Name a switch that is on by default
with its off value (`VICTORY_AB=10,VICTORY_LEAN_STEPS=0`). The column "row" says whether [ab-switches.md](ab-switches.md)
has a row that argues why a flip at any instant is safe; where it has none, the argument is in the doc comment where the
switch is read.


| switch | default | row in ab-switches.md | purpose |
|---|---|---|---|
| `VICTORY_ALIAS_BATCH` | off | yes | The other views of a run of blocks the write guards make read-only are guarded by one change of protection for each run of them side by side. |
| `VICTORY_AMBIENT_GAIN` | 1 | no | Scales the light the indirect-light stand-in puts in (share, floor and constant); 1 changes nothing. |
| `VICTORY_ASK_ASIDE` | on (=0 turns off) | yes | Offers the write-watch asking to a side thread and joins it where it used to ask. |
| `VICTORY_ASK_CACHE` | off | yes | Lets a thread remember more write-watch answers, including windows outside buffers. |
| `VICTORY_ASK_EARLY` | off | yes | Milliseconds between rounds that ask early about pages the title wrote lately; unset is off. |
| `VICTORY_ASK_GAP_MB` | 8 | yes | Gap in MiB under which ranges of one submission are asked about together. |
| `VICTORY_ASK_MERGE` | on (=0 turns off) | yes | Asks the write watch about all ranges in one sorted pass instead of per range. |
| `VICTORY_ASYNC_COMPUTE` | off | yes | Runs the title's compute lists on a compute queue (stage 1 or 2; 2 needs submit line, relay). |
| `VICTORY_ASYNC_SUBMIT` | on (via VICTORY_THIN default 1) | no | Prepares and sends submissions on a separate thread of ours instead of the title's thread. |
| `VICTORY_CARD_AWAKE` | on, 1 (`=0` off; named in `VICTORY_AB` it is off in the phases without its bit) | yes | Keeps the card's clocks up between submissions (integ/validation-26: live). |
| `VICTORY_CARD_BIG_FIRST` | on (=0 turns off) | no | Big title buffer copies come first: the texture queue keeps room free for them. |
| `VICTORY_CLOSED_IDLE` | on (=0 turns off) | yes | Whether an idle host command list is kept closed instead of open as a reset leaves it. |
| `VICTORY_COPY_BESIDE` | off | no | Copies of title memory put bytes beside the write guards. |
| `VICTORY_DROP_EARLY` | off | yes | Leaves out calls that only feed dispatches the host drops. |
| `VICTORY_DSP_CONVOLVE` | off | no | Computes the audio DSP's mono 1024-frame convolutions (TVs, phone calls, reverb) from the title's own impulse responses; those streams were silence (`VICTORY_DSP_REVERB` stands in only where this does not apply). From VIctory-NVIDIA#77 (Yve), with a faster convolver. |
| `VICTORY_DSP_REVERB` | off | no | Runs convolution audio streams through a stand-in algorithmic reverb. |
| `VICTORY_DSP_REVERB_LEVEL` | 1.0 | no | Output level of the stand-in reverb. |
| `VICTORY_FAULT_AROUND` | 0 (off) | yes | A write fault opens, with its block, up to n-1 hot blocks beside it (opened by a fault this frame or the last) in one change of protection; 1 only batches the blocks an opening has. |
| `VICTORY_FAULT_SPIN` | 0 (off) | yes | Microseconds a title thread in the write-fault handler looks for a busy guard stripe before it sleeps on it. |
| `VICTORY_FOLD_VIEWS` | off | yes | Folds view heap indexing into raw user data in draws and shaders. |
| `VICTORY_FUSE_RUNS` (needs `VICTORY_FUSE_DISPATCH=1` at translation) | off | yes | The dispatches of one program that follow one another are one table and one dispatch (integ/validation-26). |
| `VICTORY_GPU_RO_SKIP` | off | yes | Pages the title asked for GPU-read-only are left out of the ranges shaders are declared to write (integ/validation-26). |
| `VICTORY_GUARD_REST` | on, 3000 (=0 turns off) | no | Microseconds guard takers rest; value must be above 1. |
| `VICTORY_GUARD_SHARDS` | 0 (one lock) | yes | Number of stripes the write guards are divided in, at most STRIPES. |
| `VICTORY_HEAPS_EARLY` | off | yes | Asks for a descriptor round ahead of a submission's preparing. |
| `VICTORY_HEAP_SYNC_BACKOFF` | 2 | yes | Doublings of the wait between early sync rounds after quiet rounds. |
| `VICTORY_HOLDER_INDEX` | off | yes | Finds the writers meeting a copy range through an address index instead of scanning. |
| `VICTORY_INDIRECT_FOLD` | off | yes | Folds a run of indirect executions into one job. |
| `VICTORY_JOIN_PARTS` | on, 8 | no | How many list parts are joined into one host submission; 1 turns joining off. |
| `VICTORY_JOIN_RAMP` | off | yes | After a present, lets joined parts ramp up in number instead of taking the full count. |
| `VICTORY_JOIN_WAITS` | off | yes | Takes along a part behind a wait met in a way not seen before. |
| `VICTORY_JOIN_WIDE_MB` | off | yes | Joins parts whose upload fills read up to this many MiB; 0 refuses such parts. |
| `VICTORY_KEEP_SHOWN` | 1 | no | When a slot gets a view of another texture: 0 show nothing, 1 keep a similar one, 2 any same-dimension one. |
| `VICTORY_LAND_AHEAD` | on (=0 turns off) | yes | The landing merge asks for its bytes ahead (the card's, the copy's, title memory: 1 KiB in front, and the head of the next piece); same bytes, fewer stalls. |
| `VICTORY_LAND_ASIDE` | on (=0 turns off) | yes | Lands a submission's lists aside, on another thread, saving time on the submitting thread. |
| `VICTORY_LAND_ECHO` | on (=0 turns off) | no | Compares pages 4 KiB at a time and sends up only those that differ from what landed. |
| `VICTORY_LAND_OWN` | off | yes | Leaves the pieces of shader-written copies that nothing but the title's CPU can read on the card: guarded, landed only when something reads or writes them (integ card propagation, perf/card-propagate). |
| `VICTORY_LAND_OWN_UNTAKEN` | off | yes | With VICTORY_LAND_OWN, also keeps pieces that other copies lie over when none of those copies ever took their bytes. |
| `VICTORY_LAND_TAIL_THREADS` | off | yes | Threads for the tail of a landing; 0 uses the ordinary landing threads (max 16). |
| `VICTORY_LANE_CORE` | off | yes | Pins the lane thread to one logical processor of one core and moves others off it. |
| `VICTORY_LANE_HIGH` | off | yes | Gives the lane thread the highest priority of the process. |
| `VICTORY_LEAN_CARD` | on (=0 turns off) | yes | Finds copies whose CPU side changed through a queue instead of looking at all of them. |
| `VICTORY_LEAN_STEPS` | on (=0 turns off) | yes | Holds each buffer once for all steps of a table instead of once per step. |
| `VICTORY_LEVELS_RETRY` | on (=0 turns off) | yes | Lets a level-capped texture retry upload when room appears; =0 brings the old rule back. |
| `VICTORY_LIST_NICE` | off | yes | Runs list-translating threads this many priority levels below the guest's; 0 is normal. |
| `VICTORY_LIST_VIEWS` | on (=0 turns off) | no | Makes a view its tables hold once per list instead of once per table. |
| `VICTORY_MAKE_ASIDE` | on (=0 turns off) | yes | Makes a buffer's copy aside from its lock; =0 makes it under the lock. |
| `VICTORY_ONE_CALL` | off | yes | A submission joined of several parts is given to the queue in one `ExecuteCommandLists` call (integ/validation-26). |
| `VICTORY_ONE_PASS` | off | yes | Prepare a submission from one pass over the steps instead of four walks. |
| `VICTORY_OPEN_ONE_VIEW` | on (=0 turns off) | no | Open only the guarded block a write faults on instead of all views of it. |
| `VICTORY_OPUS_PITCH` | off | no | Resample audio packets to the pitch the title asks for. |
| `VICTORY_PAGE_REFRESH` | off | yes | Refreshes driven by pages the title wrote: 1 shadow mode, 2 driven mode. |
| `VICTORY_PIPELINE_BOOST` | off | yes | Raise the level of pipelines being made, per pipeline at its time. |
| `VICTORY_PIPELINE_NOW_FIRST` | off | yes | Of the requested pipelines a worker takes first one a draw of this frame or the one before asked for (also in the resolves' lane); else the oldest. |
| `VICTORY_PIPELINE_PREPARE` | off | yes | Begin the next submission refreshes on another thread while the current one is sent. |
| `VICTORY_PIPELINE_PRIORITY` | on (=0 turns off) | yes | Pipelines the title waits on or touches go first in the queue. |
| `VICTORY_PIPELINE_RAM_FREE_MB` | 1536 (0: no limit) | no | The free physical memory below which an extra pipeline worker or a ray-tracing resolve starts no new compile. |
| `VICTORY_PIPELINE_WAIT_ADAPT` | off | yes | Let the per-frame pipeline wait budget follow how the waits end. |
| `VICTORY_PLACED_CONSTANTS` (needs `VICTORY_COPIES_IN_PLACE`, on) | off | yes | A constant buffer that reads bytes the list's own placed copy wrote is snapshotted on the card (integ/validation-26). |
| `VICTORY_PLACED_ORDER` | off | no | Order placed copies by where they stand in title memory. |
| `VICTORY_PRECISE_BARRIERS` | off | yes | Hold the title UAV waits as precise barriers. |
| `VICTORY_PRECISE_LOOK` | off | yes | Make and keep precise write ranges beside the declarations. |
| `VICTORY_PRECISE_WRITES` | off | yes | Declare buffer views as the precise ranges a shader writes. |
| `VICTORY_PROP_COUNT` | off | yes | Counts per frame the bytes landed GPU to title memory, refreshed title memory to copies and their overlap, with CPU estimates (a diagnostics line). |
| `VICTORY_QUICK_FIND` | on (=0 turns off) | no | Keep the table of found buffers. |
| `VICTORY_QUIET_COMPARE` | off | yes | Compare copies against the CPU side and note only what differs. |
| `VICTORY_QUIET_COPIES` | on (=0 turns off) | no | Keep working buffers of upscalers from being copied whole at every refresh. |
| `VICTORY_RANGE_VALID` | off | yes | Validate copies by the log of changes instead of counts. |
| `VICTORY_READY_ONCE` | on (=0 turns off) | no | Skip entries already found ready instead of going through all of them at every draw. |
| `VICTORY_REFRESH_NARROW` | off | yes | Splits each copy's refreshes into narrow spans separated by gaps (1 = 64 KiB, else that many KiB). |
| `VICTORY_REFRESH_NOTES` | on (=0 turns off) | no | Skips re-looking at ranges whose write stamps stand and whose bits are set. |
| `VICTORY_RESIDENT_BATCH` | on (=0 turns off) | yes | Makes residency calls for a batch of objects together instead of one call or round trip each. |
| `VICTORY_RESOLVE_ASYNC` | off | no | The ray-tracing resolves (`CS_RaytracingResolve`) are made in a lane of their own and a dispatch of one is dropped, never waited for, while its pipeline is made. Not with the structure builder. |
| `VICTORY_RESOLVE_WORKERS` | all but a quarter of the workers | no | How many workers make ray-tracing resolves at once with `VICTORY_RESOLVE_ASYNC` (`0`: every one). |
| `VICTORY_SAME_RECORDS` | on (=0 turns off) | no | Skips writing records into title memory that already hold the same bytes. |
| `VICTORY_SEND_COPYQ` | off | yes | Sends submission bytes over the bus on a copy queue. |
| `VICTORY_SNAP_ASIDE` | on (=0 turns off) | yes | Gives snapshots to kept threads and waits for them before sending, saving about 3.5 ms. |
| `VICTORY_STAGE_A` | off | yes | Offers the next submission's refreshes before the steps of the one being prepared. |
| `VICTORY_STAGE_A_DEEP` | off | yes | (integ/validation-23) Also refreshes the copies of the entry after the next while the next is prepared. |
| `VICTORY_STAGE_A_LOOKAHEAD` (with `VICTORY_STAGE_A_STEPS`) | off | yes | (integ/validation-27) Stage A prepares the oldest submission of the line whatever its queue, past the signals and met waits that stand before it on its own queue (it used to look only at the first of the line). |
| `VICTORY_STAGE_A_RELAX` | 0 (off) | yes | (integ/validation-23) Bits of what stage A's thread may also do: note-only host steps, reads, planes, bounded waits for flights. |
| `VICTORY_STAGE_A_STEPS` | off | yes | Has a thread of ours prepare the next submission's whole front part while the previous one is sent. |
| `VICTORY_STAGING_HOLD_MS` | 0 (off) | yes | Milliseconds an empty texture-staging block is held before being let go. |
| `VICTORY_STEPS_MERGE_READS` | off | yes | Records each list's reads in one go, whole or joined. |
| `VICTORY_STORE_BESIDE` | off | no | Writes title stores to a second place beside the title memory. |
| `VICTORY_SUBMIT_FRAMES` | 1 | yes | How many presents may be in the line when a present returns. |
| `VICTORY_TAKE_PARTS` | on (=0 turns off) | no | Takes only the parts of a page written since last taken instead of the whole page. |
| `VICTORY_UNDER_TRY` | off | yes | Copies whose locks are free go first, copies a landing holds go last. |
| `VICTORY_UNDER_WIDE` | on (=0 turns off) | no | Compares pages of merged under-writes on several threads instead of the submitting thread. |
| `VICTORY_UNWATCHED_FRAMES` | 1 | yes | Refreshes copies of memory that is not write-watched every N frames; 1 is every frame. |
| `VICTORY_UPLOAD_AHEAD` | on (built-in default 1; =0 turns off) | no | Reads, detiles and copies texture uploads ahead on threads of ours. |
| `VICTORY_UPLOAD_HELD_ONLY` | off | yes | Upload copies only the texture levels the title holds. |
| `VICTORY_VIEW_AT_CACHE` | off | yes | A thread answers the view a record names from a table of its own (live in the A/B). |
| `VICTORY_VIEW_CACHE` | 0 (off) | yes | Keeps buffer views from one recording to the next; 2 also compares each with one made again. |
| `VICTORY_WAIT_ORDER` | off | yes | Gives a flush's copy refreshes to the threads costliest first. |
| `VICTORY_WIDE_SPIN` | 0 | yes | Microseconds a wide wait spins awake before sleeping; 0 does not spin. |
| `VICTORY_WIDE_WHOLE` | on (=0 turns off) | no | Copies a long unwatched range whole on several threads. |

## Added with the VIctory-NVIDIA port (2026-11-28, `feat/nv1-dlss-sr`)

DLSS and frame generation come from the VIctory-NVIDIA fork (trustno1-coder, readmet3xt-lab, DexNom; `docs/dlss.md` there). All off unless asked; none changes the
translation of shaders.

| Switch | Default | Kind | Translation | A/B | Cache | What it is for |
|---|---|---|---|---|---|---|
| `VICTORY_DLSS` | off | flag |  |  |  | `1`: DLSS Super Resolution does the title's FidelityFX upscaling (its `ffxDispatch`), from the same inputs into the same output. Needs NVIDIA's files (`VICTORY_STREAMLINE_DIR`); the title renders through its FidelityFX path (the image is patched, cached in `images\upscaler-<mode>`). |
| `VICTORY_DLSS_MODE` | quality (dlaa with frame generation alone) | text |  |  |  | dlaa, quality, balanced, performance, ultraperformance: the scene's size, 1, 2/3, 0.58, 1/2, 1/3 of the output. |
| `VICTORY_DLSS_PRESET` | default | text |  |  |  | The DLSS model: default, j, k, l, m. |
| `VICTORY_DLSS_JITTER`, `VICTORY_DLSS_MV` | `1,1` | text |  |  |  | Multipliers for the jitter and the motion-vector scale DLSS is given (sign and scale checks). |
| `VICTORY_DLSS_REACTIVE` | on (=0 turns off) | flag |  |  |  | `0`: the title's reactive mask is not given to DLSS. |
| `VICTORY_FRAMEGEN` | off | flag |  |  |  | `1`: DLSS Frame Generation on our present (RTX 40 and later, hardware-accelerated GPU scheduling on); needs `VICTORY_THIN=1` or `VICTORY_ASYNC_SUBMIT=1` and `VICTORY_DLSS=1`. About 0.5 GB of video memory more at 1440p. |
| `VICTORY_FRAMEGEN_X` | 2 | number | | | | Frames shown for each one rendered, 2 to 6 (more than 2 on RTX 50). |
| `VICTORY_FRAMEGEN_HUDLESS` | on (=0 turns off) | flag |  |  |  | `0`: no colour without the interface is copied or tagged on Series X (generated frames warp the interface). Needs `VICTORY_INTERFACE_COMPOSITE`; the copy is recorded in front of that draw in the title's list (`draw::State::before_shown_target`, one copy a list) and tagged at the present that follows its submission. |
| `VICTORY_INTERFACE_COMPOSITE` | unset | text |  |  |  | `<vertex hash>,<pixel hash>` of the title's interface composite pipelines (Series X): the draw in front of which the colour without the interface is copied. Unset: none is copied. The hashes are not in the repository. |
| `VICTORY_FRAMEGEN_CYCLE`, `VICTORY_FSR_FRAMEGEN`, `VICTORY_FSR_FRAMEGEN_DEBUG`, `VICTORY_FIDELITYFX_DIR` | off | flag/text |  |  |  | AMD FSR 3 frame generation for cards without DLSS frame generation (module `streamline/fsr.rs`; its host hooks are not ported yet: only the module is there). |
| `VICTORY_FSR_PATH` | off | flag |  |  |  | `1`: the title's FidelityFX path at `VICTORY_DLSS_MODE`'s scale with its own FidelityFX upscaling and no Streamline: the comparison for DLSS. |
| `VICTORY_NATIVE_1440P`, `VICTORY_RENDER_1080P` | off | flag |  |  |  | Series X retail image at a 2560x1440 output (scene 2560x1440, or 1920x1080 scaled up by the title): the image is patched before it is mapped and cached apart (`native_1440p.rs`). The frontend's legacy 4K-projection triangle gets the 3840x2160 viewport on the 1440p target (`draw::State::native_frontend_viewport`; known by its shape, and by `VICTORY_INTERFACE_COMPOSITE` too when that is set). |
| `VICTORY_HOTKEYS` | off | flag |  |  | diag | `1`: with one of our windows in front and Ctrl and Shift held, F5 frame generation off/on (at the start's amount, 2 if none), F6 the colour without the interface off/on, F7 the next DLSS model preset (default, j, k, l, m), F8 a mark in the log; each press is a line `hotkeys phase N: ...` with the seconds since it began and the clock's time. No key is taken from the title. F3 (the stats panel) says frame generation, frames shown a second, the HUD-less copy and the model while they are in the run. The DLSS mode is not live (the title's scene size is fixed by its mapped image). See `hotkeys.rs`. |
| `VICTORY_MENU_CONSOLE` | unset | text |  |  |  | `series-s`: the title's Series S menu mode for the DLSS scene size. |
| `VICTORY_STREAMLINE_DIR` | `C:\dev\sdk\streamline-sdk-v2.14.16\bin\x64` (read only, normally absent) | path |  |  |  | The folder of `sl.interposer.dll`, its plugins and `nvngx_dlss.dll`, `nvngx_dlssg.dll` (NVIDIA's, never in the repository). Use a folder on G:. |
| `VICTORY_STREAMLINE_LOG` | unset | path |  |  |  | A folder for Streamline's own verbose log files. |
| `VICTORY_STREAMLINE_PRESENT` | off | flag |  |  |  | `1`: DLSS without frame generation with our swap chain as Streamline's proxy of it (no "presentCommon() was not observed"). |
| `VICTORY_CHECK_TEARDOWN` | on (=0 turns off) | flag |  |  |  | `victory upscaler-check`: shut Streamline down at the end. |
| `VICTORY_LEAVE_OUT` | unset | text |  |  |  | `<16 hex digits>[,..]`: pipelines whose shader has one of these hashes are left out in every build (the VIctory-NVIDIA fork leaves out the decal heap's relocation program, 11.85 ms of the card a frame there, and the heat-haze pass). Part of the warm book's signature. |

## All switches, alphabetical


| switch | default | kind | key | A/B | build | purpose |
|---|---|---|---|---|---|---|
| `VICTORY_AB` | off | list |  |  | diag | Names switches to flip between phases inside one run for in-process A/B timing. |
| `VICTORY_AB_BUNDLE` | off | flag |  |  | diag | Alternates between all named A/B switches off and all on, instead of every combination. |
| `VICTORY_AB_SCREEN` | off | flag |  |  | diag | Up to 31 named A/B switches follow Hadamard rows and their complements; the log gives each switch's effect on the frame time from one run. |
| `VICTORY_AB_SHOTS` | 0 (no pictures) | number |  |  | diag | Saves a picture of every phase of every Nth A/B cycle (needs VICTORY_CAPTURE). |
| `VICTORY_AB_SKIP` | 1 (second) | number |  |  | diag | Seconds at the start of each A/B phase whose frame times are left out. |
| `VICTORY_AB_START` | unset: first phase at the first present | number |  |  | diag | Seconds after the world is found before the first A/B phase begins. |
| `VICTORY_ACTIVATE_RAYS` | on (=0 turns off) | flag |  |  |  | Runs the title's probe-ray activation compute program; 0 leaves that program out. |
| `VICTORY_ALIAS_BATCH` | off | flag |  | live |  | The other views of a run of blocks the write guards make read-only are guarded by one change of protection for each run of them side by side. |
| `VICTORY_ALIAS_COPY` | on (=0 turns off) | number |  |  |  | Finds the texture aliasing a title resource's memory; 0 none, 1 only single-level single-layer textures. |
| `VICTORY_ALIAS_FAST` | off | flag |  |  |  | Lets page-by-page remapping skip pages that already show the wanted page and do small groups itself. |
| `VICTORY_AMBIENT` | on, 0.8 (0/off turns off) | text |  |  |  | Stand-in for the title's missing indirect light: share of the estimate, optional floor or constant. |
| `VICTORY_AMBIENT_AIR` | 1.5 | text |  |  |  | Multiple of the air's scattering for the stand-in light, with optional max:level cap. |
| `VICTORY_AMBIENT_GAIN` | 1 | number |  | live |  | Scales the light the indirect-light stand-in puts in (share, floor and constant); 1 changes nothing. |
| `VICTORY_AMBIENT_SHADE` | 1.0,0.6 | list |  |  |  | Strength and radius in metres of the creases' shade in the stand-in light. |
| `VICTORY_AMBIENT_TRUST` | 4 | number |  |  |  | Bounds what the title's own program left by this multiple of the estimate; 0 drops it. |
| `VICTORY_ASK_ASIDE` | on (=0 turns off) | flag |  | live |  | Offers the write-watch asking to a side thread and joins it where it used to ask. |
| `VICTORY_ASK_CACHE` | off | flag |  | live |  | Lets a thread remember more write-watch answers, including windows outside buffers. |
| `VICTORY_ASK_EARLY` | off | number |  | live |  | Milliseconds between rounds that ask early about pages the title wrote lately; unset is off. |
| `VICTORY_ASK_GAP_MB` | 8 | number |  | live |  | Gap in MiB under which ranges of one submission are asked about together. |
| `VICTORY_ASK_MERGE` | on (=0 turns off) | flag |  | live |  | Asks the write watch about all ranges in one sorted pass instead of per range. |
| `VICTORY_ASK_ONCE` | on (=0 turns off) | flag |  |  |  | A submission asks the kernel once about every range its lists refresh, before the first refresh. |
| `VICTORY_ASK_TOKENS` | off | flag |  |  |  | Lets a submission's write-watch asking stand beside another's, ground for preparing two submissions at once. |
| `VICTORY_ASYNC_COMPUTE` | off | number |  | live |  | Runs the title's compute lists on a compute queue (stage 1 or 2; 2 needs submit line, relay). |
| `VICTORY_ASYNC_SUBMIT` | on (via VICTORY_THIN default 1) | flag |  | live | diag | Prepares and sends submissions on a separate thread of ours instead of the title's thread. |
| `VICTORY_AUDIO_CAPTURE` | off | path |  |  |  | Records what the title plays to a wav file through a child process loopback recorder. |
| `VICTORY_AUDIO_CAPTURE_OF` | n/a | number |  |  |  | Internal: process id the recorder child is told to record, set by the parent. |
| `VICTORY_AUDIO_STATS` | off | number |  |  |  | Logs a line every n seconds with counts of what the XMA audio decoder did. |
| `VICTORY_BAKE_EXACT` | on (=0 turns off) | flag |  |  |  | Makes the title's texture-bake draws see textures exactly as title memory holds them now. |
| `VICTORY_BAKE_TEXTURES_MS` | 1500 | number |  |  |  | Milliseconds the texture-upload pass may take just before the title's decoration bake draw. |
| `VICTORY_BETWEEN_FIT` | off | flag |  |  |  | (integ/validation-27, VRAM2) The buffer `between` of a list of the card-steps pool is the size its plans need (at least 256 KiB, a power of two, at most the 4 MiB cap) instead of always 4 MiB; the pool hands back the smallest buffer that is big enough. Est. 80 to 160 MB of the card. Read once. |
| `VICTORY_BARRIER_LOOK` | off | flag |  |  |  | Reads the title's barrier groups and counts which need a wait for earlier shader writes. |
| `VICTORY_BARRIER_TEST` | off | text |  |  | diag | Timing experiment: leaves out barrier waits for groups, indirect passes or both (values groups, indirect, both). |
| `VICTORY_BINDLESS_CENSUS` | off | path |  |  |  | Tool (`translate`): appends a line for each program that has table entries only windows read (`VICTORY_BINDLESS_BUFFERS`). |
| `VICTORY_BINDLESS_HOST` | 0 | number |  | live |  | The host's side of `VICTORY_BINDLESS_BUFFERS`: 1 writes the window of each buffer view beside the view (a table of windows is made for shaders translated with the switch whatever the level); 2 also leaves unmade, in the tables made for a draw, the views its program reads through windows alone. |
| `VICTORY_BIND_CACHE` | on (=0 turns off) | flag |  |  |  | Skips re-setting render targets and depth on the host list when they are unchanged. |
| `VICTORY_BOOK` | on (=0 turns off) | flag |  |  | diag | Keeps a record of the title's memory objects to explain device faults; 0 keeps none. |
| `VICTORY_BUFFER_NAN` | off | text |  |  | diag | (integ/validation-23, diag) Scans buffers the watched compute programs write for NaN and infinities: `all` or `@<list file>`. |
| `VICTORY_BUFFER_NAN_EVERY` | 5 | number |  |  | diag | (integ/validation-23, diag) Scan every Nth frame. |
| `VICTORY_BUFFER_NAN_MAX` | 4000 | number |  |  | diag | (integ/validation-23, diag) Most watched dispatches scanned. |
| `VICTORY_BUFFER_PROBE` | off | list |  |  |  | Hex addresses of title buffers whose title memory and copy contents are logged now and then. |
| `VICTORY_BUFFER_WHOLE` | off | list |  |  |  | Hex addresses of title buffers taken whole from title memory at every refresh (experiment). |
| `VICTORY_BURY_LAG` | 64 (16 on a PC with 32 GiB or less) | number |  |  |  | Submissions that must finish before a released object or reset host list is dropped. |
| `VICTORY_BINDLESS_BUFFERS` | off | flag | yes |  |  | A program reads the SRV of a descriptor table as raw dwords through the host's window of its heap slot when there is one (`VICTORY_BINDLESS_HOST`), else through its view. |
| `VICTORY_BVH_BOUNDS` | on (=0 turns off) | flag | yes |  |  | Keeps the ray-structure builder's view accesses near their views and ignores impossible header sizes. |
| `VICTORY_BVH_BUILDER` | off | flag |  |  |  | Runs the title's ray-tracing structure builder on the host; otherwise its dispatches are dropped. |
| `VICTORY_BVH_FENCES` | on (=0 turns off) | flag | yes |  |  | Puts the structure builder's buffer atomics between two fences. |
| `VICTORY_BVH_MISS` | off | flag | yes |  |  | Every ray-tracing node test misses without reading the node, while the structure builder is off; the programs that walk structures then read no title memory. |
| `VICTORY_BVH_QUARANTINE` | off | flag |  |  |  | Keeps what the structure builder's stages store in host copies, not copied back to title memory. |
| `VICTORY_BVH_SIZES` | off | text |  |  |  | How compacted structure sizes the title reads are handled: off, wait, predict or both. |
| `VICTORY_BVH_SIZES_WAIT` | off | flag |  |  |  | Older switch: waits on the card for every compacted-size ask; same as VICTORY_BVH_SIZES=wait. |
| `VICTORY_BVH_TRACE` | off | flag |  |  | diag | Logs the sizes the structure builder is asked for and when title memory has them. |
| `VICTORY_CACHE_DIR` | n/a | path |  |  |  | Folder for the shader and other caches; overrides the config file's cache_dir. |
| `VICTORY_CAPTURE` | off | path |  |  | diag | Folder where frames are captured, the base for most VICTORY_CAPTURE_* settings. |
| `VICTORY_CAPTURE_BUFFER_WORDS` | off (0) | number |  |  | diag | Lists non-zero dwords of raw title buffers, and of buffer views (shader and unordered access) of that many bytes or fewer, in the frame listing. |
| `VICTORY_CAPTURE_CBV` | off | list |  |  | diag | Program hashes whose constant buffers the frame listing prints whole. |
| `VICTORY_CAPTURE_CBV_WORDS` | off (0) | number |  |  | diag | Also prints the first n words of constant buffers as hex in the frame listing. |
| `VICTORY_CAPTURE_FRAMES` | off | list |  |  | diag | Comma-separated frame numbers to capture, besides the power-of-two frames. |
| `VICTORY_CAPTURE_LEVELS` | off | flag |  |  | diag | At a captured frame lists textures made level by level whose levels are missing tiles. |
| `VICTORY_CAPTURE_MIN_OPS` | 5000 | number |  |  | diag | Fewest operations a capture window must hold to count as whole frames. |
| `VICTORY_CAPTURE_REQUIRE` | a built-in list of one program hash | list |  |  | diag | Program hashes the capture window must contain to be accepted. |
| `VICTORY_CAPTURE_TRIGGER` | off | path |  |  | diag | File whose appearance opens an operations capture window; the file is deleted. |
| `VICTORY_CAPTURE_UAVS` | off | flag |  |  | diag | Saves the noted unordered-access buffers at the captured frame. |
| `VICTORY_CAPTURE_WINDOW` | 4 | number |  |  | diag | Presents an operations capture window lasts after the trigger. |
| `VICTORY_CAPTURE_WORLD` | off | flag |  |  | diag | Also captures the 2nd and 6th frames after the first frame of 500 draws or more. |
| `VICTORY_CAPTURE_WORLD_ALSO` | off | list |  |  | diag | More frame offsets counted from the first world frame to capture. |
| `VICTORY_CARD_ACCOUNTING` | off | flag |  |  |  | Logs an accounting line of the card's memory use when the texture queue finds no room. |
| `VICTORY_CARD_ALL` | off | flag |  |  |  | Makes every title buffer's host copy live in the card's memory, with no minimum size. |
| `VICTORY_CARD_AWAKE` | on, 1 (turns = by turns, idle = only while the card has nothing of ours to run) | text |  | live |  | Keeps the card's clocks up between submissions; turns alternates on and off for comparison; idle sends the loop only while the card is idle by its clock; in `VICTORY_AB` it is a switch of its own. |
| `VICTORY_CARD_BIG_FIRST` | on (=0 turns off) | flag |  | live |  | Big title buffer copies come first: the texture queue keeps room free for them. |
| `VICTORY_CARD_BUFFERS` | on (=0 turns off) | flag |  |  |  | Keeps host copies of big title buffers in the card's own memory instead of the PC's. |
| `VICTORY_CARD_BUFFERS_MB` | derived from VRAM budget (512, 2048 or a quarter of it up to 8192) | number |  |  |  | Most MiB of title buffer copies kept in the card's memory. |
| `VICTORY_CARD_BUFFERS_MIN_KB` | 64 | number |  |  |  | Smallest title buffer in KiB that gets a host copy in the card's memory. |
| `VICTORY_CARD_BUFFERS_MIN_MB` | derived from VRAM budget (16, 4 or 1 MiB) | number |  |  |  | Smallest title buffer in MiB that gets a host copy in the card's memory. |
| `VICTORY_CARD_HASH_LEN` | on (=0 turns off) | flag |  |  |  | Hashes only the title buffer's own length for a card copy; 0 hashes the whole buffer. |
| `VICTORY_CARD_MEDIUM_KB` | 1024 | number |  |  |  | Size in KiB from which card copies get the big copies' margin and reserve. |
| `VICTORY_CARD_REHASH_RUNS` | on, 1 | flag |  |  |  | Hashes pieces sent up to a card copy in runs of touching pieces, once each. |
| `VICTORY_CARD_ROOM_FOR_BUFFERS` | on (=0 turns off) | flag |  |  |  | Keeps room in the card's budget for title buffer copies and gives up room of copies turned down. |
| `VICTORY_CARD_ROOM_FOR_TARGETS_MB` | 3072 (at most a quarter of the VRAM budget) | number |  |  |  | MiB of card memory the texture queue leaves free for render targets not yet made. |
| `VICTORY_CARD_SPANS` | off | text |  |  |  | Times the card's work on our submissions with timestamps around each submission (`1`); `2` adds a stamp behind every list of a submission (the card's time by kind of list: card steps, preludes, title lists, compare) and counts our own full waits by site. |
| `VICTORY_CARD_STEPS` | on, 2 (0 off, 1 constant buffers only) | number |  |  |  | Does submission steps that read title memory on the card in the flights' order, not on the CPU. |
| `VICTORY_CARD_WINDOWS_LINE` | 80 | number |  |  |  | Hundredths of the card's budget up to which a memory window's card side is made. |
| `VICTORY_CARD_WINDOWS_MB` | 0 (none) | number |  |  |  | Megabytes of title memory kept in card-side windows; 0 means none. |
| `VICTORY_CLEAR_UAV` | on (=0 turns off) | flag |  |  |  | Whether clears of unordered access views are done on the host. |
| `VICTORY_CLOSED_IDLE` | on (=0 turns off) | flag |  | live |  | Whether an idle host command list is kept closed instead of open as a reset leaves it. |
| `VICTORY_CLZERO_ASK` | off | flag |  |  |  | =1 asks every time instead of remembering the answer for the trapped instruction. |
| `VICTORY_COMMITTED_QUERY` | off | flag |  |  |  | =1 asks VirtualQuery whether memory is committed instead of using cheaper tracking. |
| `VICTORY_COMPARE_AVX` | on (=0 turns off) | flag |  |  |  | Compares memory with 256-bit loads where the processor has AVX2. |
| `VICTORY_COMPUTE_WAIT_MS` | 120000 | number |  |  |  | Milliseconds a compute pipeline may be waited for before the draw is dropped. |
| `VICTORY_CONFIG` | n/a | path |  |  |  | Path of the configuration file, else ./victory.toml. |
| `VICTORY_COPIES_IN_PLACE` | on (=0 turns off) | flag |  |  |  | Buffer copies are done on the card at their place in the list. |
| `VICTORY_COPY_ASK` | off | flag |  |  |  | =1 asks first whether the copy destination is committed memory. |
| `VICTORY_COPY_BESIDE` | off | flag |  | live |  | Copies of title memory put bytes beside the write guards. |
| `VICTORY_COPY_MODE` | guarded copies | text |  |  |  | How title copies run: guarded (default), plain, or none. |
| `VICTORY_COPY_OVER_TITLE` | on (=0 turns off) | text |  |  |  | First copy into a title texture zeroes or uploads what unwritten parts hold. |
| `VICTORY_CRASH_SAVE` | n/a | list |  |  |  | Hex rva:len ranges of title memory written to files on a crash. |
| `VICTORY_CUT_WAITS` | on (=0 turns off) | flag |  |  |  | Cuts lists into host lists at their waits for memory values. |
| `VICTORY_CUT_WRITES` | off | flag |  |  |  | =1 also cuts lists at writes of query results and the like. |
| `VICTORY_D3D12_BACKGROUND` | off | flag |  |  |  | =0 switches off the D3D12 runtime's background shader work. |
| `VICTORY_D3D12_DEBUG` | off | flag |  |  |  | Turns on the D3D12 debug layer so refused pipelines carry its messages. |
| `VICTORY_D3D12_DRED` | off | text |  |  | diag | Turns on device-removed data; light and faults select lighter or fault modes. |
| `VICTORY_D3D12_GBV` | off | flag |  |  | diag | Turns on GPU-based validation of the debug layer. |
| `VICTORY_D3D12_WARP` | off | flag |  |  | diag | Uses the software adapter so wild shader accesses crash the process. |
| `VICTORY_DATA_DIR` | n/a | path |  |  |  | Directory for the data files; default is a built-in data folder. |
| `VICTORY_DECLARE_ONCE` | on (=0 turns off) | flag |  |  |  | A frame's dispatches are declared once for deferred GPU writes. |
| `VICTORY_DEPTH_TWINS` | on (=0 turns off) | text |  |  |  | Makes extra depth and stencil twin views of typeless textures. |
| `VICTORY_DESC_WHY` | off | flag |  |  |  | Prints a debug line to stderr about descriptor lane handling during translation. |
| `VICTORY_DIRECT_WRITES` | on (=0 turns off) | flag |  |  |  | Writes to title memory go directly instead of the earlier way. |
| `VICTORY_DISPLAY_MODE` | 3840x2160 | text |  |  |  | WxH display size reported to the title. |
| `VICTORY_DRIVER_CACHE` | off | text |  |  |  | =off turns the driver's and runtime's shader caches off for this process. |
| `VICTORY_DROP_EARLY` | off | flag |  | live |  | Leaves out calls that only feed dispatches the host drops. |
| `VICTORY_DSP_CONVOLVE` | off | flag |  | live |  | Computes the audio DSP's mono 1024-frame convolutions from the title's impulse responses (TVs, phone calls, reverb). |
| `VICTORY_DSP_CONVOLVE_CHECK` | on (=0 turns off) | flag |  |  |  | Whether a response is checked to be in the layout read (windows agree with their neighbours in time) before it is used; `=0` takes it as it is. |
| `VICTORY_DSP_CONVOLVE_GAIN` | 2.0 | number |  | live |  | Scale of the computed convolutions (the PR picked 2 by ear; the hardware's own scaling is not known). |
| `VICTORY_DSP_CONVOLVE_SIMD` | on (=0 turns off) | flag |  |  |  | Whether the convolution runs the code compiled for AVX2 where the machine has it. |
| `VICTORY_DSP_IR_DUMP` | n/a | path |  |  |  | Directory where the first 16 audio streams' impulse responses are saved. |
| `VICTORY_DSP_REVERB` | off | flag |  | live |  | Runs convolution audio streams through a stand-in algorithmic reverb. |
| `VICTORY_DSP_REVERB_LAYOUT` | off | text |  |  |  | =interleaved selects the interleaved layout for the stand-in reverb streams. |
| `VICTORY_DSP_REVERB_LEVEL` | 1.0 | number |  | live |  | Output level of the stand-in reverb. |
| `VICTORY_DSP_REVERB_RT60` | 2.0 | number |  |  |  | Reverb tail time of the stand-in reverb in seconds. |
| `VICTORY_DSTORAGE_HALF` | off | flag |  |  |  | =1 submits storage requests in halves as the PC library does. |
| `VICTORY_DSTORAGE_IO` | handles | text |  |  |  | How storage files are read: one, handles, overlapped or direct. |
| `VICTORY_DSTORAGE_LOG` | off | path |  |  |  | File that receives a line for every storage streaming request. |
| `VICTORY_DSTORAGE_RETRIES` | 12 | number |  |  |  | Times a failed write to title memory is retried before the request fails. |
| `VICTORY_DSTORAGE_STAGING_MB` | 256 | number |  |  |  | Floor in megabytes for the staging buffer size the title sets. |
| `VICTORY_DSTORAGE_TRACE` | 0 (off) | number |  |  |  | Level of the storage streaming trace; 0 is off. |
| `VICTORY_DUMP_CONTAINERS` | off | path |  |  |  | Folder where every translated shader container is also written. |
| `VICTORY_DUMP_MATCH` | n/a | text |  |  |  | dir;text list: dumps containers whose line holds one of the texts. |
| `VICTORY_DUMP_PRECOMPILED` | off | path |  |  |  | Folder where precompiled pipeline calls from the title are dumped. |
| `VICTORY_DUMP_PRECOMPILED_MAX` | 4000 | number |  |  |  | Most dumps written by VICTORY_DUMP_PRECOMPILED. |
| `VICTORY_DUMP_TARGETS` | off | text |  |  |  | Saves render targets of captured frames; =stats writes only statistics. |
| `VICTORY_DXIL` | n/a | path |  |  |  | Path of the DXIL validator library used to check emitted shaders. |
| `VICTORY_DYNAMIC_SLOTS` | computed (254976 on a capped host heap, else up to 1<<20) | number |  |  |  | Number of dynamic descriptor slots added to host view heaps. |
| `VICTORY_EARLY_ALL` | off | flag |  |  |  | =1 starts every title pipeline compiling at creation. |
| `VICTORY_EARLY_TEXTURES_MS` | 12 | number |  |  |  | Milliseconds per frame the early texture upload pass may take. |
| `VICTORY_EFFICIENCY_MODE` | off | flag |  |  |  | =1 leaves the system its choice of efficiency mode for the process. |
| `VICTORY_EMIT_DUMP` | n/a | path |  |  |  | Directory where emitted shaders and validator reports are dumped. |
| `VICTORY_EXACT_BACK` | on (=0 turns off) | flag |  |  |  | Writes back only the changed bytes of a range instead of the whole stretch. |
| `VICTORY_F16_NEAREST` | 2 since integ/validation-24 (off before; `=0` off, `=1` scalar ops) | level | yes |  |  | Rounds 16-bit float results to nearest, as the console does (2 also covers mixed and packed ops). |
| `VICTORY_F32_FTZ` | on since integ/validation-24 (off before; `=0` off) | flag | yes |  |  | Asks the driver for the console float mode: 32-bit denormals flushed to zero (shader model 6.2). |
| `VICTORY_FAIL_DUMP` | n/a | path |  |  |  | Directory where the first containers of each failure reason are dumped. |
| `VICTORY_FAST_CORES` | 2 | number |  |  |  | Stage of asking the system to put threads on fast cores; 0 none, 1, 2. |
| `VICTORY_FAULT_AROUND` | 0 (off) | number |  | live |  | A write fault opens, with its block, up to n-1 hot blocks beside it (opened by a fault this frame or the last) in one change of protection; 1 only batches the blocks an opening has; at most 64. |
| `VICTORY_FAULT_BOOK` | off | path |  |  | diag | File that receives the fault book's report of blocks and timing. |
| `VICTORY_FAULT_BOOK_AT` | 150,210,270 | list |  |  | diag | Seconds after start at which the fault book is written. |
| `VICTORY_FAULT_SPIN` | 0 (off) | number |  | live |  | Microseconds (at most 1000) a title thread in the write-fault handler looks for a busy guard stripe before it sleeps on it. |
| `VICTORY_FETCH_CHECK` | off | flag |  |  |  | =1 checks fetched pieces by hash against what a flight ended with. |
| `VICTORY_FETCH_TRIM` | off (on when VICTORY_RAM_LEAN is 1 or 2) | flag |  |  |  | Shrinks the fetch buffers again when a minute never needed a quarter. |
| `VICTORY_FILL_TITLE` | off | flag |  |  |  | =1 treats small fills of title memory as title data. |
| `VICTORY_FIRSTLANE_OWN` | off | flag | yes |  |  | =1 takes the host wave's own first lane in wave-wide first-lane reads. |
| `VICTORY_FIRST_HALF` | on (=0 turns off) | flag | yes |  |  | Removes first-half interpolants only the second half reads before lowering. |
| `VICTORY_FOLD_VIEWS` | off | flag | yes | live |  | Folds view heap indexing into raw user data in draws and shaders. |
| `VICTORY_FOOTPRINT_STATS` | off | flag |  |  |  | Runs the store footprint analysis on every program and prints stats to stderr. |
| `VICTORY_FULL_CARD_PASS` | on (=0 turns off) | flag |  |  |  | Texture pass puts back every slot needing a new host copy. |
| `VICTORY_FUSE_DISPATCH` | off | flag | yes |  | key (text) | `1`: compute programs are translated in the form of a member of a run of dispatches (they read user data and group id from a record of a table, the root CBV `SLOT_FUSE`); a run of one when nothing joins. Off: the emitted code is the same as without the switch (checked on the corpus, integ/validation-26). |
| `VICTORY_FUSE_RUNS` | off | flag |  | live |  | Needs the programs translated with `VICTORY_FUSE_DISPATCH=1`: the dispatches of one program that follow one another are one table and one dispatch of all their groups (`draw/fuse.rs`). |
| `VICTORY_GAME_DIR` | n/a | path |  |  |  | Directory of the title's files. |
| `VICTORY_GPU_LOAD_MS` | 0 (off) | number |  |  |  | Milliseconds of artificial card load added per frame. |
| `VICTORY_GPU_RO_CHECK` | off | flag |  |  |  | Verification for `VICTORY_GPU_RO_SKIP`: declares full ranges and warns if the compare finds a changed piece in title memory the title asked for GPU-read-only. |
| `VICTORY_GPU_RO_SKIP` | off | flag |  | live |  | Leaves the pages the title asked for with read-only GPU protection (`PAGE_GRAPHICS_READONLY/EXECUTE/EXECUTE_READ`) out of the ranges shaders are declared to write (the CPU constant ring at the head of the big heap): fewer fills, merges and flight waits. Port of Mikael Jarvinen's VIctory-NVIDIA #1014 (+16% fps on a 3080, one run per arm). |
| `VICTORY_GPU_TIMING` | off | flag |  |  | diag | Timestamps each noted command and reports card time by command. |
| `VICTORY_GPU_TIMING_FILE` | n/a | path |  |  |  | File that receives the table of card time by title queue. |
| `VICTORY_GROUP_SKIP` | off | flag |  |  |  | =1 leaves the barrier wait out for groups that do not need it. |
| `VICTORY_GROUP_SYNC` | 2 (every barrier waits) | number | yes |  |  | Which group barriers wait for the whole group: 0 none, 1 title's only, 2 all. |
| `VICTORY_GUARD_ASK` | off | flag |  |  |  | =1 asks the guard sets every time instead of remembering. |
| `VICTORY_GUARD_CHUNK` | 0 (off) | number |  |  |  | Blocks per call while a guard lock is held; 0 means one call. |
| `VICTORY_GUARD_HOLES` | on (=0 turns off) | flag |  |  |  | Guard range walk looks past holes instead of the first address only. |
| `VICTORY_GUARD_REST` | on, 3000 (=0 turns off) | number |  | live |  | Microseconds guard takers rest; value must be above 1. |
| `VICTORY_GUARD_SHARDS` | 0 (one lock) | number |  | live |  | Number of stripes the write guards are divided in, at most STRIPES. |
| `VICTORY_HALF_EXIT` | on (=0 turns off) | flag | yes |  |  | Exchanging host waves clear their exchange dword when leaving a loop. |
| `VICTORY_HEAPS_EARLY` | off | flag |  | live |  | Asks for a descriptor round ahead of a submission's preparing. |
| `VICTORY_HEAP_PASS_ASIDE` | on (=0 turns off) | flag |  |  |  | Heap pass is done aside where possible instead of where needed. |
| `VICTORY_HEAP_RECHECK` | on (=0 turns off) | flag |  |  |  | Uploaded textures are looked at again for changes. |
| `VICTORY_HEAP_RECHECK_EVERY` | off | number |  |  |  | Frames between looks at an uploaded texture; diagnostic. |
| `VICTORY_HEAP_SPARSE` | off | flag |  |  |  | Per-slot arrays of host view heaps take memory only for used slots. |
| `VICTORY_HEAP_SPREAD` | on (=0 turns off) | flag |  |  |  | Spreads the looks at heap slots over passes, each slot every sixteenth. |
| `VICTORY_HEAP_SYNC` | on (=0 turns off) | text |  |  |  | Whether descriptor heaps are synced; =all mirrors every new record at once. |
| `VICTORY_HEAP_SYNC_ASIDE` | on (=0 turns off) | flag |  |  |  | Heap sync is done beside the rest of the preparing. |
| `VICTORY_HEAP_SYNC_BACKOFF` | 2 | number |  | live |  | Doublings of the wait between early sync rounds after quiet rounds. |
| `VICTORY_HEAP_SYNC_EARLY` | 4 | number |  |  |  | Milliseconds between early heap sync rounds; 0 means none. |
| `VICTORY_HEAP_TEXTURES` | on (=0 turns off) | flag |  |  |  | Lets texture uploads use heap-placed textures during baking; =0 skips this path. |
| `VICTORY_HELD_LEVELS` | on (=0 turns off) | flag |  |  |  | Keeps host texture copies from the first mip level their views show, not whole. |
| `VICTORY_HELPER_OWN` | on (=0 turns off) | flag | yes |  |  | Pixel-shader helper lanes read their own value where the console reads the first lane's. |
| `VICTORY_HOLDER_INDEX` | off | flag |  | live |  | Finds the writers meeting a copy range through an address index instead of scanning. |
| `VICTORY_HOST_LDS_KIB` | off | number | yes |  |  | Tells the translator the host allows this many KiB of group shared memory (default 32). |
| `VICTORY_HOST_MAX_WAVE` | off | number | yes |  |  | Translate as for a host whose waves have at most this many lanes (for example 32). |
| `VICTORY_HOST_MESH_OUTPUT_KIB` | off | number | yes |  |  | Tells the translator mesh shaders may output this many KiB (default 32). |
| `VICTORY_HOST_RDNA` | off | flag | yes |  |  | Translate as for an AMD RDNA host (only the exact value 1 turns it on). |
| `VICTORY_HOST_WAVE` | off | number | yes |  |  | Same as the host max wave setting: translate for a GPU with waves of at most this many lanes. |
| `VICTORY_IMAGE_WATCH` | off | text |  |  | diag | Debug aid: watches named words at given offsets of the loaded image and reports changes. |
| `VICTORY_INDIRECT_BATCH` | on, 2 | number |  |  |  | How indirect clamps are batched: 0 each on its own, 1 and 2 batch (2 across dispatches). |
| `VICTORY_INDIRECT_CARD_BUDGET` | on (=0 turns off) | flag |  |  |  | Keeps the indirect-argument budget in card memory instead of CPU-mapped memory. |
| `VICTORY_INDIRECT_FOLD` | off | flag |  | live |  | Folds a run of indirect executions into one job. |
| `VICTORY_INDIRECT_LOOK` | off | flag |  |  |  | Notes indirect runs for inspection; also on when VICTORY_INDIRECT_RUNS=1. |
| `VICTORY_INDIRECT_REST` | off | flag |  |  |  | When one indirect record is oversized, the records after it are also dropped instead of still running. |
| `VICTORY_INDIRECT_RUNS` | off | flag |  |  |  | Makes consecutive indirect draws of a list one execution of all of them. |
| `VICTORY_INDIRECT_WIDE` | on (=0 turns off) | flag |  |  |  | Allows indirect dispatch counts up to 2^20 on AMD cards instead of 65535. |
| `VICTORY_INDIRECT_WINDOW` | on (=0 turns off) | flag |  |  |  | Clamps indirect buffers one window at a time. |
| `VICTORY_INPLACE` | off | number |  |  |  | Places windows of guest private memory as host heap buffers instead of copying (level 1 or 2). |
| `VICTORY_INPLACE_MB` | 4096 | number |  |  |  | Most guest memory in MiB that may be in place (locked) at once. |
| `VICTORY_INPLACE_PROBE` | off | number |  |  |  | Runs the in-place probe and memory census (1), and opens a guest part briefly (2). |
| `VICTORY_INPLACE_PROBE_MB` | 64 | number |  |  |  | Size in MiB of the memory the in-place probe tests (capped at 1024). |
| `VICTORY_JOIN_COPIES` | on (=0 turns off) | flag |  |  |  | Takes a copy step in a list along when joining list parts into one host submission. |
| `VICTORY_JOIN_LISTS` | 96 | number |  |  |  | Most lists that may be joined into one host submission. |
| `VICTORY_JOIN_PARTS` | on, 8 | number |  | live |  | How many list parts are joined into one host submission; 1 turns joining off. |
| `VICTORY_JOIN_RAMP` | off | flag |  | live |  | After a present, lets joined parts ramp up in number instead of taking the full count. |
| `VICTORY_JOIN_RAMP_FIRST` | 1 | number |  |  |  | With the join ramp on, how many parts the first submission after a present may join. |
| `VICTORY_JOIN_WAITS` | off | flag |  | live |  | Takes along a part behind a wait met in a way not seen before. |
| `VICTORY_JOIN_WIDE_MB` | off | number |  | live |  | Joins parts whose upload fills read up to this many MiB; 0 refuses such parts. |
| `VICTORY_KEEP_CHECK` | off | flag |  |  |  | Verifies what the snapshot keeping does not take and counts the misses. |
| `VICTORY_KEEP_FAST` | off | flag |  | live |  | Looks at a slot's kept resource without its lock when it is the same again. |
| `VICTORY_KEEP_FREED` | on (=0 turns off) | flag |  |  |  | Keeps freed scratch chunks instead of giving them back. |
| `VICTORY_KEEP_LOOP_SKIPS` | on (=0 turns off) | flag | yes |  |  | Keeps the early-outs and skips over loops in the translated shader. |
| `VICTORY_KEEP_REMARK` | on (=0 turns off) | flag |  |  |  | Re-marks snapshot copies as current again after they are used; =0 turns that off. |
| `VICTORY_KEEP_SHOWN` | 1 | number |  | live |  | When a slot gets a view of another texture: 0 show nothing, 1 keep a similar one, 2 any same-dimension one. |
| `VICTORY_KEEP_SNAPSHOTS` | on (=0 turns off) | flag |  |  |  | Keeps snapshots of declared memory ranges between submissions instead of taking each afresh. |
| `VICTORY_KEEP_SNAPSHOTS_MB` | 1024 | number |  |  |  | Megabytes of snapshots kept between submissions. |
| `VICTORY_KEEP_VIEWS` | on (=0 turns off) | flag |  |  |  | Keeps a replaced view alive until its use ends instead of nulling it at once. |
| `VICTORY_KEPT_SKIPS_WIDE` | on, safe classes at integ/validation-22 (=0 off, =all every class); off unless =1 or =all in integ/validation-23 | text | yes |  |  | Takes the kept loop skips in their wave-wide form (with VICTORY_KEEP_LOOP_SKIPS); a hang suspect, off in integ/validation-23. `=audited` (integ/validation-27, fix/defrag1, with the hang guard on): the form is audited as the program has it, then made wide. |
| `VICTORY_KEPT_SKIPS_ONLY` | off | text | yes |  |  | (integ/validation-27, fix/defrag1) With `VICTORY_KEPT_SKIPS_WIDE=audited`: the program tags (seven hex digits, comma list) the audited kept skips' form is taken for. Env only. |
| `VICTORY_KEYS` | off | list |  |  |  | Scripted key presses: seconds:key pairs separated by commas, held for the set time. |
| `VICTORY_KEYS_FILE` | off | path |  |  |  | File whose contents give the keys held right now, re-read at most every 10 ms. |
| `VICTORY_KNOWN_HOLES` | on (=0 turns off) | flag |  |  |  | Treats placeholder views as known for what they are, to cut lock waits. |
| `VICTORY_LAND` | on (=0 turns off) | flag |  |  |  | Lands writes from card back to guest memory in bulk instead of piece by piece. |
| `VICTORY_LAND_ANYTIME` | on (=0 turns off) | flag |  |  |  | Lands writes through a mapping at any time instead of through the guest address. |
| `VICTORY_LAND_AHEAD` | on (=0 turns off) | flag |  | live |  | The landing merge asks for its bytes ahead (the card's, the copy's, title memory: 1 KiB in front, and the head of the next piece); same bytes, fewer stalls. |
| `VICTORY_LAND_ASIDE` | on (=0 turns off) | flag |  | live |  | Lands a submission's lists aside, on another thread, saving time on the submitting thread. |
| `VICTORY_LAND_BESIDE` | on (=0 turns off) | flag |  |  |  | Writes views of guest pages through one of our own mappings. |
| `VICTORY_LAND_ECHO` | on (=0 turns off) | flag |  | live |  | Compares pages 4 KiB at a time and sends up only those that differ from what landed. |
| `VICTORY_LAND_LAZY` | off | flag |  |  |  | Makes the tail's landing lazy: the guest is told the copy is owed and it lands later. |
| `VICTORY_LAND_OWN` | off | flag |  | live |  | Leaves the pieces of shader-written copies that nothing but the title's CPU can read on the card: guarded, landed only when something reads or writes them. |
| `VICTORY_LAND_OWN_FORCED` | 8 | number |  |  |  | How many landings of a copy's kept pieces our own readers may force before the copy lands eagerly for good. |
| `VICTORY_LAND_OWN_MB` | 384 | number |  |  |  | Megabytes of kept pieces' bytes; over it, pieces land as before. |
| `VICTORY_LAND_OWN_UNTAKEN` | off | flag |  | live |  | With VICTORY_LAND_OWN, also keeps pieces that other copies lie over when none of those copies ever took their bytes. |
| `VICTORY_LAND_QUIET` | off | flag |  |  |  | Leaves large copies out of landings. |
| `VICTORY_LAND_QUIET_MIN_KB` | 1024 | number |  |  |  | Smallest copy in KiB that quiet landing leaves out. |
| `VICTORY_LAND_SKIP` | off | flag |  |  |  | Skips landing blocks the guest already had (counted either way). |
| `VICTORY_LAND_TAIL_THREADS` | off | number |  | live |  | Threads for the tail of a landing; 0 uses the ordinary landing threads (max 16). |
| `VICTORY_LAND_TAKEN` | on (=0 turns off) | flag |  |  |  | Counts a block with one landed copy as taken so the next refresh does not copy it again. |
| `VICTORY_LAND_THREADS` | cores/4, 1 to 4 | number |  |  |  | Number of landing threads (1 to 16). |
| `VICTORY_LAND_UNWATCH` | off | flag |  |  |  | Resets write watch bits after landing so only later guest writes set them again. |
| `VICTORY_LANE_CORE` | off | flag |  | live |  | Pins the lane thread to one logical processor of one core and moves others off it. |
| `VICTORY_LANE_HIGH` | off | flag |  | live |  | Gives the lane thread the highest priority of the process. |
| `VICTORY_LATE_WAIT` | on, 3 | number |  |  |  | Late-wait stage 0 to 3: when to settle flights; 1 and 2 are earlier stages, 3 is on top. |
| `VICTORY_LATE_WAITS` | off | flag |  |  |  | Makes timed waits sleep to the next tick, as before. |
| `VICTORY_LATE_WAIT_LISTS` | 256 | number |  |  |  | Most lists a late wait may leave in flight. |
| `VICTORY_LATE_WAIT_WRITES` | on (=0 turns off) | flag |  |  |  | Settles a flight's declared writes lazily; =0 settles them as soon as sent. |
| `VICTORY_LAZY_PRELUDE` | on (=0 turns off) | flag |  |  |  | Makes a list's prelude lazily instead of together with the list. |
| `VICTORY_LAZY_TEXTURES` | on (=0 turns off) | flag |  |  |  | Makes host textures when first used instead of when the guest creates them. |
| `VICTORY_LEAN_CARD` | on (=0 turns off) | flag |  | live |  | Finds copies whose CPU side changed through a queue instead of looking at all of them. |
| `VICTORY_LEAN_STEPS` | on (=0 turns off) | flag |  | live |  | Holds each buffer once for all steps of a table instead of once per step. |
| `VICTORY_LEGACY_ASSUME` | off | flag | yes |  |  | Writes legacy multiplies as the host product where no factor can be infinity or NaN. |
| `VICTORY_LENT_HEAP` | on (=0 turns off) | flag |  |  |  | Lends the biggest descriptor heap to dispatches that need it; =0 gives them none. |
| `VICTORY_LEVELS_MIN_KB` | 1024 | number |  |  |  | Smallest texture in KiB made level by level on demand (minimum 64). |
| `VICTORY_LEVELS_ON_DEMAND` | on (=0 turns off) | flag |  |  |  | Makes host texture copies in card memory level by level on demand, through tiled resources. |
| `VICTORY_LEVELS_RETRY` | on (=0 turns off) | flag |  | live |  | Lets a level-capped texture retry upload when room appears; =0 brings the old rule back. |
| `VICTORY_LIBRARY_HANDLE` | on (=0 turns off) | flag |  |  |  | Keeps the pipeline library file handle open after mapping. |
| `VICTORY_LIBRARY_LOCK` | off | flag |  |  |  | Serializes pipeline library calls, one at a time. |
| `VICTORY_LIBRARY_MAP` | on (=0 turns off) | flag |  |  |  | Reads and writes the pipeline library through a file mapping instead of buffers. |
| `VICTORY_LIBRARY_TRIM` | on (=0 turns off) | flag |  |  |  | Trims the library's mapped pages from the working set once used. |
| `VICTORY_LIBRARY_STATIC_SAMPLERS` | off | flag |  |  |  | `=1` loads and stores pipelines with static samplers in the pipeline library too (the behaviour up to integ/validation-24: the driver's place table for static samplers is baked into a stored pipeline and differs between processes, so a loaded one may sample through another sampler; ported from the other fork's a6a302ec9). |
| `VICTORY_LIGHT_REFRESH` | on (=0 turns off) | flag |  |  |  | Skips refreshing view memory that nothing has written since. |
| `VICTORY_LINES_ASIDE` | on (=0 turns off) | flag |  |  |  | Writes the copy statistics lines on a worker thread; =0 writes them on the present thread. |
| `VICTORY_LIST_NICE` | off | number |  | live |  | Runs list-translating threads this many priority levels below the guest's; 0 is normal. |
| `VICTORY_LIST_POOL` | on (=0 turns off) | flag |  |  |  | Reuses command lists from a pool instead of making and dropping each. |
| `VICTORY_LIST_SLOTS` | off | list |  |  | diag | Hash prefixes of pixel shaders whose draws get their slots listed in the frame listing. |
| `VICTORY_LIST_OPS` | off | flag |  |  |  | Diagnostics: per frame, how often and how long each operation of the list translators ran (view_at, shadow lookup, hosted, keep, make_listed, make_twinned, readable, tables, bind). |
| `VICTORY_LIST_VIEWS` | on (=0 turns off) | flag |  | live |  | Makes a view its tables hold once per list instead of once per table. |
| `VICTORY_LIVE_FILE` | off | path |  |  | diag | (integ/validation-23, diag) File read once a second; its `skip=<hash>,...` lines add programs to the skipped list while the title runs. |
| `VICTORY_LOCAL_EXIT` | off | flag | yes |  |  | Translates console wave64 loops as two host waves of 32 that leave together (=1 screen programs, =2 broader). |
| `VICTORY_LOG` | n/a | text |  |  |  | Log filter for the tracing output; falls back to RUST_LOG, for example crate=trace,info. |
| `VICTORY_KEEP_STALLING_SKIPS` | off | flag | yes |  |  | `=1`: a skip over a loop that never ends with no lane left stays in the forms without skips. |
| `VICTORY_LDS_FIRST_INIT` | off | flag | yes |  |  | `=1`: a thread group's shared-memory set-up by its first thread is done whether that thread's lane runs there or not. |
| `VICTORY_LOOPS_WAVE` | off | flag | yes |  |  | `=1` (`=2` every compute program): console wave64 loops are left by each host wave together (water tile generation and tiled light on 32-lane hosts). |
| `VICTORY_LOOPS_WAVE_ONLY` | off | list | yes |  |  | With `VICTORY_LOOPS_WAVE`: only the programs named by a word of their container (comma-separated). |
| `VICTORY_LOOP_LIMITS` | off | list | yes |  |  | Table of per-program, per-stage iteration limits for translated loops. |
| `VICTORY_LOOP_NOTES` | off | flag | yes |  |  | Translated loops that hit their iteration limit leave a note in the faults buffer. |
| `VICTORY_LOOP_WHY` | off | flag | yes |  |  | Prints one stderr line per translated loop, saying why it has its limit. |
| `VICTORY_MAKE_ASIDE` | on (=0 turns off) | flag |  | live |  | Makes a buffer's copy aside from its lock; =0 makes it under the lock. |
| `VICTORY_MAPPED_BY_PAGE` | off | flag |  |  |  | Reads which pages of a texture are mapped by page, without the walk. |
| `VICTORY_MAPPED_BY_VIEWS` | on (=0 turns off) | flag |  |  |  | Finds which pages of a texture are mapped from the views, not by asking the host. |
| `VICTORY_MAP_FRESH` | off | flag |  |  |  | Gives pages the guest commits one at a time their own mapping, not a shared one. |
| `VICTORY_MAP_LEAN` | on (=0 turns off) | flag |  |  |  | When a page is both committed and mapped to the GPU, do each step once instead of twice. |
| `VICTORY_MAP_THREADS` | on, 1 | number |  |  |  | Threads that map a group of pages side by side; auto picks 1 or 16 by load. |
| `VICTORY_MASK_NUMBERS` | on (=0 turns off) | flag | yes |  |  | In the retry translation of a program refused for a mask read as data, allow mask numbers. |
| `VICTORY_MASK_WHY` | off | flag |  |  |  | Prints why a mask has no value of its own, for finding mask refusals. |
| `VICTORY_MEMORY_CARD_MB` | 4096 | number |  |  |  | Most MiB of title memory kept by address on the card before falling back to PC memory. |
| `VICTORY_MEMORY_CARD_OVER` | off | flag |  |  |  | With 1, skip asking the card video memory budget before placing memory copies on the card. |
| `VICTORY_MEMORY_DUMP` | off | path |  |  | diag | Folder where title memory arenas are copied out when a file named go appears there. |
| `VICTORY_MEMORY_FAULT_PAGES` | on, 8192 | number |  |  |  | Most pages taken from shader faults over the whole run. |
| `VICTORY_MEMORY_MAP` | on (=0 turns off) | flag |  |  | diag | Walk the address space to log the memory map line. |
| `VICTORY_MEMORY_STATS` | on (=0 turns off) | flag |  |  |  | Log per-call counts and pages of memory-module calls. |
| `VICTORY_MESH_TEST` | off | text |  |  |  | Experiment on mesh draws: skip, scissor (one pixel) or keep:<hex> to issue only matching ones. |
| `VICTORY_NAN_SAFE` | off (0 = no bits) | number | yes |  |  | Set of bits (decimal or 0x hex) choosing where min/max and compares keep NaN-safe behaviour. |
| `VICTORY_NONFINITE_LIMIT` | 60000 | number |  |  | diag | Magnitude from which a channel counts as extreme when scanning captured pictures for non-finite values. |
| `VICTORY_NOTES` | on, 2 | number |  |  |  | How many notes a copy keeps; clamped 1..64. |
| `VICTORY_NOTES_VOID` | off | text |  |  |  | With range, void only the notes in the written range instead of all of the copy. |
| `VICTORY_ONE_PAD` | on (=0 turns off) | flag |  |  |  | Present all host pads as one player instead of each as its own. |
| `VICTORY_ONE_CALL` | off | flag |  | live |  | A submission joined of several parts is given to the queue in one call of `ExecuteCommandLists`, not one for each part. |
| `VICTORY_ONE_PASS` | off | flag |  | live |  | Prepare a submission from one pass over the steps instead of four walks. |
| `VICTORY_OPEN_ONE_VIEW` | on (=0 turns off) | flag |  | live |  | Open only the guarded block a write faults on instead of all views of it. |
| `VICTORY_OPUS_PITCH` | off | flag |  | live |  | Resample audio packets to the pitch the title asks for. |
| `VICTORY_OWED_CHECK` | off | flag |  |  |  | Compare what a lazy landing wrote with what was fetched. |
| `VICTORY_PAGE_PARTS` | on (=0 turns off) | flag |  |  |  | Check written pages in smaller parts than the 64 KiB page. |
| `VICTORY_PAGE_REFRESH` | off | text |  | live |  | Refreshes driven by pages the title wrote: 1 shadow mode, 2 driven mode. |
| `VICTORY_PAGE_WRITES` | on, 1 | text |  |  |  | What to do with writes the guards miss: 0 count only, 1 bits 1 and 2, slots, all; default bit 1. |
| `VICTORY_PAIRS` | n/a | list |  |  |  | Tool parameter: shader label substrings (vs+ps,vs+gs+ps) to translate in the victory tool. |
| `VICTORY_PAIR_CLAMP` | on (=0 turns off) | flag | yes |  |  | Bound the shared-memory access of paired host waves with a clamp. |
| `VICTORY_PAIR_LEAN` | on (=0 turns off) | flag | yes |  |  | Use the cheaper wave-pair meetings instead of two meetings per access. |
| `VICTORY_PAIR_SYNC` | on (=0 turns off) | flag | yes |  |  | Allow wave-pair meeting waits with own counters; off refuses the forms needing them. |
| `VICTORY_PANEL` | off | flag |  |  | diag | Draw the run numbers on the picture. |
| `VICTORY_PAST_LOG` | off | flag |  |  |  | Log each title slot past the host heap once with what its record names. |
| `VICTORY_PATCH_CLZERO` | on (=0 turns off) | flag |  |  |  | Patch the cache-line-zero instruction once instead of emulating it each time. |
| `VICTORY_PATCH_WAITS` | on (=0 turns off) | flag |  |  |  | Patch the wait instructions once instead of emulating them each time. |
| `VICTORY_PC_DXC` | n/a | path |  |  |  | Path of the PC dxc.exe used to validate or dump DXIL; else the newest SDK one. |
| `VICTORY_PEEK` | off | text | yes |  |  | Program hash and code offsets whose values are left as notes for debugging. |
| `VICTORY_PENDING_FLIGHT` | off | flag |  |  |  | Mark copies as written from the moment a flight is pending. |
| `VICTORY_PICKED_MB` | 32 | number |  |  |  | MiB of picked buffers looked at in turn per frame. |
| `VICTORY_PICKED_WRITES` | off | number |  |  |  | Stage 1 declares what is found as written, 2 also lands it. |
| `VICTORY_PIPELINE_AHEAD` | off | flag |  |  |  | Queue a compute pipeline at bind time, ahead of the dispatch. |
| `VICTORY_PIPELINE_BOOK` | off | flag |  |  |  | Keep the pipelines the title creates, to make host pipelines ahead of later runs. |
| `VICTORY_PIPELINE_BOOST` | off | flag |  | live |  | Raise the level of pipelines being made, per pipeline at its time. |
| `VICTORY_PIPELINE_LOWEST` | on (=0 turns off) | flag |  |  |  | Run pipeline workers below the title own threads. |
| `VICTORY_PIPELINE_NOW_FIRST` | off | flag |  |  |  | Of the requested pipelines a worker takes first one a draw of this frame or the one before asked for (also in the resolves' lane); else the oldest. |
| `VICTORY_PIPELINE_PREPARE` | off | flag |  | live |  | Begin the next submission refreshes on another thread while the current one is sent. |
| `VICTORY_PIPELINE_PRIORITY` | on (=0 turns off) | flag |  | live |  | Pipelines the title waits on or touches go first in the queue. |
| `VICTORY_PIPELINE_RAM_FREE_MB` | 1536 (0: no limit) | number |  |  |  | The free physical memory below which an extra pipeline worker or a ray-tracing resolve starts no new compile. |
| `VICTORY_PIPELINE_SLOW` | on (=0 turns off) | number |  |  |  | Limit how many slow programs are made at once, a quarter of the workers. |
| `VICTORY_PIPELINE_WAIT_ADAPT` | off | flag |  | live |  | Let the per-frame pipeline wait budget follow how the waits end. |
| `VICTORY_PIPELINE_WAIT_FLOOR_MS` | 100 | number |  |  |  | Lowest the adaptive pipeline wait budget may fall to, in ms. |
| `VICTORY_PIPELINE_WAIT_MS` | 500 | number |  |  |  | Waiting allowed per title frame for host pipelines, over all threads, in ms. |
| `VICTORY_PIPELINE_WAIT_SHARED` | on (=0 turns off) | flag |  |  |  | Count the pipeline wait budget over all threads together. |
| `VICTORY_PIPELINE_WIDE` | off | flag |  |  |  | Add extra pipeline workers while a world pass waits for its compute pipeline. |
| `VICTORY_PIPELINE_WORKERS` | on, 2/3 of logical CPUs | number |  |  |  | Number of threads making host pipelines; clamp 1..64. |
| `VICTORY_PLACED_CONSTANTS` | off | flag |  | live |  | A constant buffer that reads bytes the list's own placed copy wrote is snapshotted on the card instead of filled from the title's ring behind flights (the 22.7 ms flight wait of the lane). Port of k0ma2k15's VIctory-NVIDIA #34. |
| `VICTORY_PLACED_FILLER` | off | flag |  |  |  | Keep filler fields of a placed resource description in the lookup key. |
| `VICTORY_PLACED_ORDER` | off | flag |  | live |  | Order placed copies by where they stand in title memory. |
| `VICTORY_POKE` | off | list |  |  | diag | Debug aid: write bytes at given RVAs, form rva:byte,... in hex, into title data. |
| `VICTORY_POOL_COMPACT` | off | flag |  |  |  | (integ/validation-27) Keeps the blocks of textures dense (VRAM2): pieces are cut from the fullest block with room, and while the card is under pressure the blocks with the fewest bytes in use are emptied once a second (`VICTORY_POOL_COMPACT_MB` of textures each), their textures made again in the others and shown after their upload ran; a block that stays after a minute is left alone for two. Gives back the 1.2 to 1.9 GB the blocks hold free at the window spot. |
| `VICTORY_POOL_COMPACT_MB` | 48 | number |  |  |  | MB of textures the compaction makes again each second (`VICTORY_POOL_COMPACT=1`). |
| `VICTORY_POOL_FIT` | off (on with `VICTORY_POOL_COMPACT`) | text |  |  |  | `best`: a piece of a texture block is cut from the fullest block that has room, not from the first. |
| `VICTORY_POOL_SLACK_MB` | 256 | number |  |  |  | MB the blocks of textures keep free (the empty spare blocks among them) before the compaction empties one more block. |
| `VICTORY_POP_ALL` | off | flag |  |  | diag | Compare every frame for popping textures instead of the asked time ranges. |
| `VICTORY_POP_FOLDER` | off | path |  |  | diag | Folder for the pictures around a frame where a texture pops. |
| `VICTORY_POP_WATCH` | off | list |  |  | diag | Time ranges in seconds, from-to, in which presented frames are checked for popping textures. |
| `VICTORY_PRECISE_BARRIERS` | off | flag |  | live |  | Hold the title UAV waits as precise barriers. |
| `VICTORY_PRECISE_LOOK` | off | flag |  | live |  | Make and keep precise write ranges beside the declarations. |
| `VICTORY_PRECISE_WAITS` | n/a | flag |  |  |  | Names a stage of the precise barrier model. |
| `VICTORY_PRECISE_WRITES` | off | flag |  | live |  | Declare buffer views as the precise ranges a shader writes. |
| `VICTORY_PROP_COUNT` | off | flag |  |  | diag | Counts per frame the bytes landed GPU to title memory, refreshed title memory to copies and their overlap (landed to be taken by another copy), with the CPU of each from the landing stamps; one line with the frame line. |
| `VICTORY_PRECOMPILED` | off | text |  |  |  | Answer pipelines from precompiled pieces: 1, fsr or upscaler for the upscaler, all for any. |
| `VICTORY_PRECOMPILED_CALLER` | n/a | text |  |  |  | Module name (lower case) that marks a call as coming from the upscaler. |
| `VICTORY_PRESSURE_LEGACY` | off | flag |  |  |  | Use the memory bounds of a 32 GiB PC on every PC. |
| `VICTORY_PROBE_WATCH` | off | number |  |  | diag | Seconds between samples of the host volumes at a present; 0 is off. |
| `VICTORY_PROCESS_PRIORITY` | on (=normal or 0 turns off) | text |  |  |  | Run the process above normal priority; normal or 0 keeps the starting priority. |
| `VICTORY_PROFILE` | off | number or list |  |  | diag | Sampling profiler. A rate (`1000`): the whole-process profiler (the `victory-profile` thread): every 1/rate s it stops each thread of the process that used cycles since it was last seen (ours, the title's, the driver's), unwinds its stack by the `.pdata` of each module and counts it in a tree of fixed size (no allocation or lock while a thread is stopped). Writes `VICTORY_PROFILE_FILE` (folded stacks, count = samples), `.cycles` (the same in thousands of cycles) and `.txt` (run numbers, modules with their PDB GUID, threads, groups, top 50 self and inclusive functions of each group) every `VICTORY_PROFILE_SECS` and at exit. Marks `[fault]` (stack passes `KiUserExceptionDispatcher`), `[syscall]`, `[spin]` (at a `pause` or in a wait loop), `[truncated]`. Read with `prof-read.py` against the PDB of `victory-diag.exe`. A list `from,to,ms,file[;...]` is the first profiler: the 28 busiest threads, between two times, written at the end. |
| `VICTORY_PROFILE_DEPTH` | 32 | number |  |  | diag | Frames kept of a stack by the rate profiler (2 to 64); a deeper stack is cut at the outer end and marked `[truncated]`. |
| `VICTORY_PROFILE_DUTY` | off | list |  |  | diag | `on,off` seconds: the rate profiler samples for `on` seconds, then stops for `off`, alternating from the start, and writes the windows in the report (to read its own cost from the census lines of the two). |
| `VICTORY_PROFILE_FILE` | `profile.folded` beside the run's capture folder (`VICTORY_CAPTURE`), else in the cache folder | path |  |  | diag | The folded file of the rate profiler; `.cycles` and `.txt` are written beside it. |
| `VICTORY_PROFILE_FROM` | 0 | number |  |  | diag | Seconds after the title starts at which the rate profiler begins (leave the load out): `VICTORY_PROFILE=1000 VICTORY_PROFILE_FROM=240 VICTORY_PROFILE_TO=360` takes 120 s of the window spot of a warm run. |
| `VICTORY_PROFILE_FOR` | off | number |  |  | diag | Seconds the rate profiler lasts from the moment it begins (`VICTORY_PROFILE_FROM` or the start file); then it writes its files and ends. |
| `VICTORY_PROFILE_LINES` | 200000 | number |  |  | diag | Stacks of the heaviest, by cycles, that the folded files keep (the tables count them all). |
| `VICTORY_PROFILE_MIN_KC` | 300 | number |  |  | diag | Thousands of cycles a thread must have used since its last sample to be sampled again by the rate profiler (about 0.14 ms of CPU on the test PC). Stopping a thread costs the thread itself 60 to 120 thousand cycles (measured: median 58k, p90 124k), so any lower and a thread that waits would look busy because it was sampled; a thread that runs less is sampled when what it used adds up to this, and each sample carries all of it. |
| `VICTORY_PROFILE_NODES` | 1048576 | number |  |  | diag | Nodes of the rate profiler's tree of stacks (24 bytes each and 8 for the table): its memory is fixed at the start (about 32 MB); a sample that finds no room counts for the deepest node it reached (`lost_samples`). |
| `VICTORY_PROFILE_SECS` | 15 | number |  |  | diag | Seconds between the writes of the files of the rate profiler (at least 1; one more at exit). |
| `VICTORY_PROFILE_START_FILE` | off | path |  |  | diag | The rate profiler begins when this file exists (the driver of a run makes it when the world is reached: `pilot2-prof.ps1`), instead of at `VICTORY_PROFILE_FROM`. |
| `VICTORY_PROFILE_TO` | off | number |  |  | diag | Seconds after the title starts at which the rate profiler writes its files and ends (later than `VICTORY_PROFILE_FROM`). |
| `VICTORY_QUEUE_WAIT` | on (=0 turns off) | flag |  |  |  | Queues wait for fences. |
| `VICTORY_QUICK_FIND` | on (=0 turns off) | flag |  | live |  | Keep the table of found buffers. |
| `VICTORY_QUICK_MIRROR` | on (=0 turns off) | flag |  |  |  | Skip the mirror step in descriptor copies. |
| `VICTORY_QUIET_COMPARE` | off | flag |  | live |  | Compare copies against the CPU side and note only what differs. |
| `VICTORY_QUIET_COPIES` | on (=0 turns off) | flag |  | live |  | Keep working buffers of upscalers from being copied whole at every refresh. |
| `VICTORY_R11G11B10_AS_F16` | off | flag |  |  |  | Make R11G11B10 targets and volumes as 16-bit float RGBA stand-ins. |
| `VICTORY_RAM_LEAN` | off | number |  |  |  | Memory caps level: 1 the caps, 2 caps and finer windows. |
| `VICTORY_RAM_TOTAL_GB` | off | number |  |  |  | Make the host take the PC for one with this many GiB of RAM. |
| `VICTORY_RANGE_VALID` | off | flag |  | live |  | Validate copies by the log of changes instead of counts. |
| `VICTORY_RAW_VIEWS` | on (=0 turns off) | flag | yes |  |  | Keep a raw twin of every buffer view so shaders picking buffers by computed index can read them. |
| `VICTORY_RDNA_FLOAT` | on, all bits | number | yes |  |  | Set of bits (decimal or 0x hex) of RDNA float behaviour used when the host is an RDNA card. |
| `VICTORY_READY_ONCE` | on (=0 turns off) | flag |  | live |  | Skip entries already found ready instead of going through all of them at every draw. |
| `VICTORY_READ_BACK` | on (=0 turns off) | flag |  |  |  | Make the read-back footprint copies of buffers nobody wrote. |
| `VICTORY_READ_PROBE` | off | flag |  |  |  | Probe pages of buffers only shaders touch to see if the title CPU reads them. |
| `VICTORY_READ_WATCH` | n/a | flag |  |  |  | Names a past run option in a comment. |
| `VICTORY_REAL_OCCLUSION` | off | flag |  |  |  | Run occlusion queries on the host instead of faking them. |
| `VICTORY_RECHECK_SOON` | on (=0 turns off) | flag |  |  |  | Recheck again in the next frame instead of after four. |
| `VICTORY_RECORD` | off | path |  |  | diag | Folder where a frame of D3D12 work is recorded for replay. |
| `VICTORY_RECORD_ALWAYS` | off | flag |  |  | diag | Record every trigger instead of one capture. |
| `VICTORY_RECORD_AT` | n/a | text |  |  | diag | When to record: seconds or f<frame>; else a file trigger. |
| `VICTORY_RECORD_FRAMES` | 1 | number |  |  | diag | How many frames to record. |
| `VICTORY_REFRESH_AHEAD` | on (=0 turns off) | flag |  |  |  | Bring the copies a submission shaders write up to date together on several threads, ahead of its steps. |
| `VICTORY_REFRESH_CHECK` | off | flag |  |  |  | Checks after each refresh that title memory pages match the card copy, reporting differences. |
| `VICTORY_REFRESH_FRONT` | on (effectively, via VICTORY_JOIN_PARTS default 8; needs >=4 CPUs) | flag |  |  |  | Refreshes everything a submission needs in one go on several threads before its steps. |
| `VICTORY_REFRESH_LPT` | off | flag |  |  |  | Orders refresh work longest-piece-first so the last piece of work to finish waits less. |
| `VICTORY_REFRESH_NARROW` | off | number |  | live |  | Splits each copy's refreshes into narrow spans separated by gaps (1 = 64 KiB, else that many KiB). |
| `VICTORY_REFRESH_NOTES` | on (=0 turns off) | flag |  | live |  | Skips re-looking at ranges whose write stamps stand and whose bits are set. |
| `VICTORY_REFRESH_SIZING` | off | flag |  |  |  | Sizes each refresh to the span the touches of a flush refer to instead of by address. |
| `VICTORY_RELAUNCHED_FROM` | n/a | text |  |  |  | Internal hand-off: handle number of the old process, set in the environment of a relaunched copy. |
| `VICTORY_RELAY` | on (stage 2; =1 stage 1; =0 off) | number |  |  |  | Lands what shaders wrote back to title memory on a thread of its own. |
| `VICTORY_RELAY_COPY` | on (=0 turns off) | flag |  |  |  | Lands the relay's fetches over the card's copy queue instead of the drawing queue. |
| `VICTORY_REPLAY_COUNT` | off | flag |  |  |  | Counts what reusing recorded lists could save, without reusing anything. |
| `VICTORY_REPLAY_GBV` | off | flag |  |  | diag | Replay tool: also turns on GPU-based validation of every shader access under the debug layer. |
| `VICTORY_REPLAY_PEEK` | off | flag |  |  | diag | Replay tool: peeks at the state at every diverging command, not only when several kinds differ. |
| `VICTORY_REPLAY_TRACE_UPLOAD` | off | flag |  |  | diag | Replay tool: uploads textures one part at a time, waiting on each, to name a hanging part. |
| `VICTORY_RESIDENCY_PRIORITY` | off | flag |  |  |  | Sets D3D residency priorities on allocations by class. |
| `VICTORY_RESIDENT_BATCH` | on (=0 turns off) | flag |  | live |  | Makes residency calls for a batch of objects together instead of one call or round trip each. |
| `VICTORY_RESOLVE_ASYNC` | off | flag |  |  |  | The ray-tracing resolves are made in a lane of their own and a dispatch of one is dropped, never waited for, while its pipeline is made (not with `VICTORY_BVH_BUILDER=1`). |
| `VICTORY_RESOLVE_WORKERS` | all but a quarter of the workers | number |  |  |  | How many workers make ray-tracing resolves at once with `VICTORY_RESOLVE_ASYNC` (`0`: every one). |
| `VICTORY_REUPLOAD_WHEN_FULL` | on (=0 turns off) | flag |  |  |  | Keeps bringing textures that have a host copy up to date while the card has no room. |
| `VICTORY_RIPE_BLOCKS` | 12 | number |  |  |  | Number of blocks kept ready in the pool lists before pieces are cut from fresh ones. |
| `VICTORY_ROOT_SRV_DIRECT` | off | flag |  |  |  | Goes back to root descriptors for buffer views instead of the bounded descriptor-table path. |
| `VICTORY_ROOT_UAV_MB` | no limit | number |  |  |  | Caps in MB the view, refresh and copy-back of a root unordered-access view. |
| `VICTORY_ROOT_VIEW_MB` | no limit | number |  |  |  | Caps in MB the size taken for a root shader-resource view of a title buffer. |
| `VICTORY_RT_RESOLVE` | on (=0 leaves the hit-shading programs out) | flag |  | live |  | Whether the ray-tracing hit-shading compute programs run; 0 leaves them out (never made, their dispatches dropped before they are recorded), which saves their compile time and, with no structures built, changes nothing in the picture: no ray hits, nothing to shade. Live in `VICTORY_AB` (the programs are made, their dispatches left out in the B phases). |
| `VICTORY_SAFE_COPY` | on (=0 turns off) | flag |  |  |  | Copies title memory with a fault-tolerant routine instead of the kernel's write. |
| `VICTORY_SAMPLER_TABLE` | off | flag |  |  |  | `=1` binds the immediate samplers of the title's pipelines through one-descriptor tables of the sampler heap (a region of 64 slots at the end of each host sampler heap holds the same descriptions the static samplers would) instead of as static samplers of the root signature, so that nothing the NVIDIA driver puts into a stored pipeline (the place of each static sampler in its own table) depends on the process; the pipelines that have them go into the pipeline library again (`VICTORY_LIBRARY_STATIC_SAMPLERS` is then of no matter for them). The translation does not change (no shader cache entry is made again); the library names of these pipelines are new (a tag in the key), and `victory warm` makes them again (the tag is in its fingerprint). Falls back to static samplers, pipeline by pipeline, when the root signature with tables is refused or a border colour has no float form; a draw is dropped (logged once) when the bound sampler heap has no room for the region. |
| `VICTORY_SAME_RECORDS` | on (=0 turns off) | flag |  | live |  | Skips writing records into title memory that already hold the same bytes. |
| `VICTORY_SCISSOR_TEST` | off | text |  |  |  | Experiment on the title's scissor rectangles; only the value full enables it. |
| `VICTORY_SCRATCH_POOL` | on (=0 turns off) | flag |  |  |  | Keeps scratch chunks in a shared pool instead of per list (4 MiB chunks). |
| `VICTORY_SCRATCH_SPARE_MB` | 1024 on a PC of more than 32 GiB (when keep_freed), else 128; lean 128 | number |  |  |  | Megabytes of scratch chunks kept while no list holds them. |
| `VICTORY_SCREEN_WAIT_MS` | 10000 | number |  |  |  | Longest wait in ms for screen-size compute passes to compile before they are skipped. |
| `VICTORY_SECTION_WINDOW` | on (=0 turns off) | flag |  |  |  | Writes through a long-lived mapped window of a section instead of a new view per write. |
| `VICTORY_SEND_BAR` | off | flag |  |  |  | Keeps send memory in the card's memory, written by the CPU across the bus. |
| `VICTORY_SECTION_MB` | off | number |  |  |  | Makes the physical pages the title asks for (one request of ~6.1 GiB) in sections of at most this many MiB each (2 is the tested value), so that a map or unmap of a page waits for the lock of a small section and not of one with hundreds of thousands of views. Read once; compared between processes, not live. Port of CLHelper's VIctory-NVIDIA #44. |
| `VICTORY_SEND_CHANGED` | off | flag |  |  |  | Sends up only the dwords of a page that changed since the last send. |
| `VICTORY_SEND_COPYQ` | off | flag |  | live |  | Sends submission bytes over the bus on a copy queue. |
| `VICTORY_SEND_COPYQ_MB` | 64 | number |  |  |  | Most video memory in MB the copy-queue stages take together. |
| `VICTORY_SEND_COPYQ_MIN_KB` | 512 | number |  |  |  | Least a submission must send, in KiB, to use a copy-queue stage. |
| `VICTORY_SEND_OWN` | on (=0 turns off) | flag |  |  |  | Takes the bytes a submission sends into memory of its own when it is prepared. |
| `VICTORY_SEND_SPARE_MB` | 768 (256 with VICTORY_SEND_BAR=1); lean 128 | number |  |  |  | Megabytes of send memory kept while no submission holds it. |
| `VICTORY_SHADER_CENSUS` | off | path |  |  |  | Appends a line of instruction counts per emitted program to the given file. |
| `VICTORY_SHADER_LOOP_LIMIT` | none (the stages' own limits) | number | yes |  | diag | Forces one iteration limit on every translated loop; decimal or 0x hex, at least 1. |
| `VICTORY_SHADER_OPT` | 63 (every clean-up pass but bit 64) | number | yes |  |  | Picks which clean-up passes run over translated shader code; 0 none, else a pass bitmask. |
| `VICTORY_SHADER_SAVE` | off | path |  |  |  | Saves each shader as given and as translated into the given folder. |
| `VICTORY_SHADER_SOURCE_HASH` | n/a | text |  |  |  | Compile-time hash of the translator source set by build.rs, embedded with env!. |
| `VICTORY_SHADER_SWITCHES` | n/a | list |  |  |  | Compile-time list of the VICTORY_ names found in translator source, set by build.rs. |
| `VICTORY_SHOW_AFTER` | on (=0 turns off) | flag |  |  |  | Shows a stand-in texture until the submission with its upload has finished. |
| `VICTORY_SKIP_BVH_SKINNING` | off | path |  |  |  | Leaves out skinning dispatches listed in a hash file (=1 beside the exe, =@path elsewhere). |
| `VICTORY_SKIP_CPU_CHECK` | off | flag |  |  |  | Bypasses the start-up check that the CPU supports AVX2. |
| `VICTORY_SKIP_PIPELINES` | off | list |  |  | diag | Skips draws and dispatches of pipelines using the listed 16-digit shader hashes. |
| `VICTORY_SLOT_CHECK` | off | flag |  |  | diag | Starts a thread that walks the view heap and reports slots left stale. |
| `VICTORY_SLOT_HOLDS` | on (=0 turns off) | flag |  |  |  | Has a constant-buffer view slot hold its buffer and scratch memory, not only the list. |
| `VICTORY_SNAPSHOT_POOL` | 1024 | number |  |  |  | How many snapshot buffers are kept for reuse. |
| `VICTORY_SNAP_ASIDE` | on (=0 turns off) | flag |  | live |  | Gives snapshots to kept threads and waits for them before sending, saving about 3.5 ms. |
| `VICTORY_SPLIT_BLOCKED` | off | number |  |  |  | Sends the parts before the first part that waits on flights first (1); 2 only counts. |
| `VICTORY_SPLIT_LISTS` | off | flag |  |  |  | Lets a split cut also fall inside lists, with at least VICTORY_SPLIT_MIN of them. |
| `VICTORY_SPLIT_MIN` | 2 | number |  |  |  | Least number of lists the second part must have for VICTORY_SPLIT_LISTS to cut. |
| `VICTORY_SPLIT_WAITS` | on (=0 turns off) | flag |  |  |  | Splits a queue submission's waits so parts go on while others wait, instead of holding whole submissions. |
| `VICTORY_SPIN_SAMPLE` | off | number |  |  |  | Diagnostics: every this many ms (1-1000) looks at where the title's busy threads are and says each 5 s how much of their processor time is in wait loops (monitorx/mwaitx). |
| `VICTORY_STACKS` | off | list |  |  |  | Prints every thread's stack at each listed time, in seconds from start. |
| `VICTORY_STAGE_A` | off | flag |  | live |  | Offers the next submission's refreshes before the steps of the one being prepared. |
| `VICTORY_STAGE_A_DEEP` | off | flag |  | live |  | (integ/validation-23) Also refreshes the copies of the entry after the next while the next is prepared. |
| `VICTORY_STAGE_A_LOOKAHEAD` | off | flag |  | live |  | (integ/validation-27) Stage A prepares the oldest submission of the line whatever its queue, past the signals and met waits that stand before it on its own queue. Needs `VICTORY_STAGE_A_STEPS`. |
| `VICTORY_STAGE_A_RELAX` | 0 (off) | number |  | live |  | (integ/validation-23) Bits of what stage A's thread may also do: note-only host steps, reads, planes, bounded waits for flights. |
| `VICTORY_STAGE_A_STEPS` | off | flag |  | live |  | Has a thread of ours prepare the next submission's whole front part while the previous one is sent. |
| `VICTORY_STAGING_HOLD_MS` | 0 (off) | number |  | live |  | Milliseconds an empty texture-staging block is held before being let go. |
| `VICTORY_STAGING_SPARE` | 8; lean 2 | number |  |  |  | Number of empty texture-staging blocks kept. |
| `VICTORY_STALE_TEXTURES` | on (start-of-block rule; =0 off) | text |  |  |  | Rule for when a view the title writes makes older views of other memory stale. |
| `VICTORY_STATE_FILTER` | off | number |  | live |  | Skips telling the host list state it already has (1); 2 tells it always but counts. |
| `VICTORY_STEPS_MERGE_READS` | off | flag |  | live |  | Records each list's reads in one go, whole or joined. |
| `VICTORY_STEP_MERGE` | off | flag |  |  |  | Merges neighbouring copies with nothing between into one copy. |
| `VICTORY_STORE_BESIDE` | off | flag |  | live |  | Writes title stores to a second place beside the title memory. |
| `VICTORY_STREAM_NOTICE` | on (=0 turns off) | flag |  |  |  | Tells the GPU side which pages a storage read wrote so earlier texture uploads are redone. |
| `VICTORY_STREAM_PRIORITY` | on (=0 turns off) | flag |  |  |  | Raises a below-normal streaming thread to above-normal priority. |
| `VICTORY_STREAM_REUSE` | on (=0 turns off) | flag |  |  |  | Reuses audio and storage stream buffers instead of making and freeing each time. |
| `VICTORY_STREAM_STATS` | on (=0 turns off) | flag |  |  |  | Logs audio and storage streaming statistics every few seconds. |
| `VICTORY_STRICT_JOINS` | off | flag | yes |  |  | Marks joins read through other joins strictly when lowering, instead of the shortcut. |
| `VICTORY_SUBMIT_DEPTH` | 64 | number |  |  |  | How many submissions may be in the line when a submit call returns. |
| `VICTORY_SUBMIT_FRAMES` | 1 | number |  | live |  | How many presents may be in the line when a present returns. |
| `VICTORY_SWEEP` | on (=0 turns off) | flag |  |  |  | Once a second lets go of host objects that nothing names any more. |
| `VICTORY_TABLED_TEXTURES` | on (=1 turns off) | flag |  |  |  | Drops slots that draws read through tables from the queue of textures picked by index. |
| `VICTORY_TAKE_PARTS` | on (=0 turns off) | flag |  | live |  | Takes only the parts of a page written since last taken instead of the whole page. |
| `VICTORY_TARGET_FPS` | off | number |  |  |  | Rewrites the title's settings file for a frame rate of 15, 20, 30 or 60 before start. |
| `VICTORY_TEST_TITLES` | none (the config file's test_titles) | list |  |  |  | Path list of test titles, overriding the config file; split like PATH. |
| `VICTORY_TEXTURE_AWAY_MS` | 3000 | number |  |  |  | Milliseconds a texture's memory must stay unmapped before its host copy is let go. |
| `VICTORY_TEXTURE_BACK` | on (the four default kinds; 0 none) | list |  |  |  | Copies back into title memory textures of listed kinds that shaders write. |
| `VICTORY_TEXTURE_BUDGET_MB` | none (share of the card budget) | number |  |  |  | Limits in MB the bytes of host texture copies instead of the card-budget share. |
| `VICTORY_TEXTURE_CATCHUP` | on (=0 turns off) | flag |  |  |  | Gives a backlog of texture slots three times the per-frame byte and time budget. |
| `VICTORY_TEXTURE_COPY` | off | flag |  |  |  | Fills textures through the kernel's copy instead of in place. |
| `VICTORY_TEXTURE_FILL` | on (=0 turns off) | flag |  |  |  | Fills a texture staging buffer from the pages present when a no-wait refresh finds a page missing. |
| `VICTORY_TEXTURE_GUARD` | on (=0 turns off) | flag |  |  |  | Uses the write watch to skip texture hash checks when no write was seen in the range. |
| `VICTORY_TEXTURE_HOT` | on (=0 turns off) | flag |  |  |  | Recently read textures are checked for change on every frame instead of every eighth frame. |
| `VICTORY_TEXTURE_KEEP` | on (=0 turns off) | flag |  |  |  | Keeps idle textures resident longer while the card has room, instead of a flat 5 s idle rule. |
| `VICTORY_TEXTURE_NO_WAIT` | on (built-in default 1) | flag |  |  |  | Texture upload goes on without waiting for the recording that copies from it. |
| `VICTORY_TEXTURE_ONCE` | on (=0 turns off) | flag |  |  |  | Texture memory is opened to the title once per refresh instead of repeatedly (fewer write faults). |
| `VICTORY_TEXTURE_POOL` | on (=0 turns off) | flag |  |  |  | Pools texture allocations; =0 makes each one an allocation of its own. |
| `VICTORY_TEXTURE_PRIORITY` | on, mode 1 (0 off, 2 also counts untouched textures cold) | number |  |  |  | Gives card tiles to hot textures first; mode 2 also treats never-sampled textures as cold. |
| `VICTORY_TEXTURE_RECLAIM` | on (=0 turns off) | flag |  |  |  | Takes tiles back out of cold textures when the card is short of room. |
| `VICTORY_TEXTURE_ROOM_PERCENT` | 90 (clamped 50 to 97) | number |  |  |  | Percent of the card's memory budget textures may fill before levels are capped. |
| `VICTORY_TEXTURE_SAME` | on (=0 turns off) | flag |  |  |  | Asks whether a texture upload is unchanged before copying it again. |
| `VICTORY_TEXTURE_SQUEEZE` | on (=0 turns off) | flag |  |  |  | Caps texture level sizes by the card's current headroom instead of a fixed rule. |
| `VICTORY_TEXTURE_STAGE_MB` | by video memory: 96, 192, 256 or 384 | number |  |  |  | Megabytes of texture staging allowed per frame; unset picks by the card's memory budget. |
| `VICTORY_TEXTURE_STAGE_MS` | 250 | number |  |  |  | Milliseconds per frame the texture staging pass may spend before leaving the rest for later frames. |
| `VICTORY_TEXTURE_SWEEP` | on (=0 turns off) | text |  |  |  | Lets idle texture levels go in sweeps; =0 lets nothing go, =whole sweeps everything each time. |
| `VICTORY_TEXT_ENTRY` | off (asks the user) | text |  |  |  | Answer for the on-screen text entry dialog: default, cancel, or anything else asks the user. |
| `VICTORY_THIN` | on (built-in default 1) | flag |  |  |  | Records command lists on worker threads (thin lists). |
| `VICTORY_THIN_THREADS` | 8 (clamped 1 to 32) | number |  |  |  | Number of worker threads that record thin lists. |
| `VICTORY_THREAD_DIMENSIONS` | on (=0 turns off) | flag |  |  |  | Dispatch counts given in threads are converted to groups instead of being taken as groups. |
| `VICTORY_THREAD_PRIORITY` | 1 | number |  |  |  | Priority scheme of our threads against the title's: 0 all equal, 1 default, 2 leftovers to ours. |
| `VICTORY_TILE_GPU` | n/a (console's own pipe and packer counts) | text |  |  |  | Overrides the tile configuration (pipes,packers) that the XOR swizzle texture layouts use. |
| `VICTORY_TILE_TRIM` | off | flag |  |  |  | Trims the card memory the system counts against us for tiled textures. |
| `VICTORY_TIMELINE` | off (unset) | path |  |  | diag | Writes a timeline of a few frames to a file, optionally with start times and frame count. |
| `VICTORY_TIMELINE_AT` | 60 | number |  |  | diag | Seconds after which the timeline recording starts at the next present. |
| `VICTORY_TIMELINE_FRAMES` | 3 | number |  |  | diag | Number of frames the timeline records. |
| `VICTORY_TITLE_POOLS` | off | text |  |  |  | Patches the title's start-up so its memory pools are sized as on the bigger console (series-x). |
| `VICTORY_TITLE_PRIORITIES` | on (=0 turns off) | flag |  |  |  | Remaps the title's thread priorities; =0 passes them through as the title sets them. |
| `VICTORY_TITLE_TAIL` | on (=0 turns off) | flag |  |  |  | Descriptor slots past the table read the title's tail data instead of what a table put there. |
| `VICTORY_TRAIL` | on (=0 turns off) | flag |  |  |  | Keeps and reports a trail of recent events; =0 keeps and says nothing. |
| `VICTORY_TRANSLATE_ONLY` | n/a (all containers) | list |  |  | diag | Tool parameter: comma list of hash prefixes, translates only those shader containers. |
| `VICTORY_TRANSLATOR` | n/a | text |  |  |  | Compile-time stamp (hash of translator sources) set by build.rs and hashed into the shader cache key. |
| `VICTORY_TRIGGER_RUMBLE` | on (=0 turns off) | flag |  |  |  | Sends trigger motor rumble to the pad; =0 folds the trigger motors into the main ones. |
| `VICTORY_UNDER_THREADS` | 6 (clamped 1 to 16) | number |  |  |  | Threads in the pool that makes landings under the title's memory. |
| `VICTORY_UNDER_TRY` | off | flag |  | live |  | Copies whose locks are free go first, copies a landing holds go last. |
| `VICTORY_UNDER_WIDE` | on (=0 turns off) | flag |  | live |  | Compares pages of merged under-writes on several threads instead of the submitting thread. |
| `VICTORY_UNDER_WORDS` | on (=0 turns off) | flag |  |  |  | Sends the changed words under a copy instead of the changed bytes. |
| `VICTORY_UNIFORM_LOADS` | 0 | number | yes |  |  | Treats loads at lane-uniform addresses as alike in every lane (1 LDS reads, 2 buffer loads too). |
| `VICTORY_UNWATCHED_FRAMES` | 1 | number |  | live |  | Refreshes copies of memory that is not write-watched every N frames; 1 is every frame. |
| `VICTORY_UPCOPY_CHECK` | off | flag |  |  |  | Diagnostic that compares what is sent up to a card copy against what the card holds. |
| `VICTORY_UPLOAD_AHEAD` | on (built-in default 1; =0 turns off) | flag |  | live |  | Reads, detiles and copies texture uploads ahead on threads of ours. |
| `VICTORY_UPLOAD_HELD_ONLY` | off | flag |  | live |  | Upload copies only the texture levels the title holds. |
| `VICTORY_UPLOAD_LOW` | on (=0 turns off) | flag |  |  |  | Upload buffers come from low addresses; =0 uses the process heap. |
| `VICTORY_VIEW_BYTES` | on (=0 turns off) | flag |  |  |  | Fills in byte sizes of descriptor views; =0 leaves them null. |
| `VICTORY_VIEW_AT_CACHE` | off | flag |  | live |  | A thread answers the view a record names from a table of its own, which it drops entries from as the title frees or reuses them, instead of through the lock of the table of views. |
| `VICTORY_VIEW_CACHE` | 0 (off) | number |  | live |  | Keeps buffer views from one recording to the next; 2 also compares each with one made again. |
| `VICTORY_VIEW_COPY` | on (=0 turns off) | flag |  |  |  | Makes view copies in a staging descriptor heap instead of in place. |
| `VICTORY_VIEW_EXTENT` | on (=0 turns off) | flag |  |  |  | Copies up the whole extent a view shows rather than only the view's range. |
| `VICTORY_VIEW_GUARD` | on (=0 turns off) | flag |  |  |  | Write watch guards the other views of the same memory, not each address alone. |
| `VICTORY_VIEW_INDEX` | off | flag | yes |  |  | Root SRVs are read through the view heap by an index placed in the user data. |
| `VICTORY_VIEW_INLINE` | on (=0 turns off) | flag |  |  |  | Views stay inline in their records rather than each becoming a table entry. |
| `VICTORY_VIEW_LIMIT` | n/a (host probes the heap limit) | number | yes |  |  | Tool parameter: translates as on a host whose view heap has this null slot. |
| `VICTORY_VIEW_THREADS` | 4 (6 for the records pool; clamped 1 to 16) | number |  |  |  | Threads that make descriptors and records. |
| `VICTORY_VRAM_HOLD` | off | flag |  |  |  | Keeps the hot allocations in the card's memory: residency priorities on every hot allocation (maximum for render targets and depth, high for the other textures made whole and the card buffers), the trim of heaps with no tile, and the limit of the card's whole use past the budget (`VICTORY_VRAM_OVER_MB`, 400 here). |
| `VICTORY_VRAM_OS` | on (=0 turns off) | flag |  |  |  | Reads the kernel's statistics of this process's video memory (demoted bytes by priority class, allocations in a segment they do not prefer, evictions) and says them in the `vram os` line every 5 s; asks the system for budget change events. |
| `VICTORY_VRAM_OVER_MB` | off (400 with `VICTORY_VRAM_HOLD`) | number |  |  |  | How far past the system's budget the card's whole use may stand, in MB, before the texture room says no (free room inside blocks and heaps does not count against it). |
| `VICTORY_WAIT_ASK` | off | flag |  |  |  | Asks whether memory is committed before waiting on it. |
| `VICTORY_WAIT_FORCE` | on (=0 turns off) | flag |  |  |  | Forces the held wait to finish across frames; =0 keeps it held. |
| `VICTORY_WAIT_NICE` | off | flag |  | live |  | Replaces the title's busy-wait loops with eased waits. |
| `VICTORY_WAIT_ORDER` | off | flag |  | live |  | Gives a flush's copy refreshes to the threads costliest first. |
| `VICTORY_WAIT_RESIDENT` | on (=0 turns off) | flag |  |  |  | Waits for named heaps or blocks to be resident on the card. |
| `VICTORY_WALLS_TRACE` | off | number |  |  |  | Logs read requests that fail (1) or all requests (2). |
| `VICTORY_WARM_SECONDS` | n/a (no limit) | number |  |  |  | Tool parameter: the shader warm-up stops taking new shaders after this many seconds. |
| `VICTORY_WARP` | off | flag |  |  |  | Uses the WARP software adapter for the shader test host. |
| `VICTORY_WATCH` | off | list |  |  | diag | Logs the 32-bit value at each listed RVA whenever it changes, polled every 5 ms. |
| `VICTORY_WATCHDOG_HOLD` | on (=0 turns off) | flag |  |  |  | Holds the title's watchdog byte so the watchdog stays paused; =0 leaves it alone. |
| `VICTORY_WATCHDOG_HOLD_MAX_S` | 900 | number |  |  |  | Seconds the watchdog pause is kept without a heartbeat. |
| `VICTORY_WATCH_LARGE_XMEM` | off | flag |  |  |  | Gives the title's blocks of 1 MiB and more from `XMemAllocDefault` a write-watched reservation (between 4 GiB and 2^40, like its other 64-bit memory), so the GPU side asks the kernel which pages were written instead of copying the block whole (the 21 MB upscaler buffer: 6.5 ms and 41 MB a frame "copied whole" in the processes where `quiet_skip` fails). Read once; compared between processes. Port of shreezzz's VIctory-NVIDIA #1038, with the address bounds Exopus asked for. |
| `VICTORY_WAVE_DEEP_SKIPS` | on (=0 turns off) | flag | yes |  |  | Wave-wide form that drops skips with scalars followed, so more code runs in every lane. |
| `VICTORY_WAVE_LOCAL` | on (=0 turns off) | text | yes |  |  | Gives each host wave its own wave for programs not run by every lane; =idiom refuses approximate ones. |
| `VICTORY_WAVE_OWN_BITS` | on (=0 turns off) | flag | yes |  |  | Allows a wave OR where not every lane runs, using each lane's own bits. |
| `VICTORY_WAVE_WHY` | off (any value prints) | flag |  |  |  | Prints to stderr why a program keeps 64-lane waves and why waterfall checks failed. |
| `VICTORY_WAVE_WIDE` | off for most programs, on for two named kernels (=1 forces on, =0 off) | flag | yes |  |  | Translates console waves run as two host waves in the wave-wide form with barriers in branches. |
| `VICTORY_WAVE_WIDE_AUTO` | off (=1 every program, or a comma list of program names) | list | yes |  |  | Takes the wave-wide form before "without skips, scalars followed" where the forms before it stopped at an exchange of the two host waves that only a deep-only skip keeps from some lane (the cascade-shadow rasters); refused where it puts a translator group barrier in a loop. |
| `VICTORY_WAVE_WIDE_FIRST` | a built-in list of one program | list | yes |  |  | Comma list of program names that take the wave-wide form before any approximate local form. |
| `VICTORY_WHOLE_CHECK` | off | flag |  |  |  | Diagnostic that checks a long range before it is copied whole. |
| `VICTORY_WIDE_BACK` | on (built-in default 1; =0 turns off) | flag |  |  |  | Copies back across ranges on several threads. |
| `VICTORY_WIDE_FILLS` | on if 4 or more CPUs (=0 turns off) | flag |  |  |  | Fills of several copies are done on several threads. |
| `VICTORY_WIDE_MERGE` | off | flag |  |  |  | Merges landings into title memory on several threads. |
| `VICTORY_WIDE_REFRESH` | on if 4 or more CPUs (0 off, 1 forces) | flag |  |  |  | Gives each copy's refresh to its own thread. |
| `VICTORY_WIDE_SPIN` | 0 | number |  | live |  | Microseconds a wide wait spins awake before sleeping; 0 does not spin. |
| `VICTORY_WIDE_TEXTURES` | on if 4 or more CPUs (=0 turns off) | flag |  |  |  | Uploads textures of several lists on several threads. |
| `VICTORY_WIDE_WHOLE` | on (=0 turns off) | flag |  | live |  | Copies a long unwatched range whole on several threads. |
| `VICTORY_WINDOWS_BAR_MB` | 0 (off) | number |  |  |  | Megabytes of window memory mapped through the bus and filled by the CPU. |
| `VICTORY_WINDOWS_OWN` | off | flag |  |  |  | Windows in the air read the bytes they were prepared with. |
| `VICTORY_WINDOW_KB` | 4096 (1024 with VICTORY_RAM_LEAN=2) | number |  |  |  | Grid in KiB that title-memory windows are cut on; a power of two from 64 to 4096. |
| `VICTORY_WINDOW_MAX` | n/a (no limit) | text |  |  |  | Limits the title's window size to WIDTHxHEIGHT when smaller than the display. |
| `VICTORY_WINDOW_TAILS` | off (on with VICTORY_RAM_LEAN=2) | flag |  |  |  | Windows of asks without a length begin at the ask instead of the grid line. |
| `VICTORY_WRITE_SITES` | off (unset) | path |  |  |  | File naming title write sites that are made to look first (flags, registers). |
| `VICTORY_WRITE_SKIP` | on (=0 turns off) | flag |  |  |  | Skips comparing ranges known not to have been written since the last pass. |
| `VICTORY_XMA_DUMP` | off (unset) | path |  |  | diag | Directory the audio decoder saves its input context words and streams to. |
| `VICTORY_XMA_PRIORITY` | highest | text |  |  |  | Audio decoder thread priority: normal, critical, anything else highest. |
| `VICTORY_XOR_READBACK` | on (=0 turns off) | flag |  |  |  | (integ/validation-23) Lands the read-back of textures the title reads on the CPU in the XOR tile modes too. |
| `VICTORY_XOR_UPLOAD` | on (=0 turns off) | flag |  |  |  | Uploads XOR-swizzled textures by the tile configuration; =0 refuses them. |

Names that appear only in tests, as placeholders or in comments (no behaviour): `VICTORY_A`, `VICTORY_B`, `VICTORY_BCPACK_CASES`, `VICTORY_BCPACK_TABLES`, `VICTORY_BENCH_CBV`, `VICTORY_BENCH_CONTAINER`, `VICTORY_BENCH_CS`, `VICTORY_BENCH_DUMP`, `VICTORY_BENCH_FILE`, `VICTORY_BENCH_GROUPS`, `VICTORY_BENCH_HOLES`, `VICTORY_BENCH_N`, `VICTORY_BENCH_REPS`, `VICTORY_BENCH_S`, `VICTORY_BENCH_THREADS`, `VICTORY_BENCH_USER`, `VICTORY_BENCH_VIEWS`, `VICTORY_BENCH_WARP`, `VICTORY_BENCH_WAVE`, `VICTORY_BLOCK_WAY`, `VICTORY_CONSOLE_DXC`, `VICTORY_DIFF_ONLY`, `VICTORY_EXIT_CHILD`, `VICTORY_HOST_LOCK_CHILD`, `VICTORY_NONE_SUCH`, `VICTORY_NO_SUCH_LIST`, `VICTORY_PROBE`, `VICTORY_REF_PS`, `VICTORY_REF_VS`, `VICTORY_SAVE_CHILD`, `VICTORY_SAVE_OP`, `VICTORY_SAVE_STOP`, `VICTORY_TEST_D3D_DEBUG`, `VICTORY_TEST_DUMP`, `VICTORY_TEST_KEEP`, `VICTORY_TEST_ROUNDS`, `VICTORY_TILES_TEST_CARD`, `VICTORY_TRAP_LOG`, `VICTORY_XMA_REFERENCE`, `VICTORY_XMA_STREAMS`.
