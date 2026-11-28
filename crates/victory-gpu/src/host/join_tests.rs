use super::*;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
};

const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// A part whose fill reads upload memory inside a range that the part in front declares writable is refused;
/// with `VICTORY_JOIN_WIDE_MB` it is joined when the range is that wide, and only then; every other kind of
/// read, and a copy of title memory, keep it out whatever the width.
#[test]
fn a_fill_inside_a_wide_declaration_joins_only_when_asked_and_nothing_else_does() {
    let Some(host) = host() else {
        return;
    };
    const LEN: usize = 16 << 20;
    // SAFETY: memory of our own, never freed.
    let start = unsafe {
        VirtualAlloc(
            std::ptr::null(),
            LEN,
            MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
            PAGE_READWRITE,
        )
    } as usize;
    assert_ne!(start, 0);
    crate::shadow::register(0x6a11, start as u64, (LEN / 2) as u64, false);
    let copy = crate::shadow::find(start as u64, 64).unwrap();
    // In front: shaders may write the first 8 MiB; or one 64 KiB range of it.
    let wide_list = host.new_list().unwrap();
    copy.gpu_writes(&wide_list, start as u64, (LEN / 2) as u64);
    wide_list.close();
    let narrow_list = host.new_list().unwrap();
    copy.gpu_writes(&narrow_list, (start + (1 << 20) - 4096) as u64, 64 << 10);
    narrow_list.close();
    let nothing = host.new_list().unwrap();
    nothing.close();
    // Behind: a fill of upload memory from 1 MiB into the copy; a copy of title memory.
    let fills = host.new_list().unwrap();
    let (staging, staged) = fills.upload(host, 4096).unwrap();
    fills.defer_fill_buffer(&staging, staged, (start + (1 << 20)) as *const u8, 500);
    fills.close();
    let copies = host.new_list().unwrap();
    copies.defer_copy(
        (start + LEN / 2 + 4096) as *mut u8,
        (start + 4096) as *const u8,
        64,
    );
    copies.close();
    let four = 4 << 20;
    let no = |ahead: &HostList, next: &HostList, wide: usize, classify: bool| {
        join_refusal_with(std::iter::once(ahead), &[next], wide, classify)
    };
    // As before: the wide range keeps the fill out; told apart when asked to.
    assert_eq!(no(&wide_list, &fills, 0, false), Some(NoJoin::Written));
    assert_eq!(no(&wide_list, &fills, 0, true), Some(NoJoin::WideOnly));
    // Asked: it joins.
    assert_eq!(no(&wide_list, &fills, four, false), None);
    assert_eq!(no(&wide_list, &fills, four, true), None);
    // A range under the width still keeps it out, as it is read.
    assert_eq!(no(&narrow_list, &fills, four, false), Some(NoJoin::Written));
    // A range of the width that the fill does not read does not matter, and neither does nothing in front.
    assert_eq!(no(&nothing, &fills, four, false), None);
    assert_eq!(no(&nothing, &fills, 0, false), None);
    // A copy of title memory is never joined, wide or not.
    assert_eq!(
        no(&wide_list, &copies, four, false),
        Some(NoJoin::CopyOrRun)
    );
    assert_eq!(no(&nothing, &copies, four, false), Some(NoJoin::CopyOrRun));
    assert!(!joinable(std::iter::once(&wide_list), &[&copies]));
}
