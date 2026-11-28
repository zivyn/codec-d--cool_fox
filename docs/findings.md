# What was learned on the RX 7900 XTX (2026-11-23 to 2026-11-25)

One PC: i9-12900K (8 fast cores, 8 slow ones, 24 threads), 64 GB, Radeon RX 7900 XTX
24 GB, Windows 11, 1440p, Series X profile. Figures are of diagnostics builds with a
screenshot a second. Scenes are not the same from run to run, so read differences
under about 5 % as nothing. What a switch does is in the comment where it is read.

## Where a frame goes

Median of whole runs, same part of the prologue: 280 ms, then 124, 103, 75, now 46-65.
On the water (first boat mission) 46-53 ms.

The title needs 33 ms. Of a 55 ms frame on the water:

- **The thread that prepares submissions: 48 ms.** It is the wall. Steps of the lists
  18 (refreshes of host copies 10), steps done on the card 7, waiting for descriptor
  records 6, asking the write watch 5, copies in the card's memory 4, the title's bytes
  under shader writes 4.
- **The card: about 41 ms busy**, the next wall.
- **List calls: 36,000 a frame, 230 ms of work** spread over eight threads. In a street
  chase 67,000 calls and 430 ms.
- Descriptor records copied as plain bytes: 400,000 slots looked at, 40,000 mirrored a
  frame, most of them empty.
- Landing what shaders wrote: 16 MB a frame put as dwords, on a thread of its own.

## The one idea behind most of it

On the console the CPU and the graphics processor share memory. Here they do not, so
everything one writes and the other reads must be noticed and moved. Every large cost
is one of four: **noticing** writes of the title (write watch, guards), **noticing**
writes of shaders (hashes on the card, compares on the CPU), **moving** them, and
**waiting** until an order is safe. Gains came from making each of them narrower
(dwords, not pages), wider (threads, AVX2) or later (on the card, behind the frame),
in that order of size:

| What | Frame before | after |
|---|---|---|
| Descriptor records of draws' tables left to the draws | 280 ms | about 124 |
| Our hash passes over card copies, 64 threads a piece | card 79 ms | 66 |
| Preparing on a thread of its own, refreshes and records spread | 103 | 75 |
| List calls written down and translated by eight threads (`VICTORY_THIN`) | 75 | 68 |
| Submissions joined, card steps, textures sent ahead | 68 | about 63 |
| Title bytes under shader writes compared with AVX2 on six threads | 14 ms of it | 4-7 |
| Stores into guarded memory looked at first, fewer guards reset | 69 ms of threads' time | about 6 |

(Each figure is from the run in which it was measured; they overlap and do not add up.)

## Faults found, and what they were

- **Boats sank, the sea plane went under.** The ocean's height and displacement maps are
  textures that compute passes write and the title's physics sample on the CPU. They were
  never brought back to title memory. Fixed by Geckomacabre's read-back as merged in
  fxceless/victory-fork f5c03f3b; on by default here (`VICTORY_TEXTURE_BACK=0`: off).
  With it the boat stays up through a whole run; without it sank in every one.
- **A device loss about every 16 minutes**: a page fault inside a texture made in the same
  tenth of a second. A slot showed it before its upload had run.
  `VICTORY_SHOW_AFTER=1`; none in the sessions since (likely fixed, not proven).
- **A rare lost write** (6 of 200 runs of a stress test): a view was mapped between our
  look at its guards and our lock, and views were opened one after another. None in 603
  runs after.
- **Blocky patches**: indirect dispatches cut at 65,535 groups.
- **Square holes and dashed edges in shadows**: pixel shaders that read their quad's
  neighbours with wave reads; on AMD helper lanes take no part in them.
- **Cube reflections too blurred, wrong materials**: DXIL's `Log` is base 2, and
  normalized buffer codes were one step off.
- **Every ray missed**: the dispatch flag that says counts are threads, not groups.

## Measured and not worth it

