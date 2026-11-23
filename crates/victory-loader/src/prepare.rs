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
/// * import, bound-import and IAT directories cleared: we bind the original
///   IAT slots ourselves, using `info.imports`;
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

    // Clear the IAT directory along with the import directory for Windows 10.
    // The slots themselves stay in the image for our binder.
    for index in [dir::IMPORT, dir::BOUND_IMPORT, dir::IAT] {
        if let Some(at) = layout::data_directory_offset(&bytes, index) {
            write(&mut bytes, at, &[0; layout::DATA_DIRECTORY_SIZE])?;
        }
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
        let field_rva = tls.directory_rva + layout::TLS64_ADDRESS_OF_CALLBACKS as u32;
        for (entry, target) in dir64_relocations(&bytes, &info) {
            if target == field_rva {
                write_u16(&mut bytes, entry, 0)?; // IMAGE_REL_BASED_ABSOLUTE
            }
        }
    }

    Ok(PreparedImage {
        info,
        bytes,
        original_hash: blake3::hash(original),
    })
}

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
            if e >> 12 == DIR64 {
                out.push((entry, page + u32::from(e & 0xFFF)));
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
        for index in [dir::IMPORT, dir::BOUND_IMPORT, dir::IAT] {
            let at = layout::data_directory_offset(&original, index).unwrap();
            assert_eq!(
                &prepared.bytes[at..at + layout::DATA_DIRECTORY_SIZE],
                &[0; 8]
            );
        }
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
        let descriptor_bytes = u32::try_from(info.imports.len() * 20).unwrap();
        write_u32(&mut original, directory + 4, descriptor_bytes).unwrap();
        let prepared = prepare(&original).unwrap();
        let iat = layout::data_directory_offset(&original, dir::IAT).unwrap();
        assert_ne!(layout::read_u32(&original, iat), Some(0));
        assert_eq!(
            &prepared.bytes[iat..iat + layout::DATA_DIRECTORY_SIZE],
            &[0; 8]
        );
        assert_eq!(prepared.info.import_count(), info.import_count());
        for module in &prepared.info.imports {
            for entry in &module.entries {
                let at = info.rva_to_file_offset(entry.iat_slot_rva).unwrap() as usize;
                assert_eq!(&prepared.bytes[at..at + 8], &original[at..at + 8]);
            }
        }
    }

    #[test]
    fn file_name_is_stable_per_build() {
        let original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let a = prepare(&original).unwrap();
        let b = prepare(&original).unwrap();
        assert_eq!(a.file_name(), b.file_name());
    }
}
