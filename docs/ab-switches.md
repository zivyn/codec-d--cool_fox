# A/B inside one process: `VICTORY_AB`

Two runs of the same build differ by several ms/frame at the window spot (run-to-run sd 3.7 ms over 12 runs,
96 % of it between processes and not inside one: a process sits at its own level for its whole run). An A/B of
two runs therefore needs about ten pairs to show 5 ms. Changing the switch **inside** one run, every few
seconds, cancels that level: both arms are measured by the same process, in the same scene, with the same
core placement.

This is a diagnostics build feature (`victory-diag.exe`; the release build reads the switches from the
environment once, as before).

## Using it

```
VICTORY_AB=<seconds>,<switch>[=<value>][,<switch>..]
VICTORY_AB_START=<seconds>     # first phase begins this long after the world is found (default: at the first present)
VICTORY_AB_SKIP=<seconds>      # left out at the start of each phase (default 1)
VICTORY_AB_BUNDLE=1            # the phases alternate between all the named switches off and all on (a bundle against none)
```

- Phase 0 has every named switch **off** (A), phase 1 has bit 0 on (B), and so on: with one switch A, B, A, B, ...;
  with n switches all `2^n` combinations come one after another. A named switch does not go by its own variable
  any more. `<switch>=<value>`: what it is when on (a number for `VICTORY_REFRESH_NARROW`, `VICTORY_JOIN_RAMP=1`
  and so on; no value: `1`). What is off is what the switch is when its variable is not set.
- The world is found as the pilot's driver finds it: three seconds in a row of 3000-5500 draws per frame and under
  260 ms per frame. `VICTORY_AB_START=70` begins the alternation at world+70 s, the window spot.
- The switches change at the title's present call (`queue::frame_ends`), at any instant for the threads that read
  them: each switch below is read so that this is safe. The first `VICTORY_AB_SKIP` seconds of each phase are not
  counted (the exe marks those seconds `settled 0`).
