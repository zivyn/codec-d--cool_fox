//! Who gets the card's tiles first, and who gives them back (levels on
//! demand, `crate::tiles`).
//!
//! The card's room for texture levels is one number, `resource::headroom`,
//! and the size of level a texture may be given falls with it
//! (`resource::level_cap_for`). Past the line the cap is a level of one tile,
//! so every texture uploaded then shows a small mip for good, and which ones
//! are those is a race of the title's streaming against our allocations (the
//! banding of IMG1: 3 to 6 thousand levels held back, a different set in
//! each run). Nothing said which textures are looked at.
//!
//! What is known on the CPU is which textures a draw reads through a table
//! (`texture::upload_from`: each one a list reads is noted on its host copy,
//! [`crate::tiles::Tiled::note_sampled`]). Shaders that pick a texture by an
//! index they compute give nothing away, so the policy never judges a texture
//! by their silence:
//! - Hot: read through a table within [`HOT_MS`]. It may be given levels as
//!   if the card had [`BONUS`] more room ([`hot_cap`]), as long as the card
//!   is not [`FLOOR`] past its line, and its waiting levels are asked for
//!   first, the largest shortfall first ([`upgrade_order`]).
//! - Cold: read through a table before, and not for [`COLD_MS`]. Its large
//!   levels go back to the pool when a hot texture waits and the card has
//!   no room ([`reclaim_plan`]), the longest idle first; never one whose
//!   tiles were given less than [`HOLD_MS`] ago (so what a pass upgrades is
//!   not taken back by the next).
//! - The rest (never read through a table: those picked by index among them):
//!   left as they are. `VICTORY_TEXTURE_PRIORITY=2` also counts as cold those
//!   of them that no upload has touched for [`UNTOUCHED_MS`].
//! - Levels the title no longer holds, whichever the texture, are dead weight
//!   and go back always (`Tiled::dead`).
//!
//! The between of [`HOT_MS`] and [`COLD_MS`] is the hysteresis: a texture is
//! neither upgraded nor reclaimed in it.
//! `VICTORY_TEXTURE_PRIORITY=0` turns all of it off.

/// A texture read through a table this long ago or less is hot.
pub(crate) const HOT_MS: u64 = 10_000;
/// A texture read through a table before, and not for this long, is cold.
pub(crate) const COLD_MS: u64 = 30_000;
/// Tiles given less than this long ago are not taken back.
pub(crate) const HOLD_MS: u64 = 20_000;
/// With `VICTORY_TEXTURE_PRIORITY=2`: a texture never read through a table
/// that no upload touched for this long is cold.
pub(crate) const UNTOUCHED_MS: u64 = 120_000;
/// How much more room a hot texture is given as if the card had.
pub(crate) const BONUS: i64 = 1536 << 20;
/// A hot texture is given no more levels once the card is this far past its
/// line.
pub(crate) const FLOOR: i64 = -(64 << 20);
/// The room (bytes under the line) the reclaim works toward while a hot
/// texture waits.
pub(crate) const WANT_ROOM: i64 = 192 << 20;
/// The most bytes of tiles one pass takes back from cold textures.
pub(crate) const RECLAIM_PASS_BYTES: u64 = 48 << 20;
/// Textures looked at by one pass.
pub(crate) const SCAN_A_PASS: usize = 512;
/// The most bytes of tiles the textures one pass asks for again lack.
pub(crate) const UPGRADE_PASS_BYTES: u64 = 256 << 20;
/// Levels of a cold texture of at most this many bytes of tiles stay: the
/// texture then shows a small mip instead of nothing.
pub(crate) const KEEP_BYTES: u64 = 256 << 10;

/// `VICTORY_TEXTURE_PRIORITY`: 0 off, 2 the cold of [`UNTOUCHED_MS`] too,
/// else 1.
pub(crate) fn mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        match std::env::var("VICTORY_TEXTURE_PRIORITY")
            .as_deref()
            .map(str::trim)
        {
            Ok("0") => 0,
            Ok("2") => 2,
            _ => 1,
        }
    })
}

/// `VICTORY_TEXTURE_RECLAIM=0`: tiles are never taken back out of a texture
/// (hot textures are still given levels first). The null mapping of a level
/// that has tiles is what a card test must have passed.
pub(crate) fn reclaim_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_RECLAIM").is_none_or(|v| v != "0"))
}

/// Whether textures are given tiles by priority: levels on demand are on,
/// and the switch is not 0.
pub(crate) fn enabled() -> bool {
    mode() > 0 && crate::tiles::wanted()
}

/// How a texture stands. In the order tiles are given in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Heat {
    Hot,
    Unknown,
    Cold,
}

/// The heat of a texture read through a table `sampled` ms ago (`None`:
/// never) and touched by an upload `touched` ms ago, under `mode`.
pub(crate) fn heat(mode: u8, sampled: Option<u64>, touched: u64) -> Heat {
    match sampled {
        Some(age) if age <= HOT_MS => Heat::Hot,
        Some(age) if age >= COLD_MS => Heat::Cold,
        Some(_) => Heat::Unknown,
        None if mode >= 2 && touched >= UNTOUCHED_MS => Heat::Cold,
        None => Heat::Unknown,
    }
}

