//! Xbox Series shaders to host shaders.
//!
//! An Xbox Series shader is a standard DXBC container ([`dxbc`]). Its `DXIL`
//! part ([`dxil`]) may carry Xbox additions around the LLVM bitcode: an
//! `XDAT` table before it indexing blobs after it, among them `xbop`
//! (compiler options) and the zlib-compressed GFX10 code. The bitcode may use
//! console-only intrinsics, so the GFX10 code is translated in general.

pub mod bvh;
pub mod bvh_build;
pub mod dxbc;
pub mod dxil;
pub mod emit;
pub mod footprint;
pub mod gfx10;
pub mod io;
pub mod ir;
pub mod native;
pub mod resource;
pub mod spill;
pub mod structure;

#[cfg(test)]
mod footprint_tests;

pub use dxbc::{Container, FourCC, Part};
pub use dxil::{DxilProgram, ShaderKind, XboxExtras};
pub use native::{
    HwStage, NativeProgram, ResourceType, RootParam, RootType, UsedResource, native_programs,
};

/// Whether a `VICTORY_*` switch's value turns it on: it is set, and is not
/// empty, `0` or `false` (any case). A switch used to be on whenever its
/// variable existed, so `VICTORY_X=0`, typed to switch it off or left over
/// from comparing two runs, still switched it on.
fn value_is_on(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false")))
}

/// Whether the environment variable `name` switches something on
/// (`value_is_on`). The one reading of an "any value" switch; a switch
/// that is on unless it is `0` is read where it is used.
pub fn env_on(name: &str) -> bool {
    value_is_on(std::env::var_os(name).as_deref())
}

#[cfg(test)]
mod switch_tests {
    use super::value_is_on;
    use std::ffi::OsStr;

    #[test]
    fn unset_empty_zero_and_false_are_off() {
        assert!(!value_is_on(None));
        for value in ["", "0", "false", "FALSE", "False"] {
            assert!(!value_is_on(Some(OsStr::new(value))), "{value:?}");
        }
    }

    #[test]
    fn any_other_value_is_on() {
        for value in ["1", "true", "yes", "2", "00", " 0", "all"] {
            assert!(value_is_on(Some(OsStr::new(value))), "{value:?}");
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShaderError {
    #[error("not a DXBC container")]
    NotDxbc,
    #[error("malformed shader: {0}")]
    Malformed(&'static str),
    #[error("GFX10 code at {offset:#x}: {reason}")]
    Isa { offset: usize, reason: &'static str },
    /// Valid code the translator cannot handle yet: an opcode name or a construct.
    #[error("GFX10 code at {offset:#x}: {what} is not supported")]
    Unsupported { offset: usize, what: &'static str },
}