- Lines in the log: `A/B begins`, one `A/B second: phase N bits B settled S: X ms/frame over F frames, D draws a frame` every
  second, `A/B phase N bits B on [..]: ... mean ...` at each phase's end, and, with one switch, `A/B so far (<switch>): B
  minus A +x.xx ms/frame +-y.yy (95%, n pairs ...)` (each B phase against the mean of the A phases before and after
  it, so a drift that is straight over the three cancels; the interval is Student's t on n-1 degrees). The other
  per-second lines of the log (what the submit thread worked, the card, the CPU) are given to their arm by time with
  `bin\bench1-ab.py <tag>` (outside the repo, in `G:\victory-work\bin`). The pilot kills the process, so there is no summary
  at exit: the last `so far` line is the summary.

### Recipe (window spot)

```
$env:VICTORY_AB = '10,VICTORY_HOLDER_INDEX'; $env:VICTORY_AB_START = '70'; $env:VICTORY_AB_SKIP = '2'
```
(through `pilot2-spot.ps1 -Extra 'VICTORY_AB=10,VICTORY_HOLDER_INDEX;VICTORY_AB_START=70;VICTORY_AB_SKIP=2'`, the run
kept to world+70+240 s with `-EndSec 320`). 240 s of alternation is 24 phases, 11 drift-corrected pairs of 8 settled
seconds. A second of the window spot has a robust sd of ~2.7 ms (11 frames), so a phase mean carries ~1 ms, a pair
~1.2 ms and the 95 % interval of the mean of 11 pairs is about +-0.8 ms/frame: effects of 1.5 to 2 ms show in one run.
Phases of 20 s with 3 s left out make 11 phases (5 pairs, +-2 ms): use them for switches that may need seconds to warm
(`VICTORY_ASK_CACHE`, `VICTORY_VIEW_CACHE`: caches that fill) and compare the means of the two arms for
the first and the last settled seconds of the B phases: a difference between them is a warm-up that the skip does not cover.
Repeat in two or three processes: the effect is measured at one process's level of luck, and a switch that helps only
at one placement should show as different effects in different runs. Pictures and stability are still judged from the
shots of the run (both arms are in every shot; the arm is in the `A/B second` line of the shot's second).

## The switches that may be named

`ab::live_flag!` (off unless `=1`) and `ab::live_value!` (a value) read the phase's value at each use, with
`crate::ab::on(bit)`, an atomic load. Where a decision has more than one read in it, it reads once and keeps the answer.

| switch | read at | why it may change at any instant | tests |
|---|---|---|---|
| `VICTORY_HOLDER_INDEX` | `card_steps::plan`, once per planning (`holder_index_on`) | the index of writers is built in that planning from its own writers, used there, and dropped with it; both ways find the same holder | `the_index_of_writers_finds_what_a_look_at_all_of_them_finds` |
| `VICTORY_STEPS_MERGE_READS` | `card_steps`, once per list of steps (`merge_reads_on`) | the list's reads are recorded in one go, whole or with those that lie together joined; no state outlives the list (the counter line only counts what was joined) | `reads_that_lie_together_are_one_copy` |
| `VICTORY_REFRESH_NARROW` (gap, a number of KiB; `=1` 64) | `host::Batch` (`refresh_narrow_gap`, `narrow_on`), `prefetch::spans_of` | each use makes spans of what it reads or writes for the batch at hand; narrow spans hold what the steps refer to, wide ones are a superset, so a batch that begins one way and ends the other still refreshes what its steps read (`Batch::parts` is taken out at its flush whichever way the switch stands; a gap read as "everything" joins all into one span); the write watch's askings that were not made over the gaps are made by the next wide one (it reports what was written since the last asking of a page) | `narrow_spans_hold_what_the_steps_refer_to_and_the_union_is_as_before` |
| `VICTORY_PIPELINE_PREPARE` | `prefetch::offer` (once), `behind` per item, `prefetch::join` | **join**: a refresh offered while the switch was on is joined by every later preparing, whatever the switch says then (`must_join`: on, or anything was ever offered), so the next preparing never meets copies the prefetch thread is refreshing; `submit` reads the switch once (`early`) for both what it excludes and what it offers; `AHEAD` is set and taken inside one item | `a_join_looks_at_the_gate_after_the_switch_went_off_once_anything_was_offered`, `the_next_preparing_waits_for_what_was_offered` |
| `VICTORY_HEAPS_EARLY` | `behind::Job::round`, once when a submission comes into the line | the number of the round of the heaps' records is stored in the entry when it comes in; what takes it later (`ROUND`) does not look at the switch | `a_submission_is_done_with_the_round_asked_for_what_it_took_along` |
| `VICTORY_JOIN_RAMP` (`=1`, first `VICTORY_JOIN_RAMP_FIRST`) | `behind::ramp_first`, per take-along, present and submission | `RAMP` is set again at every present (when on); a value left from before only limits how many parts a submission takes along up to what it would anyway; off: no limit | `the_ramp_after_a_present_doubles_up_to_the_joins` |
| `VICTORY_ASK_CACHE` | `shadow::loose` (once per ask), `found_most` | what a thread remembers is stamped (`stamp`) and every answer is checked with `still` before and after it is made strong; the table sizes follow the switch and `remember` clears a table whose size is not the switch's | `asks_remembered_by_range_give_the_one_window_while_others_change` and the cache's own |
| `VICTORY_RANGE_VALID` | `shadow::stamp`, `still`, `found_stamp`, `found_still` | the two kinds of stamp are different numbers: the top bit of a stamp says which kind it is, and a stamp is taken as holding only by the kind that made it (`stamped_so`); one of the other kind is made again | `a_stamp_made_the_other_way_never_holds`, `a_buffer_changes_the_stamps_of_its_addresses_only` |
| `VICTORY_VIEW_CACHE` (`1`, or `=2` to compare) | `descriptor::ListViews::begin` | the recording that begins takes the mode (`ListViews::mode`) and keeps it to its end: the kept views' slots are begun again, and views are kept or made fresh, by that one mode; what was kept is checked with `shadow::still` when found, so views kept before a stretch with the switch off are used only if the copies are still as they were | `a_buffer_view_is_kept_from_one_recording_to_the_next` |
| `VICTORY_GUARD_SHARDS` (`=<n>` stripes, at most 64; `=1` is the one lock; named by `VICTORY_AB`, `=64`) | `watch::shards`, once per taking of the guards' locks (`Guards::lock`) | what the guards know of a block (`protected`, `clean`, `absent`, ...) is a set of atomic bits that is the same in both modes; the mode only says which locks a thread takes to change or protect blocks. With `VICTORY_AB` naming the switch every taking goes through a gate (`watch::GATE`): the one-lock arm holds it alone, the stripes arm shares it, so the two never work at once and a thread that began in one arm is finished before one of the other begins; within an arm the locks are taken in one order (the stripes' numbers) and a thread that needs another stripe lets all go first | `no_write_is_lost_when_the_mode_of_the_guards_changes` (threads change the mode between rounds), `no_write_to_views_is_lost_between_askers_in_stripes`, `views_in_other_stripes_are_opened_and_noted_together` |
| `VICTORY_DROP_EARLY` | `list::thin::translate`, once per stream (`drop_early_on`; only with the structure builder off) | a stream decides alone which of its calls are left out and keeps the state it leaves identical (every compute root that a later dispatch can read is set, the last set of each; the pipelines are all kept; at its end every root counts as read), so streams translated with the switch on and off can follow each other on one list | `the_next_dispatch_that_runs_sees_what_it_saw_with_none_left_out`, `stages_that_are_dropped_leave_the_last_set_of_each_parameter` |
| `VICTORY_PIPELINE_BOOST` | `pipeline::make` worker loop, once per pipeline taken | the level is chosen when a worker takes a pipeline (`level_for`: asked by a draw, a worker that is not one of the passes over the screen's, switch on) and set on that thread before it makes it; nothing else reads it, a flip only changes the next pipeline taken | `only_pipelines_taken_from_the_asked_lanes_are_boosted`, `the_lane_a_pipeline_came_from_is_noted_when_it_is_taken` |
| `VICTORY_PIPELINE_WAIT_ADAPT` | `pipeline::make::wait`, once per wait | the budget a wait is given is read once at its start (`scaled`); the share is only moved by waits that were given it, so after a flip off it is not touched, and after one on it is whole again if nothing moved it for 4 s (`permille_at`) | `the_budget_of_the_wait_follows_how_the_waits_end` |
| `VICTORY_STAGING_HOLD_MS` (a number of ms; `=1` is 1 ms: name a value) | `pool::room`, once a second | the hold only says when an empty block past the spare ones is let go (`Pool::reclaim`); the time a block has been empty is noted whatever the switch says, so a flip to 0 lets go of what is past the spare ones at the next look, and a flip on holds what is empty from then on | `empty_blocks_past_the_spare_ones_are_held_for_the_hold`, `a_held_block_that_is_used_begins_its_hold_again`, `the_hold_has_a_cap` |
| `VICTORY_SUBMIT_FRAMES` (`=2`, `=0`) | `behind::frames_most`, at each present | a bound that a present compares the line's presents with when it comes (and while it waits, with the bound it came with); nothing is kept by it: a larger one lets the next present go at once, a smaller one holds it until the line is down to it. The title's own fences are signalled behind the submissions in the line, so a deeper line cannot let it reuse memory a submission has not read. 2 has never been run (the flip is safe, the depth is a new configuration: watch for a hang, `HUNG.txt`) | `a_bound_of_presents_that_changes_is_met_by_the_next_present` |
| `VICTORY_UNWATCHED_FRAMES` (`=2`, then `=4`) | `draw::unwatched_every`, once per pass over the picked buffer views | the pass keeps only when each copy was last refreshed, and only while the value is above 1: with 1 it keeps nothing, so the first pass after a flip from 1 refreshes every copy, and the first after a flip to 1 forgets the notes and refreshes every copy; nothing is lost by the flip. What the value above 1 does is its own risk: a copy not refreshed for `value - 1` frames draws from stale bytes (garbage ended runs at 120): **image risk, stop at the first device removed or garbage, compare the shots of both arms** | `the_refresh_of_unwatched_copies_follows_the_value_at_each_frame` |
| `VICTORY_HEAP_SYNC_BACKOFF` (`=0` against the 2 of the unset variable) | `descriptor::sync_backoff`, for each wait of the heap thread | a length of time; the count of quiet rounds is the thread's own | `early_rounds_back_off_while_nothing_is_found` (with the text of the variable) |
| `VICTORY_WIDE_SPIN` (`=200`) | `shadow::spin`, at each wait of a worker | microseconds a worker stays awake before it sleeps; keeps nothing | (the text is parsed as before) |
| `VICTORY_ASK_EARLY` (`=2`: ms) | `watch::early`, at each asking of a submission | the asking decides whether to note the pages it found for the early thread; the thread, begun at the first note, keeps its pace and asks the pieces noted whatever the switch says, until their counts have halved away (a second or so after the switch goes off: `VICTORY_AB_SKIP=3`). It asks what a submission asks for itself; a page found written is in the journal either way | `the_early_asking_goes_by_the_text_of_the_variable` |
| `VICTORY_RESIDENT_BATCH` (name it `=0`: on in the code) | `host::batch_residency`, once in `resident` and once in `mapped` | each call returns when its own memory is resident or mapped, by a call of the batch or by one of its own (`MakeResident` from any thread; a round trip that began after its memory was made); the two ways may be in use by different threads at the same time | (the text) |
| `VICTORY_MAKE_ASIDE` (name it `=0`: on in the code) | `shadow::made_aside`, once for the buffer asked for | a thread that makes the copy under the lock while another makes one aside makes a second copy of the same bytes; the one that comes second finds the place taken and is let go of, as a copy that lost its place is. Its effect is at the entry of a scene, not at a steady spot | (the text) |
| `VICTORY_PIPELINE_PRIORITY` (name it `=0`: on in the code) | `make::prioritised`, in `wait` and `touch` | a wait gives up or not for itself; a touch moves a queued pipeline up only if on, and one moved earlier keeps its place. Its effect is on a cold cache, at the entry of a scene | (the text) |
| `VICTORY_UPLOAD_HELD_ONLY` | `texture::held_only_copies`, once per recording of an upload | each recording stores its own `rec_*` fields (a recording before a bake always did the other way), and the fill of the buffer after it goes by what the recording said; a texture uploaded with fewer levels is judged again when the title holds more, whatever the switch | (the recording is as before) |
| `VICTORY_LEVELS_RETRY` (name it `=0`: on in the code) | `resource::may_retry`, once per question | a question asked now of the card as it is now; it changes which textures are uploaded again, so a phase may leave to the next what the other way would have done: read the frame time, not the picture of the run (judge the picture in whole runs) | (the pure predicate `retry_allowed` has its own tests) |
| `VICTORY_LAND_TAIL_THREADS` (`=6`) | `land::each`, once per batch (`tail_threads`) | a batch decides for itself where it goes; the wide pool is made with the number that sent that batch there (read once), and only the phase that names the threads sends batches to it | `the_tail_threads_go_by_the_text_of_the_variable` |
| `VICTORY_LEAN_STEPS` | `host::lean_steps`, once per submission (`Batch::new`, `stale_under_flights`, `refresh_front`, the asking) and once per planning of the card's steps (`take_steps`) | each use is a submission's own: its tables are the thread's kept ones, emptied before they are kept and taken empty (a use inside a use takes a new one), so nothing of a submission is in the next; the list of the planning's steps leaves out the refreshes, which the plan never looks at, and `elsewhere_in` answers for them what `elsewhere` did; `Batch::add` remembers the copy it looked up last, forgotten with every change of what is refreshed ahead (`put_ahead`, `forget_ahead`); the slots are made with one hold on the buffer taken by the thread (a slot that shows it takes none) and one look at the heaps, the descriptors and what the slots hold are those of `constant_at`; a flip between two submissions changes none of what they do | `a_lean_batch_adds_what_a_plain_one_adds`, `ranges_that_are_apart_are_looked_through_by_bisection`, `leaving_the_refreshes_out_of_the_steps_changes_no_choice`, `slots_made_with_one_hold_show_the_buffer`, `scratch::tests` |

A switch that is **on** in the code (`VICTORY_RESIDENT_BATCH`, `MAKE_ASIDE`, `PIPELINE_PRIORITY`, `LEVELS_RETRY`, and the ones of
`crates/victory-gpu/src/defaults.rs`) is named with its off value, `VICTORY_AB=10,VICTORY_MAKE_ASIDE=0`: its A phases are the unset
variable (on), its B phases the `=0` (off). Read B minus A with `bin\ab-verdict.py --b off`.

## Not converted, and why

- `VICTORY_STATE_FILTER`: with the filter off the host list's shadow of its roots, vertex and index buffers is not kept up
  (`State::root_changed` returns before it), so turning it on in the middle of a list would compare with a stale shadow
  and leave out calls the list needs. It would need a mode per list and a shadow cleared when it begins, on all
  recording threads at once.
- `VICTORY_KEEP_FAST`: the array of the raw pointers it keeps per slot is made with the descriptor heap, when the switch
  is read (`kept_raw`); turned on later its entries would be those of a time it was not kept up. It needs the array
  always made and cleared at the flip.
- `VICTORY_ALIAS_FAST` is not in this branch (`perf/cut3-mem` has it).
- `VICTORY_TEXTURE_RECLAIM`: with it off the texture pass returns before it finishes the retirements it began (`tiles::finish_retired`)
  and before it trims the tile heaps, so a flip to off leaves levels retired (not shown) with their tiles still mapped for as long as
  it stays off, while a cold texture asked for again may be given new tiles for the same levels, and the later `finish_retired` unmaps by
  a mask of levels that may hold the new tiles. The `=0` of the variable never begins one, so it never met that. Needs the pass to finish
  and trim whatever the switch says, and only the planning of new retirements to follow it (and a test of a texture retired, flipped, given
  again and finished).
- `VICTORY_TEXTURE_PRIORITY` (mode 0/1/2): the mode is the state of the cold and hot marks the texture pass keeps; not touched.
- Anything that changes the translation or the cache key (the pipeline book, the translated programs) and
  anything that is read when an object is made (heaps, copies, pools).

## What is not tested

The tests are of the pieces (the parsing of the switches and values, the world, B minus A with its interval and a
drift that cancels, the join, the stamps, the equivalence of the two ways of the switches that choose between two ways
of the same answer). That a flip leaves the picture and the game's own results as they are is not a test of CI (no game,
no GPU): it is looked at in the run, where both arms are in the same shots, and by the stability of the runs.