/// The cap on the size of a level (`resource::level_cap_for`) for a texture
/// of `heat` when the card is `headroom` bytes under its line and `global` is
/// the cap for everyone: a hot one gets the cap of a card with [`BONUS`] more
/// room, until the card is [`FLOOR`] past its line.
pub(crate) fn hot_cap(global: u64, headroom: i64, heat: Heat) -> u64 {
    if heat != Heat::Hot || headroom <= FLOOR {
        return global;
    }
    global.max(crate::resource::level_cap_for(
        headroom.saturating_add(BONUS),
    ))
}

/// A texture waiting for levels it holds: what it is, its heat, the bytes of
/// tiles of the levels it lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Waiting {
    pub(crate) id: u64,
    pub(crate) heat: Heat,
    pub(crate) deficit: u64,
}

/// Hot first, then the rest; the largest shortfall first within each; ties
/// by `id`, so that a pass is the same on the same input.
pub(crate) fn upgrade_order(waiting: &mut [Waiting]) {
    waiting.sort_unstable_by(|a, b| {
        a.heat
            .cmp(&b.heat)
            .then(b.deficit.cmp(&a.deficit))
            .then(a.id.cmp(&b.id))
    });
}

/// How many of `waiting`, in order, one pass takes: at most `count` and, past
/// the first, as many as `bytes` of shortfall hold (one is always taken).
pub(crate) fn upgrade_batch(waiting: &[Waiting], count: usize, bytes: u64) -> usize {
    let mut taken = 0;
    let mut sum = 0u64;
    for w in waiting.iter().take(count) {
        if taken > 0 && sum.saturating_add(w.deficit) > bytes {
            break;
        }
        sum = sum.saturating_add(w.deficit);
        taken += 1;
    }
    taken
}

/// A texture that holds tiles: what it is, its heat, how long it has been
/// idle (since read through a table, else since touched), how long ago its
/// tiles were given, and the bytes its large levels hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Holder {
    pub(crate) id: u64,
    pub(crate) heat: Heat,
    pub(crate) idle: u64,
    pub(crate) given_ago: u64,
    pub(crate) big: u64,
}

/// The ids of the textures to take large levels from, so that `need` bytes
/// come back: cold ones only, tiles not given within [`HOLD_MS`], the longest
/// idle first, and no more than `per_pass` bytes (but one texture, if
/// `need` is more than 0). Returns the ids and the bytes they hold.
pub(crate) fn reclaim_plan(holders: &mut [Holder], need: u64, per_pass: u64) -> (Vec<u64>, u64) {
    let goal = need.min(per_pass);
    let mut chosen = Vec::new();
    let mut bytes = 0u64;
    if goal == 0 {
        return (chosen, bytes);
    }
    holders.sort_unstable_by(|a, b| {
        b.idle
            .cmp(&a.idle)
            .then(b.big.cmp(&a.big))
            .then(a.id.cmp(&b.id))
    });
    for h in holders.iter() {
        if bytes >= goal {
            break;
        }
        if h.heat != Heat::Cold || h.given_ago < HOLD_MS || h.big == 0 {
            continue;
        }
        chosen.push(h.id);
        bytes += h.big;
    }
    (chosen, bytes)
}

