//! Physical pages from sections of at most `VICTORY_SECTION_MB` MiB (unset,
//! 0 or not a number: one section for each request, as before).
//!
//! Port of vilocity 0f44a1706 (voidopcode), whose `xmem.rs` this tree's
//! matches; tests after KainanP ee8b92e14 and Xeltax a8c223149. The finding
//! and the cure are RyanCase94's and CuteBooty's (fxceless/victory-fork#1089,
//! #1274).
//!
//! The title asks for its 6.1 GiB of pages in one call and maps them a 64 KiB
//! page at a time: one section with views by the hundred thousand. Windows'
//! worker that rebuilds large pages trades a section's pages one at a time,
//! walks the section's views for each and holds the section's lock meanwhile,
//! which every map and unmap of the title's pages waits for (vilocity,
//! moving through the world: maps of 114 us and unmaps of 182 us each, 848 ms
//! of mapping calls a second; in sections of 2 MiB 11 and 3 us, 117 ms).
//!
//! What goes by sections sees more of them, and nothing else changes: a run
//! of pages for a mapping or a write ends at a section's end as it did at a
//! request's (`runs`), so a title mapping over a bound is two views and a
//! DirectStorage destination over one is not given a window in place
//! (`page_window`) and is written run by run through `write_physical`. Each
//! section keeps its own whole view (`Section::window`) and is closed, and the
//! GPU side told (`title_memory_section_gone`), when its last page goes.
//!
//! Renderer port 1 (`VICTORY_PORT_CONSTANTS=1`) needs the 32-bit heap's
//! pages to be one stretch of one section. With this switch set they are
//! not: `arrange` warns and leaves them where they are, and the heap
//! stays on the copy path. Measured with the port off.

use super::*;

/// The most pages one section holds; `usize::MAX` for no limit (one section
/// for each request, as before).
pub(super) fn section_pages() -> usize {
    static PAGES: LazyLock<usize> = LazyLock::new(|| {
        let pages = pages_per_section(std::env::var("VICTORY_SECTION_MB").ok().as_deref());
        if pages != usize::MAX {
            tracing::info!(
                mb = pages * PAGE / MIB,
                pages,
                "physical pages: a request is made of sections of at most this many MiB (VICTORY_SECTION_MB)"
            );
        }
        pages
    });
    *PAGES
}

/// [`section_pages`] for a value of `VICTORY_SECTION_MB`: whole MiB.
pub(super) fn pages_per_section(mb: Option<&str>) -> usize {
    match mb
        .and_then(|mb| mb.trim().parse::<usize>().ok())
        .map_or(0, |mb| mb.saturating_mul(MIB / PAGE))
    {
        0 => usize::MAX,
        pages => pages,
    }
}

