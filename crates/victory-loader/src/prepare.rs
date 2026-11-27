//! Turns a title's image into something the Windows loader maps but does
//! not run. Pure byte manipulation, so it is testable on any host.

use victory_pe::layout::{self, dir, dll_flags, file_flags};
use victory_pe::{ImageInfo, PeError};

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error(transparent)]
    Pe(#[from] PeError),
    #[error("cannot patch image: {0}")]
    Patch(&'static str),
}

/// A patched copy of an image plus everything we need from the original.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    /// Parsed from the *original* bytes: imports, TLS callbacks and entry
    /// point as the title expects them.
    pub info: ImageInfo,
    /// The patched bytes to hand to the Windows loader.
    pub bytes: Vec<u8>,
    /// BLAKE3 of the original bytes: identifies the exact build.
    pub original_hash: blake3::Hash,
}

impl PreparedImage {
    /// File name for the patched copy, unique per original build.
    pub fn file_name(&self) -> String {
        format!("victory-image-{}.dll", &self.original_hash.to_hex()[..16])
    }
}

/// Produces the patched copy. The changes are:
///
/// * `IMAGE_FILE_DLL` set, entry point cleared: the OS maps it like a DLL
///   and calls nothing;
/// * import directory points to its empty terminator and the bound-import
///   directory is cleared: we bind the original slots using `info.imports`;
/// * TLS callback pointer cleared: the OS still allocates static TLS for
///   the image, but callbacks only run when we call them (after binding);
///   its base relocation is made a no-op so the 0 survives a rebase;
/// * `FORCE_INTEGRITY` cleared, checksum zeroed: the copy is unsigned.
pub fn prepare(original: &[u8]) -> Result<PreparedImage, PrepareError> {
    let info = ImageInfo::parse(original)?;
    let mut bytes = original.to_vec();

    let fh = layout::file_header_offset(&bytes).ok_or(PrepareError::Patch("no file header"))?;
    let opt =
        layout::optional_header_offset(&bytes).ok_or(PrepareError::Patch("no optional header"))?;

    let characteristics = info.characteristics | file_flags::DLL;
    write_u16(
        &mut bytes,
        fh + layout::FILE_HEADER_CHARACTERISTICS,
        characteristics,
    )?;
    write_u32(&mut bytes, opt + layout::OPT_ADDRESS_OF_ENTRY_POINT, 0)?;
    write_u32(&mut bytes, opt + layout::OPT_CHECKSUM, 0)?;
    let dll_characteristics = info.dll_characteristics & !dll_flags::FORCE_INTEGRITY;
    write_u16(
        &mut bytes,
        opt + layout::OPT_DLL_CHARACTERISTICS,
        dll_characteristics,
    )?;

    // Windows 10 assigns static TLS to the prepared DLL when its empty
    // import table is present. The parser has already walked one descriptor
    // per module up to the null terminator, regardless of the recorded Size.
    if let Some(at) = layout::data_directory_offset(&bytes, dir::IMPORT) {
        let rva = layout::read_u32(&bytes, at).ok_or(PrepareError::Patch("no import RVA"))?;
        if rva != 0 {
            let terminator = u32::try_from(info.imports.len())
                .ok()
                .and_then(|count| count.checked_mul(IMPORT_DESCRIPTOR_SIZE))
                .and_then(|offset| rva.checked_add(offset))
                .ok_or(PrepareError::Patch("import terminator RVA overflow"))?;
            write_u32(&mut bytes, at, terminator)?;
            write_u32(&mut bytes, at + 4, IMPORT_DESCRIPTOR_SIZE)?;
        }
    }
    if let Some(at) = layout::data_directory_offset(&bytes, dir::BOUND_IMPORT) {
        write(&mut bytes, at, &[0; layout::DATA_DIRECTORY_SIZE])?;
    }

    if let Some(tls) = &info.tls
        && tls.callbacks_va != 0
    {
        let at = info
            .rva_to_file_offset(tls.directory_rva)
            .ok_or(PrepareError::Patch("TLS directory is not file-backed"))?;
        write_u64(
            &mut bytes,
            at as usize + layout::TLS64_ADDRESS_OF_CALLBACKS,
            0,
        )?;
        // The field's base relocation would turn the 0 into `delta` whenever
        // the loader maps the copy away from its preferred base (Wine always
        // does for DYNAMIC_BASE images), and the loader then reads callbacks
        // from there. Make that entry a no-op.
        // A field past the top of the RVA space has no relocation to clear.
        if let Some(field_rva) = tls
            .directory_rva
            .checked_add(layout::TLS64_ADDRESS_OF_CALLBACKS as u32)
        {
            for (entry, target) in dir64_relocations(&bytes, &info) {
                if target == field_rva {
                    write_u16(&mut bytes, entry, 0)?; // IMAGE_REL_BASED_ABSOLUTE
                }
            }
        }
    }

    Ok(PreparedImage {
        info,
        bytes,
        original_hash: blake3::hash(original),
    })
}