/// How many bytes the reclaim is asked for, when the card is `headroom` under
/// its line and hot textures lack `hot_deficit` bytes of tiles: none without
/// a hot one waiting, else what brings the room to [`WANT_ROOM`], and no more
/// than they lack.
pub(crate) fn reclaim_need(headroom: i64, hot_deficit: u64) -> u64 {
    if hot_deficit == 0 || headroom >= WANT_ROOM {
        return 0;
    }
    u64::try_from(WANT_ROOM.saturating_sub(headroom))
        .unwrap_or(0)
        .min(hot_deficit)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;

    #[test]
    fn heat_follows_the_table_reads_with_a_gap_between() {
        assert_eq!(heat(1, Some(0), 0), Heat::Hot);
        assert_eq!(heat(1, Some(HOT_MS), 0), Heat::Hot);
        // Between hot and cold nothing is done to the texture.
        assert_eq!(heat(1, Some(HOT_MS + 1), 0), Heat::Unknown);
        assert_eq!(heat(1, Some(COLD_MS - 1), 0), Heat::Unknown);
        assert_eq!(heat(1, Some(COLD_MS), 0), Heat::Cold);
        // Never read through a table: silence says nothing, unless asked.
        assert_eq!(heat(1, None, 10 * UNTOUCHED_MS), Heat::Unknown);
        assert_eq!(heat(2, None, UNTOUCHED_MS - 1), Heat::Unknown);
        assert_eq!(heat(2, None, UNTOUCHED_MS), Heat::Cold);
        // A hot texture is hot whatever the mode says of the silent ones.
        assert_eq!(heat(2, Some(5), 10 * UNTOUCHED_MS), Heat::Hot);
    }

    #[test]
    fn a_hot_texture_is_given_the_cap_of_a_roomier_card_until_the_floor() {
        let at = |mb: i64| mb * (1 << 20);
        let global = crate::resource::level_cap_for(at(-50));
        assert_eq!(
            global,
            64 << 10,
            "past the line the card takes one-tile levels"
        );
        // Hot: the cap of a card with 1.5 GB more room.
        assert_eq!(hot_cap(global, at(-50), Heat::Hot), 16 << 20);
        assert_eq!(hot_cap(global, at(0), Heat::Hot), u64::MAX);
        // The others keep the cap of everyone.
        assert_eq!(hot_cap(global, at(-50), Heat::Unknown), global);
        assert_eq!(hot_cap(global, at(-50), Heat::Cold), global);
        // Past the floor nothing more is given, hot or not.
        assert_eq!(hot_cap(0, FLOOR, Heat::Hot), 0);
        assert_eq!(hot_cap(0, at(-500), Heat::Hot), 0);
        // Never less than the cap of everyone.
        assert_eq!(hot_cap(u64::MAX, at(-10), Heat::Hot), u64::MAX);
    }

    #[test]
    fn hot_textures_with_the_largest_shortfall_are_upgraded_first() {
        let w = |id, heat, deficit_mb| Waiting {
            id,
            heat,
            deficit: deficit_mb * MB,
        };
        let mut list = vec![
            w(1, Heat::Unknown, 64),
            w(2, Heat::Hot, 4),
            w(3, Heat::Cold, 90),
            w(4, Heat::Hot, 16),
            w(5, Heat::Unknown, 64),
            w(6, Heat::Hot, 16),
        ];
        upgrade_order(&mut list);
        let ids: Vec<u64> = list.iter().map(|w| w.id).collect();
        assert_eq!(ids, [4, 6, 2, 1, 5, 3]);
        // The same input always gives the same pass.
        let mut again = list.clone();
        again.reverse();
        upgrade_order(&mut again);
        assert_eq!(again, list);
    }

    #[test]
    fn a_pass_takes_what_its_count_and_bytes_allow_and_at_least_one() {
        let w = |id, deficit_mb| Waiting {
            id,
            heat: Heat::Hot,
            deficit: deficit_mb * MB,
        };
        let list = [w(1, 40), w(2, 40), w(3, 40), w(4, 1)];
        assert_eq!(upgrade_batch(&list, 128, 100 * MB), 2);
        assert_eq!(upgrade_batch(&list, 1, 100 * MB), 1);
        assert_eq!(upgrade_batch(&list, 128, 1), 1, "one is always taken");
        assert_eq!(upgrade_batch(&list, 128, u64::MAX), 4);
        assert_eq!(upgrade_batch(&[], 128, u64::MAX), 0);
    }

    #[test]
    fn only_cold_textures_give_tiles_back_the_longest_idle_first() {
        let h = |id, heat, idle_s: u64, given_s: u64, big_mb| Holder {
            id,
            heat,
            idle: idle_s * 1000,
            given_ago: given_s * 1000,
            big: big_mb * MB,
        };
        let mut holders = vec![
            h(1, Heat::Cold, 40, 100, 8),
            h(2, Heat::Cold, 300, 100, 8),
            h(3, Heat::Hot, 1, 100, 50),
            h(4, Heat::Unknown, 500, 100, 50),
            h(5, Heat::Cold, 90, 100, 8),
            h(6, Heat::Cold, 900, 5, 8),
            h(7, Heat::Cold, 80, 100, 0),
        ];
        let (ids, bytes) = reclaim_plan(&mut holders, 20 * MB, 48 * MB);
        // 2 (300 s) and 5 (90 s) and 1 (40 s) are needed to cover 20 MB of
        // 8 MB each; 6 was given 5 s ago, 3 and 4 are not cold, 7 has none.
        assert_eq!(ids, [2, 5, 1]);
        assert_eq!(bytes, 24 * MB);
        // One pass takes no more than its bytes allow (but the texture that
        // crosses them).
        let (ids, bytes) = reclaim_plan(&mut holders, 100 * MB, 10 * MB);
        assert_eq!((ids, bytes), (vec![2, 5], 16 * MB));
        // Nothing asked, nothing taken.
        assert_eq!(reclaim_plan(&mut holders, 0, 48 * MB), (vec![], 0));
    }

    #[test]
    fn the_reclaim_is_asked_for_only_while_a_hot_texture_waits_and_the_room_is_short() {
        let mb = |n: i64| n * (1 << 20);
        assert_eq!(reclaim_need(mb(-300), 0), 0, "no hot texture waits");
        assert_eq!(reclaim_need(mb(500), 1 << 30), 0, "the room is there");
        assert_eq!(reclaim_need(mb(192), 1 << 30), 0);
        assert_eq!(
            reclaim_need(mb(-300), 1 << 30),
            492 << 20,
            "up to the wanted room"
        );
        assert_eq!(
            reclaim_need(mb(-300), 10 << 20),
            10 << 20,
            "no more than they lack"
        );
    }
}
