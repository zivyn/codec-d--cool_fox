//! Query heaps. A heap remembers its description; with
//! `VICTORY_REAL_OCCLUSION=1` an occlusion heap also has a host heap behind
//! it ([`crate::occlusion`]). Every other kind produces no result of its own.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, QUERY_HEAP, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

const HEAP_IIDS: [Iid; 4] = [
    guid(
        0x0d9658ae,
        0xed45,
        0x469e,
        [0xa6, 0x1d, 0x97, 0x0e, 0xc5, 0x83, 0xca, 0xb4],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Desc {
    kind: u32,
    count: u32,
    nodes: u32,
}

#[repr(C)]
struct Heap {
    object: Object,
    desc: Desc,
    /// The host heap of an occlusion heap, with `VICTORY_REAL_OCCLUSION=1`.
    #[cfg(windows)]
    host: Option<windows::Win32::Graphics::Direct3D12::ID3D12QueryHeap>,
}

/// The host heap behind a title heap and how many queries it holds, if the
/// title's `heap` is one of ours of the occlusion kind and the switch is on.
#[cfg(windows)]
pub(crate) fn host_heap_of(
    heap: This,
) -> Option<(windows::Win32::Graphics::Direct3D12::ID3D12QueryHeap, u32)> {
    if heap.is_null() {
        return None;
    }
    // SAFETY: the title passes objects of ours; the first word of any of them
    // is its table, which tells ours apart before the rest is read.
    unsafe {
        if heap.cast::<*const *const ()>().read() != FUNCTIONS.0.as_ptr() {
            return None;
        }
        let heap = &*heap.cast::<Heap>();
        heap.host.clone().map(|host| (host, heap.desc.count))
    }
}

/// The host heap of a heap description: occlusion only, and only when asked.
#[cfg(windows)]
fn host_heap(desc: &Desc) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12QueryHeap> {
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_QUERY_HEAP_DESC, D3D12_QUERY_HEAP_TYPE_OCCLUSION,
    };
    const OCCLUSION: u32 = 0;
    if !crate::occlusion::real() || desc.kind != OCCLUSION || desc.count == 0 {
        return None;
    }
    let host = crate::host::host()?;
    let mut heap = None;
    // SAFETY: a plain description, a live device.
    let made = unsafe {
        host.device.CreateQueryHeap(
            &D3D12_QUERY_HEAP_DESC {
                Type: D3D12_QUERY_HEAP_TYPE_OCCLUSION,
                Count: desc.count,
                NodeMask: 0,
            },
            &mut heap,
        )
    };
    if let Err(e) = made {
        tracing::warn!(error = %e, count = desc.count, "no host occlusion query heap");
        return None;
    }
    heap
}

/// Device method.
pub(crate) extern "system" fn create(
    _: This,
    desc: *const Desc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "query heap");
    let heap = Box::new(Heap {
        object: Object::new(&FUNCTIONS),
        desc,
        #[cfg(windows)]
        host: host_heap(&desc),
    });
    #[cfg(windows)]
    if crate::occlusion::real() {
        tracing::info!(
            kind = desc.kind,
            count = desc.count,
            host = heap.host.is_some(),
            "the title makes a query heap"
        );
    }
    object::hand_out(QUERY_HEAP, heap, iid, &HEAP_IIDS, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        QUERY_HEAP,
        iid,
        object::is(iid, &HEAP_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Heap>(this)
}

/// IUnknown 3, object 4, device child 1.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<QUERY_HEAP, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOINTERFACE, E_POINTER, S_FALSE, S_OK, slot};

    #[test]
    fn null_descriptions_are_rejected() {
        let mut heap = std::ptr::null();
        assert_eq!(
            create(std::ptr::null(), std::ptr::null(), &HEAP_IIDS[0], &mut heap),
            E_INVALIDARG
        );
        assert!(heap.is_null());
        // The description is checked first: a probe with no room for the
        // heap fails on it rather than answering S_FALSE.
        assert_eq!(
            create(
                std::ptr::null(),
                std::ptr::null(),
                &HEAP_IIDS[0],
                std::ptr::null_mut()
            ),
            E_INVALIDARG
        );
    }

    #[test]
    fn creation_without_an_out_pointer_only_probes() {
        let desc = Desc {
            kind: 1, // timestamps
            count: 64,
            nodes: 0,
        };
        // A known interface with no out pointer asks whether creation would
        // succeed; the heap is dropped instead of handed out.
        assert_eq!(
            create(std::ptr::null(), &desc, &HEAP_IIDS[0], std::ptr::null_mut()),
            S_FALSE
        );
        // A null interface id is tolerated: no interface, no heap.
        let mut heap = std::ptr::null();
        assert_eq!(
            create(std::ptr::null(), &desc, std::ptr::null(), &mut heap),
            E_NOINTERFACE
        );
        assert!(heap.is_null());
    }

    /// Only the title's own heaps have a host heap to give, and only with
    /// `VICTORY_REAL_OCCLUSION=1` (the test process does not set it).
    #[cfg(windows)]
    #[test]
    fn a_heap_has_no_host_heap_unless_asked_for() {
        assert!(host_heap_of(std::ptr::null()).is_none());
        let desc = Desc {
            kind: 0,
            count: 8,
            nodes: 0,
        };
        let mut heap = std::ptr::null();
        assert_eq!(
            create(std::ptr::null(), &desc, &HEAP_IIDS[0], &mut heap),
            S_OK
        );
        if !crate::occlusion::real() {
            assert!(host_heap_of(heap).is_none());
        }
        // Something that is not a heap of ours is not read as one.
        let not_ours = [0usize; 8];
        assert!(host_heap_of(not_ours.as_ptr().cast()).is_none());
        assert_eq!(slot::<extern "system" fn(This) -> u32>(heap, 0x10)(heap), 0);
    }

    #[test]
    fn heaps_answer_their_interfaces_and_count_references() {
        let desc = Desc {
            kind: 0, // occlusion
            count: 8,
            nodes: 0,
        };
        let null = std::ptr::null();
        let mut heap = null;
        assert_eq!(create(null, &desc, &HEAP_IIDS[0], &mut heap), S_OK);
        assert!(!heap.is_null());
        let query = slot::<extern "system" fn(This, *const Iid, *mut This) -> HRESULT>(heap, 0x0);
        // Every interface the heap answers to hands out the same object.
        for iid in &HEAP_IIDS {
            let mut out = null;
            assert_eq!(query(heap, iid, &mut out), S_OK);
            assert_eq!(out as usize, heap as usize);
        }
        // An unknown interface writes null behind and reports it missing.
        let unknown = [0u8; 16];
        let mut out = heap;
        assert_eq!(query(heap, &unknown, &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        // A null interface id is tolerated rather than matched.
        let mut out = heap;
        assert_eq!(query(heap, std::ptr::null(), &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        // No room for the answer is a pointer error.
        assert_eq!(query(heap, &unknown, std::ptr::null_mut()), E_POINTER);
        // References: one from creation, four from the queries, one added;
        // each release hands back what is left, the last frees the heap.
        assert_eq!(slot::<extern "system" fn(This) -> u32>(heap, 0x8)(heap), 6);
        let release = slot::<extern "system" fn(This) -> u32>(heap, 0x10);
        for left in [5, 4, 3, 2, 1, 0] {
            assert_eq!(release(heap), left);
        }
    }
}