/// Size of an IMAGE_IMPORT_DESCRIPTOR in the PE file.
const IMPORT_DESCRIPTOR_SIZE: u32 = 20;

/// Every `IMAGE_REL_BASED_DIR64` base relocation: (file offset of its entry,
/// RVA it patches). Empty if the image has no readable relocation table.
fn dir64_relocations(bytes: &[u8], info: &ImageInfo) -> Vec<(usize, u32)> {
    const DIR64: u16 = 10;
    let mut out = Vec::new();
    let Some(d) = layout::data_directory_offset(bytes, dir::BASERELOC) else {
        return out;
    };
    let (Some(rva), Some(size)) = (layout::read_u32(bytes, d), layout::read_u32(bytes, d + 4))
    else {
        return out;
    };
    let Some(start) = info.rva_to_file_offset(rva).filter(|_| size != 0) else {
        return out;
    };
    let (mut at, end) = (start as usize, start as usize + size as usize);
    while at + 8 <= end {
        let (Some(page), Some(block)) =
            (layout::read_u32(bytes, at), layout::read_u32(bytes, at + 4))
        else {
            break;
        };
        let block = block as usize;
        if block < 8 || at + block > end {
            break;
        }
        for entry in (at + 8..at + block).step_by(2) {
            let Some(e) = bytes.get(entry..entry + 2) else {
                break;
            };
            let e = u16::from_le_bytes([e[0], e[1]]);
            // A target past the top of the 32-bit RVA space can only come from
            // a malformed page RVA: skip it rather than wrap around.
            if e >> 12 == DIR64
                && let Some(target) = page.checked_add(u32::from(e & 0xFFF))
            {
                out.push((entry, target));
            }
        }
        at += block;
    }
    out
}