- Unordered early pipelines for everything (AMD's compiler crashed).
- Series S profile, smaller textures, a second drawing queue, `ASYNC_SUBMIT` off.
- Mapping pages on sixteen threads: no more pages mapped, frames longer.
- Scalar buffer loads four dwords a call (`VICTORY_SHADER_OPT=127`): no gain on this card.
- Wave sizes, fused multiply-adds, fast reciprocals, raw picked buffers, leaving out
  barriers: nothing to 0.1 ms each. Exact RDNA float rules (`VICTORY_RDNA_FLOAT`): 0.8 ms.
- Running the console's shader code natively on the card: the Windows driver takes no raw
  code, the two generations differ, and a Vulkan host layer ends at the same compiler. What
  is left to gain from shader code was guessed at under 1 ms.
- An on-screen panel of numbers (`VICTORY_PANEL`): it helps finding where a frame goes but
  costs some, so it is off and F3 shows it.

## What is next

1. The preparing thread: split what one submission does over two threads in a row; ask
   for the next submission while this one is sent; view the card's own buffers where fills
   copy them now.
2. The card: small buffers' copies in its own memory (`VICTORY_CARD_BUFFERS_MIN_KB=64`,
   after fxceless c36971b2; not measured here yet), and the passes that run for nothing
   without rays.
3. Stalls: 270-660 s of a session were full of frames over 200 ms, almost all while new
   pipelines were made. `victory warm`, and a quarter of the workers for the slow ones.
4. Outdoor flicker: suspected to be one bad indirect record that uses up its list's
   budget, so that every later draw of it is empty. The log's frame line counts them;
   `VICTORY_INDIRECT_REST=0` is the experiment.
5. Memory, 40-46 GB private: lists' allocators 9 GB, windows 7, uploads 4
   (`VICTORY_MEMORY_MAP=1` lists it).

## The other repositories, compared

Read up to 2026-11-25: Marvisak/VIctory, fxceless/victory-fork, DexNom/VIctory,
Belobug/VIctory-Belobug-AMD, FQY55kai/ZJOU-VICTORY. What was taken is in the history,
each commit naming its source. In short:

- Marvisak's commits hardly apply any more; nearly all of what they fix is here, often
  another way. Taken: nearest normalized codes, d16 views, shader model flags.
- fxceless: the base. Newest taken: texture read-back, small buffers on the card, hashes
  over a buffer's own bytes.
- DexNom: thread counts of ray dispatches, cube gradients, joined sparse ranges.
- Belobug: pipeline workers for screen passes, the ambient stand-in's idea, pipeline and
  constant buffer bookkeeping (romainhedouin).
- ZJOU: render targets not set again; some shader corrections are being looked at.

# What was learned on an RTX 5070 Ti Laptop (2026-11-27 to 2026-11-28)

One PC: i9-14900HX, 32 GB, RTX 5070 Ti Laptop 12 GB, Windows 11 26100, Series X profile
(and Series S for the DLSS runs), release and diagnostics builds. Windows are aligned by
index and kept only where `draws/frame` agrees within 5 %, and medians are used, never
means — the same run's own fps average has read 7.76 where its median read 2.44.

## Where a frame goes here

Series X, in the world: **383-410 ms median**. Of it, `preparing` on the submitting thread
161-375 ms, the card 177-290 ms, copying back 57-85 ms, and the three overlap.

Inside `preparing`, the largest single item is a settle waiting for the card — **226-326 ms**
— and a microsecond breakdown by what was read (added while measuring this) shows it is
**all one thing: a fill of upload memory**. The blame line then names the blocker: a fill of
`0x20` bytes waits for shader writes declared over `0x11600000+0xae00000` — 174 MB. That
range is the view's own extent (`descriptor/host.rs` `buffer_copy`), not an over-wide guess,
so tightening it is not available; `VICTORY_ROOT_UAV_MB` only bounds the root path and
changes nothing here (310.5 / 334.5 / 341.8 ms at default / 4 MB / 1 MB).