/// Makes `n` physical pages (`n > 0`) in pagefile-backed sections of at most
/// `per` pages each, enters them in the page table and numbers them
/// consecutively, as one request's; the first page's number. The host's
/// error, with nothing made (the sections made so far close), if it refuses
/// a section.
pub(super) fn allocate_physical(n: usize, per: usize) -> std::io::Result<usize> {
    if n.checked_mul(PAGE).is_none_or(|size| size == 0) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let per = per.max(1);
    let mut sections: Vec<Arc<Section>> = Vec::with_capacity(n.div_ceil(per));
    let mut made = 0;
    while made < n {
        let pages = per.min(n - made);
        let size = pages * PAGE;
        // A pagefile-backed section of `size` bytes, its pages committed when
        // first mapped or written (`commit`): the title asks for 6.1 GiB at
        // once, which a busy host often cannot commit (error 1450).
        // SAFETY: plain section creation.
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                // Under Wine a committed section is a sparse file, free until
                // touched, and committing a reserved one page by page costs a
                // wineserver request each that scans every range committed so
                // far: commit it whole there.
                PAGE_READWRITE | if on_wine() { SEC_COMMIT } else { SEC_RESERVE },
                (size as u64 >> 32) as u32,
                size as u32,
                std::ptr::null(),
            )
        };
        if handle.is_null() {
            // Read before the sections made so far close (`sections` drops).
            return Err(std::io::Error::last_os_error());
        }
        sections.push(Arc::new(Section {
            handle,
            pages,
            window: std::sync::OnceLock::new(),
            committed: (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
        }));
        made += pages;
    }
    for section in &sections {
        // For the GPU side's in-place census: only a committed section can be
        // opened as a host heap.
        let bytes = (section.pages * PAGE) as u64;
        victory_gpu::title_memory_section(section.handle as usize, bytes, on_wine());
    }
    let mut table = page_table_mut();
    let first = table.next_page;
    table.next_page += n * FRAMES;
    let mut number = first;
    for section in &sections {
        for index in 0..section.pages {
            let slice = Slice {
                section: section.clone(),
                index,
            };
            table.pages.insert(number, slice);
            number += FRAMES;
        }
    }
    // Title memory is touched without our locks: a fault there may wait for
    // a mapping call, which takes them.
    drop(table);
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` pages of one new request, in sections of at most `per` pages.
    fn allocate_in(n: usize, per: usize) -> Vec<usize> {
        let first = allocate_physical(n, per).expect("allocated");
        (0..n).map(|k| first + k * FRAMES).collect()
    }

    fn slices(pages: &[usize]) -> Vec<Slice> {
        let table = page_table();
        pages.iter().map(|p| table.pages[p].clone()).collect()
    }

    /// The bytes of one page, through a view of our own.
    fn page_bytes(page: usize) -> Vec<u8> {
        let slice = page_table().pages.get(&page).cloned().expect("known page");
        View::map(&slice, 1).expect("mapped").bytes().to_vec()
    }

    fn state(address: *const c_void) -> u32 {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        assert_eq!(XMemVirtualQuery(process, address, &mut info), 1);
        info.host.State
    }

    fn reserve(n: usize) -> *mut c_void {
        let kind = MEM_RESERVE | PAGES_64K;
        let at = XMemVirtualAlloc(std::ptr::null_mut(), n * PAGE, kind, 0, PAGE_READWRITE);
        assert!(!at.is_null());
        at
    }

    /// The title's views in `[p, p + n pages)`, in pages each.
    fn views_in(p: *mut c_void, n: usize) -> Vec<usize> {
        let start = p as usize;
        physical().views.range(start..start + n * PAGE).map(|(_, (len, _))| len / PAGE).collect()
    }

    #[test]
    fn section_sizes_come_from_the_switch() {
        for off in [None, Some(""), Some("0"), Some("two"), Some("-2")] {
            assert_eq!(pages_per_section(off), usize::MAX, "{off:?}");
        }
        assert_eq!(pages_per_section(Some("1")), 16);
        assert_eq!(pages_per_section(Some(" 2 ")), 32);
        assert!(allocate_physical(0, 32).is_err());
    }

    /// (KainanP ee8b92e14) 33 pages: one section, or 2 MiB and one page;
    /// numbered as one request either way; written and mapped over the bound.
    #[test]
    fn sections_hold_at_most_the_pages_asked() {
        let runs_of = |pages: &[usize]| {
            runs(pages, 0, pages.len() * PAGE).map(|r| r.iter().map(|&(_, n)| n).collect::<Vec<_>>())
        };
        let whole = allocate_in(33, pages_per_section(None));
        let split = allocate_in(33, pages_per_section(Some("2")));
        assert_eq!(runs_of(&whole), Some(vec![33]));
        assert_eq!(runs_of(&split), Some(vec![32, 1]));
        for pages in [&whole, &split] {
            assert!(pages.windows(2).all(|p| p[1] == p[0] + FRAMES));
        }
        assert_eq!(XMemFreePhysicalPages(33, whole.as_ptr()), 1);

        // The last byte of the first section and the first of the second: no
        // window in place, but written all the same.
        let offset = 32 * PAGE - 1;
        assert!(page_window(&split, offset, 2).is_none());
        assert!(page_window(&split, offset, 1).is_some());
        assert!(write_physical(&split, offset, &[17, 29]));
        assert_eq!(page_bytes(split[31])[PAGE - 1], 17);
        assert_eq!(page_bytes(split[32])[0], 29);

        // Mapped in one call, the pages show as one range (two views).
        let at = reserve(33);
        assert_eq!(XMemMapPhysicalPages(at, 33, split.as_ptr()), at);
        assert_eq!(views_in(at, 33), [32, 1]);
        // SAFETY: the 33 pages are mapped read-write.
        unsafe {
            assert_eq!(at.cast::<u8>().byte_add(offset).read(), 17);
            assert_eq!(at.cast::<u8>().byte_add(offset + 1).read(), 29);
            at.cast::<u8>().byte_add(33 * PAGE - 1).write(41);
        }
        assert_eq!(page_bytes(split[32])[PAGE - 1], 41);
        assert_eq!(XMemMapPhysicalPages(at, 33, std::ptr::null()), at);
        assert_eq!(state(at), MEM_RESERVE);
        assert_eq!(XMemFreePhysicalPages(33, split.as_ptr()), 1);
    }

    /// (vilocity 0f44a1706) 70 pages in 2 MiB: sections of 32, 32 and 6, each
    /// page its section's own slice; a DirectStorage-sized write over a
    /// section's end lands whole. When the pages go, every section closes.
    #[test]
    fn a_request_is_made_of_sections_of_two_mebibytes() {
        let pages = allocate_in(70, pages_per_section(Some("2")));
        let held = slices(&pages);
        let sized = |i: usize| if i < 64 { 32 } else { 6 };
        assert!(held.iter().enumerate().all(|(i, s)| s.index == i % 32 && s.section.pages == sized(i)));
        assert!(Arc::ptr_eq(&held[0].section, &held[31].section));
        assert!(!Arc::ptr_eq(&held[31].section, &held[32].section));
        let data: Vec<u8> = (0..3 * PAGE).map(|i| (i % 251) as u8).collect();
        assert!(page_window(&pages, 30 * PAGE + 7, 3 * PAGE).is_none());
        assert!(write_physical(&pages, 30 * PAGE + 7, &data));
        let read: Vec<u8> = pages[30..34].iter().flat_map(|&p| page_bytes(p)).collect();
        assert_eq!(&read[7..7 + 3 * PAGE], &data[..]);
        let gone: Vec<std::sync::Weak<Section>> =
            [0, 32, 64].iter().map(|&i| Arc::downgrade(&held[i].section)).collect();
        drop(held);
        assert_eq!(XMemFreePhysicalPages(70, pages.as_ptr()), 1);
        assert!(gone.iter().all(|s| s.upgrade().is_none()), "every section closed");
    }

    /// (Xeltax a8c223149) Five pages in sections of two: views end at a
    /// section's end; bytes written over the bounds through the title's views
    /// land in the right pages; an unmapping inside the second section and
    /// the GPU's page-mapping copies show their own section's pages.
    #[test]
    fn an_allocation_in_several_sections_is_mapped_and_written_across_their_bounds() {
        let pages = allocate_in(5, 2);
        let groups = runs(&pages, 0, 5 * PAGE).expect("known pages");
        assert_eq!(groups.iter().map(|&(_, n)| n).collect::<Vec<_>>(), [2, 2, 1]);
        let from_start = groups.iter().all(|(slice, _)| slice.index == 0);
        assert!(from_start, "each run from its section's start");
        assert!(page_window(&pages, 2 * PAGE + 7, PAGE).is_some());
        assert!(page_window(&pages, PAGE, 2 * PAGE).is_none());
        let data: Vec<u8> = (0..3 * PAGE + 100).map(|i| (i % 251) as u8 ^ 0x5a).collect();
        let offset = PAGE / 2;
        assert!(write_physical(&pages, offset, &data));
        let seen: Vec<u8> = pages.iter().flat_map(|&p| page_bytes(p)).collect();
        assert!(seen[..offset].iter().all(|&b| b == 0), "before the offset");
        assert_eq!(seen[offset..offset + data.len()], data[..]);
        assert!(seen[offset + data.len()..].iter().all(|&b| b == 0), "after");

        let (a, b) = (reserve(5), reserve(2));
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 5, pages.as_ptr()), a);
        assert_eq!(views_in(a, 5), [2, 2, 1]);
        // SAFETY: all five pages are mapped read-write at `a`.
        unsafe {
            assert_eq!(std::slice::from_raw_parts(a.cast::<u8>(), 5 * PAGE), &seen[..]);
            at(a, 2).cast::<u8>().sub(1).write_bytes(0xee, 2);
            at(a, 4).cast::<u8>().sub(1).write_bytes(0xdd, 2);
        }
        assert_eq!(page_bytes(pages[1])[PAGE - 1], 0xee);
        assert_eq!(page_bytes(pages[2])[0], 0xee);
        assert_eq!(page_bytes(pages[3])[PAGE - 1], 0xdd);
        assert_eq!(page_bytes(pages[4])[0], 0xdd);
        // Where the pages show, over a bound: one run of addresses.
        assert_eq!(shown_at(&pages, PAGE, 3 * PAGE), vec![(at(a, 1) as usize, 3 * PAGE)]);
        // The second section's second page unmapped: its first stays.
        assert_eq!(XMemMapPhysicalPages(at(a, 3), 1, std::ptr::null()), at(a, 3));
        assert_eq!(views_in(a, 5), [2, 1, 1]);
        assert_eq!(state(at(a, 3)), MEM_RESERVE);
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 0xee);
        assert!(map_alias(b as u64, at(a, 4) as u64, PAGE as u64));
        assert!(map_alias(at(b, 1) as u64, at(a, 2) as u64, PAGE as u64));
        // SAFETY: both mapped read-write.
        unsafe {
            assert_eq!(b.cast::<u8>().read(), 0xdd);
            assert_eq!(at(b, 1).cast::<u8>().read(), 0xee);
        }
        for (p, n) in [(a, 5), (b, 2)] {
            assert_eq!(XMemMapPhysicalPages(p, n, std::ptr::null()), p);
        }
        assert!(shown_at(&pages, 0, 5 * PAGE).is_empty());
        assert_eq!(XMemFreePhysicalPages(5, pages.as_ptr()), 1);
    }

    /// The title's own call cuts a request as the switch says: unset (the
    /// default), one section of 33 pages, as before the switch.
    #[test]
    fn the_title_call_cuts_by_the_switch() {
        let (mut pages, mut given) = (vec![0; 33], 33);
        assert_eq!(XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()), 1);
        assert_eq!(given, 33);
        let per = section_pages().min(33);
        let expected: Vec<usize> = (0..33).step_by(per).map(|first| per.min(33 - first)).collect();
        let groups = runs(&pages, 0, 33 * PAGE).expect("known pages");
        assert_eq!(groups.iter().map(|&(_, n)| n).collect::<Vec<_>>(), expected);
        if std::env::var_os("VICTORY_SECTION_MB").is_none() {
            assert_eq!(expected, [33], "off by default");
        }
        assert_eq!(XMemFreePhysicalPages(33, pages.as_ptr()), 1);
    }

    /// (KainanP ee8b92e14) Remapping single pages of a large request while
    /// other memory is made and freed: one section per request against 2 MiB
    /// sections, no title needed. Ignored: it maps 4 GiB and runs for a
    /// minute. `cargo test --release -p victory-hle --lib physical_remap_bench
    /// -- --ignored --nocapture --test-threads=1`; `VICTORY_SECTION_BENCH=<MiB>,
    /// <seconds>,<rounds>,<pressure MiB>` changes the defaults `4096,10,3,2048`.
    #[test]
    #[ignore = "benchmark"]
    fn physical_remap_bench() {
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};

        let knobs: Vec<usize> = std::env::var("VICTORY_SECTION_BENCH")
            .unwrap_or_default()
            .split(',')
            .filter_map(|v| v.trim().parse().ok())
            .collect();
        let knob = |i: usize, default: usize| knobs.get(i).copied().unwrap_or(default);
        let (mib, seconds, rounds, pressure) = (knob(0, 4096), knob(1, 10), knob(2, 3), knob(3, 2048));
        let n = mib * MIB / PAGE;
        let percentile = |v: &mut Vec<f64>, p: usize| {
            v.sort_by(f64::total_cmp);
            v[(v.len() * p / 100).min(v.len() - 1)]
        };
        let name = |mb: &str| if mb.is_empty() { "one".to_string() } else { format!("{mb} MiB") };
        let mut results: Vec<(&str, Vec<[f64; 5]>)> = vec![("", vec![]), ("2", vec![])];
        for round in 0..rounds {
            for (mb, rows) in &mut results {
                let pages = allocate_in(n, pages_per_section(Some(mb)));
                let at = reserve(n);
                let base = at as usize;
                // A view a page, each page touched, as the title maps its pool.
                for (k, page) in pages.iter().enumerate() {
                    let page_at = (base + k * PAGE) as *mut c_void;
                    assert_eq!(XMemMapPhysicalPages(page_at, 1, page), page_at);
                    for b in (0..PAGE).step_by(4096) {
                        // SAFETY: mapped read-write just now.
                        unsafe { page_at.cast::<u8>().byte_add(b).write(1) };
                    }
                }
                let stop = AtomicBool::new(false);
                let mut calls = Vec::new();
                std::thread::scope(|scope| {
                    // Another user of memory: made, touched and freed again.
                    scope.spawn(|| {
                        while pressure > 0 && !stop.load(Ordering::Relaxed) {
                            let mut block = vec![0u8; pressure * MIB];
                            for b in block.iter_mut().step_by(4096) {
                                *b = 1;
                            }
                            std::hint::black_box(&block);
                        }
                    });
                    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
                    let mut next = || {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        seed as usize % n
                    };
                    let start = Instant::now();
                    while start.elapsed() < Duration::from_secs(seconds as u64) {
                        let (k, page) = (next(), pages[next()]);
                        let page_at = (base + k * PAGE) as *mut c_void;
                        let t = Instant::now();
                        assert_eq!(XMemMapPhysicalPages(page_at, 1, &page), page_at);
                        calls.push(t.elapsed().as_secs_f64() * 1e6);
                        // SAFETY: mapped read-write just now.
                        unsafe { page_at.cast::<u8>().write(2) };
                    }
                    stop.store(true, Ordering::Relaxed);
                });
                let slow: Vec<f64> = calls.iter().copied().filter(|&us| us >= 1000.0).collect();
                let row = [
                    calls.len() as f64 / seconds as f64,
                    percentile(&mut calls, 50),
                    percentile(&mut calls, 99),
                    slow.len() as f64,
                    slow.iter().sum::<f64>() / 1000.0,
                ];
                println!(
                    "round {round}, sections {}: {:.0} remaps/s, median {:.1} us, p99 {:.1} us, {} of 1 ms or more ({:.0} ms)",
                    name(mb),
                    row[0],
                    row[1],
                    row[2],
                    row[3],
                    row[4]
                );
                rows.push(row);
                assert_eq!(XMemMapPhysicalPages(at, n, std::ptr::null()), at);
                assert_eq!(XMemFreePhysicalPages(n, pages.as_ptr()), 1);
            }
        }
        for (mb, rows) in &results {
            let median = |i: usize| {
                let mut v: Vec<f64> = rows.iter().map(|r| r[i]).collect();
                percentile(&mut v, 50)
            };
            println!(
                "median of {rounds} rounds, sections {}: {:.0} remaps/s, median {:.1} us, p99 {:.1} us, {} of 1 ms or more ({:.0} ms)",
                name(mb),
                median(0),
                median(1),
                median(2),
                median(3),
                median(4)
            );
        }
    }
}