fn write_u16(bytes: &mut [u8], at: usize, v: u16) -> Result<(), PrepareError> {
    write(bytes, at, &v.to_le_bytes())
}
fn write_u32(bytes: &mut [u8], at: usize, v: u32) -> Result<(), PrepareError> {
    write(bytes, at, &v.to_le_bytes())
}
fn write_u64(bytes: &mut [u8], at: usize, v: u64) -> Result<(), PrepareError> {
    write(bytes, at, &v.to_le_bytes())
}
fn write(bytes: &mut [u8], at: usize, v: &[u8]) -> Result<(), PrepareError> {
    bytes
        .get_mut(at..at + v.len())
        .ok_or(PrepareError::Patch("header field outside of file"))?
        .copy_from_slice(v);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_own_test_binary() {
        let original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let prepared = prepare(&original).unwrap();
        assert!(!prepared.info.is_dll(), "info describes the original");
        assert!(prepared.info.import_count() > 0);

        let patched = ImageInfo::parse(&prepared.bytes).unwrap();
        assert!(patched.is_dll());
        assert_eq!(patched.entry_rva, 0);
        assert!(patched.imports.is_empty());
        assert_eq!(patched.dll_characteristics & dll_flags::FORCE_INTEGRITY, 0);
        if let Some(tls) = &patched.tls {
            assert_eq!(tls.callbacks_va, 0);
            let field = tls.directory_rva + layout::TLS64_ADDRESS_OF_CALLBACKS as u32;
            assert!(
                dir64_relocations(&prepared.bytes, &patched)
                    .iter()
                    .all(|&(_, target)| target != field),
                "the cleared callback pointer must not be relocated"
            );
        }
        // Nothing else moved: same sections, same size.
        assert_eq!(patched.sections.len(), prepared.info.sections.len());
        assert_eq!(prepared.bytes.len(), original.len());
    }

    #[test]
    fn keeps_iat_slots_when_import_size_excludes_the_terminator() {
        let mut original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let info = ImageInfo::parse(&original).unwrap();
        let directory = layout::data_directory_offset(&original, dir::IMPORT).unwrap();
        // This linker layout counts the descriptors for real modules only.
        let descriptor_bytes = u32::try_from(info.imports.len()).unwrap() * IMPORT_DESCRIPTOR_SIZE;
        write_u32(&mut original, directory + 4, descriptor_bytes).unwrap();
        let prepared = prepare(&original).unwrap();
        let empty = layout::read_u32(&prepared.bytes, directory).unwrap();
        assert_eq!(
            empty,
            layout::read_u32(&original, directory).unwrap() + descriptor_bytes
        );
        assert_eq!(
            layout::read_u32(&prepared.bytes, directory + 4),
            Some(IMPORT_DESCRIPTOR_SIZE)
        );
        let at = info.rva_to_file_offset(empty).unwrap() as usize;
        assert_eq!(
            &prepared.bytes[at..at + IMPORT_DESCRIPTOR_SIZE as usize],
            &[0; 20]
        );
        let iat = layout::data_directory_offset(&original, dir::IAT).unwrap();
        assert_eq!(
            &prepared.bytes[iat..iat + layout::DATA_DIRECTORY_SIZE],
            &original[iat..iat + layout::DATA_DIRECTORY_SIZE]
        );
        assert_eq!(prepared.info.import_count(), info.import_count());
        for module in &prepared.info.imports {
            for entry in &module.entries {
                let at = info.rva_to_file_offset(entry.iat_slot_rva).unwrap() as usize;
                assert_eq!(&prepared.bytes[at..at + 8], &original[at..at + 8]);
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_assigns_static_tls_to_a_prepared_dll() {
        // Model a title DLL using our own linked image. Loading a DLL leaves
        // its assigned index intact; the executable-specific redirect resets it.
        let mut original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let flags =
            layout::file_header_offset(&original).unwrap() + layout::FILE_HEADER_CHARACTERISTICS;
        let characteristics =
            ImageInfo::parse(&original).unwrap().characteristics | file_flags::DLL;
        write_u16(&mut original, flags, characteristics).unwrap();
        let prepared = prepare(&original).unwrap();
        let info = &prepared.info;
        let tls = info.tls.as_ref().expect("the test binary uses static TLS");
        let directory = info.rva_to_file_offset(tls.directory_rva).unwrap() as usize;
        let index_va = layout::read_u64(&original, directory + 16).unwrap();
        let index_rva = index_va.checked_sub(info.image_base).unwrap();
        assert!(index_rva + 4 <= u64::from(info.size_of_image));

        let folder = tempfile::tempdir().unwrap();
        let image = crate::load(&prepared, folder.path()).unwrap();
        // SAFETY: the mapped test image contains the checked DWORD above.
        let assigned =
            unsafe { ((image.base() + index_rva as usize) as *const u32).read_unaligned() };
        // SAFETY: the prepared DLL has never been attached or given to a caller.
        unsafe { image.unload() };
        assert_ne!(
            assigned, 0,
            "the process executable already owns TLS slot zero"
        );
    }

    /// A relocation block whose page RVA is not page-aligned and lies near the
    /// top of the 32-bit RVA space: the target of its first entry is past
    /// 4 GiB and must be skipped, not overflow.
    #[test]
    fn relocation_targets_past_the_top_of_the_rva_space_are_skipped() {
        use victory_pe::SectionInfo;
        const BLOCK_AT: usize = 0x200;
        let mut bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let mut info = ImageInfo::parse(&bytes).unwrap();
        // RVA 0x1000 is 12 bytes of file data at BLOCK_AT; nothing runs them.
        info.sections = vec![SectionInfo {
            name: ".reloc".into(),
            rva: 0x1000,
            virtual_size: 12,
            file_offset: BLOCK_AT as u32,
            file_size: 12,
            characteristics: 0,
        }];
        let dir = layout::data_directory_offset(&bytes, dir::BASERELOC).unwrap();
        write_u32(&mut bytes, dir, 0x1000).unwrap();
        write_u32(&mut bytes, dir + 4, 12).unwrap();
        // One block: a page RVA of 0xFFFFF001 with two DIR64 entries, at
        // offsets 0xFFF (target 0x1_0000_0000, past the top) and 0x010.
        write_u32(&mut bytes, BLOCK_AT, 0xFFFF_F001).unwrap();
        write_u32(&mut bytes, BLOCK_AT + 4, 12).unwrap();
        write_u16(&mut bytes, BLOCK_AT + 8, 0xAFFF).unwrap();
        write_u16(&mut bytes, BLOCK_AT + 10, 0xA010).unwrap();
        assert_eq!(
            dir64_relocations(&bytes, &info),
            [(BLOCK_AT + 10, 0xFFFF_F011)]
        );
    }

    /// A TLS directory at the top of the 32-bit RVA space (in a section that
    /// ends there) with one callback pointer: the RVA of the callback field
    /// lies past 4 GiB. The image must still be prepared.
    #[test]
    fn tls_directory_at_the_top_of_the_rva_space_is_prepared() {
        const IMAGE_BASE: u64 = 0x1_4000_0000;
        const TLS_RVA: u32 = 0xFFFF_FFE8;
        const CALLBACKS_RVA: u32 = 0xFFFF_FF80;
        // The TLS directory is at file offset 0x2E8, its callback pointer 24
        // bytes in.
        const TLS_AT: usize = 0x2E8;
        let mut image = vec![0u8; 0x400];
        image[..2].copy_from_slice(b"MZ");
        write_u32(&mut image, layout::DOS_E_LFANEW, 0x40).unwrap();
        image[0x40..0x44].copy_from_slice(b"PE\0\0");
        write_u16(&mut image, 0x44, victory_pe::MACHINE_AMD64).unwrap();
        write_u16(&mut image, 0x46, 1).unwrap(); // one section
        write_u16(&mut image, 0x54, 240).unwrap(); // optional header size
        write_u16(&mut image, 0x56, 0x0022).unwrap(); // executable, large address aware
        let opt = 0x58;
        write_u16(&mut image, opt, 0x20B).unwrap(); // PE32+
        write_u64(&mut image, opt + 24, IMAGE_BASE).unwrap();
        write_u32(&mut image, opt + 32, 0x1000).unwrap(); // section alignment
        write_u32(&mut image, opt + 36, 0x200).unwrap(); // file alignment
        write_u32(&mut image, opt + 56, 0x1000).unwrap(); // size of image
        write_u32(&mut image, opt + 60, 0x200).unwrap(); // size of headers
        write_u32(&mut image, opt + layout::OPT_NUMBER_OF_RVA_AND_SIZES, 16).unwrap();
        let tls_dir = opt + layout::OPT_DATA_DIRECTORY + 9 * layout::DATA_DIRECTORY_SIZE;
        write_u32(&mut image, tls_dir, TLS_RVA).unwrap();
        write_u32(&mut image, tls_dir + 4, 0x28).unwrap();
        // The one section: raw data at 0x200, mapped at the top of the RVA space.
        let sec = opt + 240;
        image[sec..sec + 4].copy_from_slice(b".tls");
        write_u32(&mut image, sec + 8, 0x200).unwrap(); // virtual size
        write_u32(&mut image, sec + 12, 0xFFFF_FF00).unwrap(); // virtual address
        write_u32(&mut image, sec + 16, 0x200).unwrap(); // raw size
        write_u32(&mut image, sec + 20, 0x200).unwrap(); // raw pointer
        write_u64(
            &mut image,
            TLS_AT + 24,
            IMAGE_BASE + u64::from(CALLBACKS_RVA),
        )
        .unwrap();
        // The callback array (at CALLBACKS_RVA) is empty: a zero slot.

        let prepared = prepare(&image).unwrap();
        assert_eq!(
            layout::read_u64(&prepared.bytes, TLS_AT + 24),
            Some(0),
            "the callback pointer is cleared"
        );
    }

    #[test]
    fn file_name_is_stable_per_build() {
        let original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let a = prepare(&original).unwrap();
        let b = prepare(&original).unwrap();
        assert_eq!(a.file_name(), b.file_name());
    }
}