`card_steps.rs` describes the same phenomenon from the AMD side (a 480-byte constant buffer
waiting on eleven skinning dispatches that write through one 174 MB heap view).
`VICTORY_FILL_TITLE=1` moves the wait — read-wait 310 -> 226 ms and preparing 236 -> 117 ms
— but the frame median stays at 404 ms: the buckets trade places.

## Five things that did not move this frame, and the numbers

Each of these shortens work the frame does not wait on here. Reporting them so the next
person does not spend the days again:

| What was tried | What it changed | Frame |
|---|---|---|
| Leaving both shadow masks out | GPU -44 ms, fps 5.02 -> 6.46 | interiors and hair go dark, so not a fix |
| Halving the non-hair mask's dispatch | nothing measurable | the switch was not reaching that pass (indirect), retracted |
| Another branch's pipeline group (`-Fast`) | GPU -21 % (389 -> 308 ms) | 2.40 -> 2.40 fps |
| Striping the mapping lock, 4 stripes | GPU -24 %, and submit lock wait 202 -> 23 ms | 2.44 -> 2.36 fps; the win is only in **loading** frames: 499 -> 258 ms |
| DLSS Super Resolution, once the files were complete | 1.00 upscalings a frame, 375 done, none failed | 400 ms against 399 without it |
| The title's own upscaler set lower (`fsrQuality` 2 -> 4) | internal resolution clearly lower | 406 ms against 400 |

The last two make the point best: **lowering what the card renders changes nothing**, so
this frame is not limited by pixel work, and the `on the GPU` figure is not pure execution
time either — it moves with the queue.

## Faults found

- **No DLSS at all, for days, without a word.** `victory.exe upscaler-check` said
  "DLSS NOT supported ... feature missing" against a Streamline folder that had
  `sl.dlss_g.dll` but no `sl.dlss.dll` or `nvngx_dlss.dll`. `victory.exe get-dlss` fixed it
  and the same check then said "DLSS supported, frame generation supported". Worth having
  the check say which file is missing, not just which feature.
- **A MiB count that wrapped.** `VICTORY_UPLOAD_QUEUE` parsed a MiB count with
  `mebibytes << 20`, so a large enough setting became a small gate. `checked_mul` now, with a
  test, found by ANDREANIADS reading the pull request.
- **A pad that was never there.** The title's XInput slot asked the system `XInputGetState`,
  which answers 1167 on all four slots for a GameInput synthetic pad; loading
  `Microsoft GameInput\x64\XInput1_3.dll` first makes the pad appear.

## The one lever left

Since neither GPU work nor internal resolution moves the frame, what is left is the
submission pipeline itself: every submission is prepared, sent, waited for and copied back
before the next is looked at (`VICTORY_LATE_WAIT` off by default, and its two stages moved
preparing 105 -> 69 ms while the frame stayed put). Two frames in flight end to end —
`settle`, the relay that lands flights, `copied_read`, the queue behind — is the next thing
worth measuring, and the only one of these that can plausibly reach 15 fps at Series X.

# What was learned on an RTX 5080 (2026-11-27)

One PC: Ryzen 9 9950X3D (16 cores, 32 threads), 64 GB, RTX 5080 16 GB (driver 621.24),
Windows 11 26300, NVMe SSD. Series X profile, 2560x1440 output, DLSS Performance, frame
generation off, diagnostics build at `df2daaf`, pipelines made ahead with `victory warm`.
Two runs over the same stretch of the title, from the same save: the cutscenes of Lucia's
arrest, the café and the prison interview, then Jason's introduction on the boat and a
minute standing still on it. Run A with the launcher's settings, run B the same plus
`VICTORY_BVH_BUILDER=1` (and `VICTORY_BVH_TRACE=1`). Figures are the log's ten-second lines;
"steady" is the last 70 s of each run, on the boat, where `draws/frame` agrees within 5 %.

## Without the builder: the card waits for us

