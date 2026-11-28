/// A list of ours is found by its name, with the debug layer's note or not.
#[cfg(feature = "diagnostics")]
#[test]
fn a_list_is_found_by_the_number_in_its_name() {
    assert_eq!(super::list_number("victory host list 7"), Some(7));
    assert_eq!(super::list_number("victory host list 158 (debug layer indirect)"), Some(158));
    assert_eq!(super::list_number("victory host list 7x"), None);
    assert_eq!(super::list_number("victory host list 7 x"), None);
    assert_eq!(super::list_number("another list 7"), None);
    assert_eq!(super::list_number(""), None);
}

/// What is there of a range some of whose pages are not: those bytes,
/// counted, and zeros for the rest, whatever the buffer held.
#[test]
fn what_is_there_is_read_and_the_rest_is_zeros() {
    use windows_sys::Win32::System::Memory::*;
    const SIZE: usize = 0x8_0000;
    // SAFETY: a reservation of ours; only committed pages are written.
    unsafe {
        let at = VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
        assert!(!at.is_null());
        // 0-0x1_1000 and 0x3_0000-0x4_0000 are there.
        for (from, len) in [(0, 0x1_1000), (0x3_0000, 0x1_0000)] {
            assert!(!VirtualAlloc(at.add(from).cast(), len, MEM_COMMIT, PAGE_READWRITE).is_null());
            at.add(from).write_bytes(0x5a, len);
        }
        // From an address inside a page, as textures are packed.
        let mut out = vec![0x77u8; SIZE - 0x300];
        assert_eq!(super::read_there(at.add(0x300), &mut out), 0x1_1000 - 0x300 + 0x1_0000);
        for (i, &b) in out.iter().enumerate() {
            let from = i + 0x300;
            let there = from < 0x1_1000 || (0x3_0000..0x4_0000).contains(&from);
            assert_eq!(b, if there { 0x5a } else { 0 }, "byte {from:#x}");
        }
        // All there: one read.
        let mut out = vec![0x77u8; 0x1_0000];
        assert_eq!(super::read_there(at.add(0x1000), &mut out), 0x1_0000);
        assert!(out.iter().all(|&b| b == 0x5a));
        // Nothing there.
        let mut out = vec![0x77u8; 0x1_0000];
        assert_eq!(super::read_there(at.add(0x5_0000), &mut out), 0);
        assert!(out.iter().all(|&b| b == 0));
        assert_ne!(VirtualFree(at.cast(), 0, MEM_RELEASE), 0);
    }
}

/// Zeros for a title destination that went meanwhile are no fault.
#[test]
fn zeros_where_nothing_is_are_lost() {
    use windows_sys::Win32::System::Memory::*;
    // SAFETY: a reservation of ours, its first page committed.
    unsafe {
        let at = VirtualAlloc(std::ptr::null(), 1 << 16, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
        assert!(!VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE).is_null());
        at.write_bytes(7, 4096);
        super::zero(at, 4096);
        assert!(std::slice::from_raw_parts(at, 4096).iter().all(|&b| b == 0));
        super::zero(at.add(4096), 4096);
        VirtualFree(at.cast(), 0, MEM_RELEASE);
    }
}

#[test]
fn sparse_copies_preserve_islands_and_observe_remaps() {
    use windows_sys::Win32::System::Memory::*;
    const N: usize = 5 << 16;
    // SAFETY: we reserve a private region, commit selected pages inside it,
    // and only directly access the pages while they are writable.
    unsafe {
        let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
        assert!(!base.is_null());
        let mut expected = vec![0; N];
        for (offset, value) in [(0, 9), (2 << 16, 37), (N - 4096, 85)] {
            let at = base.add(offset);
            assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
            at.write_bytes(value, 4096);
            expected[offset..offset + 4096].fill(value);
        }
        for remap in [false, true] {
            if remap {
                let mut old = 0;
                assert_ne!(VirtualProtect(base.cast(), 4096, PAGE_NOACCESS, &mut old), 0);
                expected[..4096].fill(0);
                let at = base.add(1 << 16);
                assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
                at.write_bytes(0xc7, 4096);
                expected[1 << 16..(1 << 16) + 4096].fill(0xc7);
            }
            let mut out = vec![0xee; N];
            super::copy_mapped(out.as_mut_ptr().add(27), base.add(27), N - 54, false);
            assert_eq!(out[27..N - 27], expected[27..N - 27]);
            assert!(out[..27].iter().chain(&out[N - 27..]).all(|&v| v == 0xee));
        }
        assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
    }
}

use super::*;

#[test]
fn warp_adapter_accepts_only_software() {
    // This probes DXGI without creating a D3D12 device or running a title.
    // SAFETY: plain DXGI adapter queries.
    let raw = unsafe {
        let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
        else {
            return;
        };
        factory.EnumWarpAdapter::<IDXGIAdapter>()
    };
    let Ok(raw) = raw else { return };
    // SAFETY: EnumWarpAdapter returns a DXGI adapter, which implements
    // IDXGIAdapter1; `raw` owns that interface.
    let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
    eprintln!(
        "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
        desc.VendorId, desc.Flags
    );
    assert_eq!(
        warp_adapter().is_ok(),
        desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
    );
}
