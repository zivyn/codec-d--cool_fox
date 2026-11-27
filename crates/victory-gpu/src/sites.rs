//! The host's buffers of the PC's memory alive now, by the line of code that
//! made them (the memory line, diagnostics).
//!
//! Upload buffers (write-combined) and shared buffers (write-back) are the
//! PC's memory the card reaches. The memory map says how much write-combined
//! memory the process holds (11.5 GB in run U1b at the cell), the counters
//! by kind say what was made, and nothing said which of it is alive and who
//! holds it. Each buffer made by `Host::upload_buffer` and
//! `Host::shared_buffer` is counted at the line that asked for it, and
//! counted out when the last reference to it goes (an object of ours that
//! the buffer holds says so, as [`crate::pool`] does for textures).
//! What the memory map holds beyond the sum of these is not ours: the
//! driver's command memory and the like.

#![cfg_attr(not(feature = "diagnostics"), allow(dead_code, unused_imports))]

use std::collections::HashMap;
use std::ffi::c_void;
use std::panic::Location;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

/// An upload buffer (write-combined).
pub(crate) const UPLOAD: u8 = 0;
/// A shared buffer (write-back).
pub(crate) const SHARED: u8 = 1;

/// What one line of code made.
#[derive(Clone, Copy)]
pub(crate) struct Site {
    pub(crate) file: &'static str,
    pub(crate) line: u32,
    pub(crate) kind: u8,
    pub(crate) alive: u64,
    pub(crate) alive_bytes: u64,
    pub(crate) made: u64,
    pub(crate) made_bytes: u64,
}

#[derive(Default)]
struct Book {
    index: HashMap<(&'static str, u32, u8), usize>,
    sites: Vec<Site>,
}

static BOOK: OnceLock<Mutex<Book>> = OnceLock::new();

fn book() -> std::sync::MutexGuard<'static, Book> {
    BOOK.get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The buffer `resource` of `size` bytes was made for `kind` at `at`.
#[cfg(feature = "diagnostics")]
pub(crate) fn made(resource: &ID3D12Resource, kind: u8, size: u64, at: &'static Location<'static>) {
    let slot = {
        let mut book = book();
        let book = &mut *book;
        let slot = *book
            .index
            .entry((at.file(), at.line(), kind))
            .or_insert_with(|| {
                book.sites.push(Site {
                    file: at.file(),
                    line: at.line(),
                    kind,
                    alive: 0,
                    alive_bytes: 0,
                    made: 0,
                    made_bytes: 0,
                });
                book.sites.len() - 1
            });
        let site = &mut book.sites[slot];
        site.alive += 1;
        site.alive_bytes += size;
        site.made += 1;
        site.made_bytes += size;
        slot
    };
    watch(resource, slot, size);
}

/// Counted in when the build has no memory line: nothing.
#[cfg(not(feature = "diagnostics"))]
pub(crate) fn made(
    _resource: &ID3D12Resource,
    _kind: u8,
    _size: u64,
    _at: &'static Location<'static>,
) {
}

/// Every site with something alive, the biggest first.
#[cfg(feature = "diagnostics")]
pub(crate) fn alive() -> Vec<Site> {
    let mut sites: Vec<Site> = book()
        .sites
        .iter()
        .copied()
        .filter(|s| s.alive > 0)
        .collect();
    sites.sort_by_key(|s| std::cmp::Reverse(s.alive_bytes));
    sites
}

/// What the book says is alive: the buffers and their bytes, by kind, and
/// how many were made and their bytes.
#[cfg(feature = "diagnostics")]
pub(crate) fn totals() -> [[u64; 4]; 2] {
    let mut out = [[0u64; 4]; 2];
    for s in &book().sites {
        let t = &mut out[usize::from(s.kind.min(1))];
        t[0] += s.alive;
        t[1] += s.alive_bytes;
        t[2] += s.made;
        t[3] += s.made_bytes;
    }
    out
}

/// What the buffer's last release gives back.
#[repr(C)]
struct Notice {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    slot: usize,
    size: u64,
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}
static NOTICE: Methods = Methods([
    query as *const (),
    add_ref as *const (),
    release as *const (),
]);

unsafe extern "system" fn query(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: COM's arguments.
    unsafe {
        if out.is_null() {
            return HRESULT(0x8000_4003_u32 as i32);
        }
        if iid.as_ref().is_some_and(|iid| *iid == IUnknown::IID) {
            add_ref(this);
            *out = this;
            return HRESULT(0);
        }
        *out = std::ptr::null_mut();
    }
    HRESULT(0x8000_4002_u32 as i32)
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: one of ours, alive while a reference to it is.
    unsafe {
        (*this.cast::<Notice>())
            .references
            .fetch_add(1, Ordering::Relaxed)
            + 1
    }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Notice>())
            .references
            .fetch_sub(1, Ordering::AcqRel)
            - 1;
        if left == 0 {
            let gone = Box::from_raw(this.cast::<Notice>());
            let mut book = book();
            if let Some(site) = book.sites.get_mut(gone.slot) {
                site.alive = site.alive.saturating_sub(1);
                site.alive_bytes = site.alive_bytes.saturating_sub(gone.size);
            }
        }
        left
    }
}

/// Has the book told when `resource` goes.
#[cfg(feature = "diagnostics")]
fn watch(resource: &ID3D12Resource, slot: usize, size: u64) {
    use windows::Win32::Graphics::Direct3D12::ID3D12Object;
    const KEY: GUID = GUID::from_u128(0x7a31c5e2_04d9_4b86_a1f3_52c8d90e6b47);
    let Ok(object) = resource.cast::<ID3D12Object>() else {
        return;
    };
    let notice = Box::into_raw(Box::new(Notice {
        methods: &NOTICE.0,
        references: AtomicU32::new(1),
        slot,
        size,
    }));
    // SAFETY: a COM object of ours with one reference, which this drops; the
    // buffer takes its own. (One that is refused is never counted out.)
    unsafe {
        let notice = IUnknown::from_raw(notice.cast());
        if object.SetPrivateDataInterface(&KEY, &notice).is_err() {
            std::mem::forget(notice);
        }
    }
}

#[cfg(all(test, feature = "diagnostics"))]
mod tests {
    use super::*;

    #[test]
    fn a_site_counts_in_and_out() {
        let at = Location::caller();
        // A site of this test alone, so that others' buffers do not count.
        let slot = {
            let mut book = book();
            book.sites.push(Site {
                file: at.file(),
                line: u32::MAX,
                kind: SHARED,
                alive: 0,
                alive_bytes: 0,
                made: 0,
                made_bytes: 0,
            });
            book.sites.len() - 1
        };
        let notice = Box::into_raw(Box::new(Notice {
            methods: &NOTICE.0,
            references: AtomicU32::new(1),
            slot,
            size: 4096,
        }));
        {
            let mut book = book();
            book.sites[slot].alive = 1;
            book.sites[slot].alive_bytes = 4096;
        }
        // SAFETY: a notice as `watch` makes one; the last release frees it.
        unsafe {
            let object = IUnknown::from_raw(notice.cast());
            let again = object.clone();
            drop(object);
            assert_eq!(book().sites[slot].alive, 1);
            drop(again);
        }
        let book = book();
        assert_eq!(
            (book.sites[slot].alive, book.sites[slot].alive_bytes),
            (0, 0)
        );
    }
}