| Run A | Median of the run | Steady (boat) |
|---|---|---|
| Frames a second | 35.2 (p10 26.1, p90 45.2) | 34-38 |
| Frame | 28.4 ms | 26-29 ms |
| `preparing` | 21.1 ms | 20-22 ms |
| The card busy, by its own clock | 71 % | 68-87 % |
| Draws dropped without a host pipeline | 127 a frame | 80-90 |

The card is idle 8-9 ms of a frame, and the log names that time as "while the submitting
thread was working (the card idle behind us)". On this card the frame is the submitting
thread's, as on the 7900 XTX.

## With the builder: the cost is on the CPU here, not on the card

| Steady, on the boat | A | B (`VICTORY_BVH_BUILDER=1`) |
|---|---|---|
| Frames a second | ~35 | ~23.5 |
| Frame | ~28 ms | ~42 ms |
| `preparing` | 20.5 ms | 31 ms |
| The submitting thread worked | 26.5 ms | 44.4 ms |
| The card busy | 68-87 % | 62-84 % |
| The card idle behind us | ~8 ms | 8-15 ms |
| Copies (frame line) | 10.0 ms, 37 MB | 15.1 ms, 41 MB |
| Copying back (relay) | 5.2 ms | 9.1 ms |
| Landing: merging what was fetched | 1.6 ms | 4.9 ms |
| Landing threads waiting for the title's bytes going in | 1.1 ms | 9.5 ms |
| Draws dropped without a host pipeline | 80-90 a frame | 1 |

On the 7900 XTX the builder cost some 40 ms of the card a frame (`fork-ray-tracing.md`).
On the RTX 5080 the card keeps its headroom and is still idle behind the submitting thread;
what grows is our side of title memory by address (23,810 pages, 1.5 GB in 25 arenas, about
24 pages a frame touched with no copy): copies, copying back and landing. So here 30 fps
with the title's ray tracing is the same problem as 30 fps without it, the submitting
thread, plus about 10 ms of landing and copies.

The 127 draws a frame dropped in run A are 1 in run B: they are passes that need the
builder's pipelines.

## The first minutes with the builder

Run B was made twice; the second time after a `victory warm` that found nothing left to
make (88,605 pipelines known, all made before). Both times the first three minutes of the
cutscenes ran at 1-4 frames a second (frames of 270-930 ms), and both times the boat settled
at the same figures (second run: 21-25 fps, 39-47 ms, `preparing` 27-31 ms). So the slow
start is not pipelines missing from the driver's cache. What the log shows in it:

- The builder unpacking the shipped static structures: 700-1,040 builder dispatches a
  frame, 30-75 MB of packed structures arriving every ten seconds, 500-1,050 pages a frame
  touched that had no copy yet. After about two minutes it falls to 180-280 dispatches.
- Pipelines still being made: about 1,100 in flight at the start, drained after 3.5 min
  (run A, same build and book: about 830, drained in one minute).
- The card 6-20 % busy and, of a 270 ms frame, about 190 ms "while the title had not
  submitted": the title itself is not giving us work then. What it waits for is not shown
  by these lines.

Cutscene audio ran on meanwhile while the picture stood still for seconds (for less time in
the second run). The picture, both times: blown-out light and black patches, more broken
than without the builder; milder in the second run.
## The structures' sizes

Of the copies of dynamic structures `VICTORY_BVH_TRACE` recorded, 175 of 2,972 (first run)
and 63 of 2,967 (second) were of a structure whose compacted size it had seen arrive; it saw
each size arrive one frame after it was asked (`0x0 -> 0x21300`). Whether the other copies
used a stale size, or the trace does not tie them to their ask, is not settled by these runs.

## Shaders on 32-lane waves

`victory shaders` with `VICTORY_HOST_WAVE=32`: 25,760 in the cache, 225 do not translate:
198 containers without a vertex, pixel, compute or mesh program, 22 wave64 programs with no
wave32 lowering, 5 with a descriptor read only as data. Two of the 22 were created in these
runs (`6b82bc7d22016cb5`, `a10179ab7eda0dc3`, both compute).
