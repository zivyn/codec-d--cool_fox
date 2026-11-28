//! Device methods 0x218 (`DeserializeComputePipelineStateX`) and 0x1c8, the
//! console's pipelines from precompiled pieces.
//!
//! The upscaler of the title (the AMD FidelityFX library it loads) asks for
//! each of its compute programs here first, and the title does so for some
//! thousands of its own; on failure each makes the same pipeline from the
//! ordinary description (0x1f0, 0x2b0), and that is what ran until now.
//! What the call takes, as read from the calls (the first sixteen words of
//! the description, 64-bit):
//!
//! ```text
//! w0       the root signature object
//! w1  w2   packet     (0x1c bytes)  the FSPX entry of kind 0
//! w3  w4   hash       (0x14 bytes)  kind 1
//! w5  w6   code       (a 256-aligned copy of the program's GFX10 code)  kind 6
//! w7  w8   metadata   (0x403 record list)                               kind 7
//! w9  w10  two pointers (not read)       w11 a 64-bit key       w12 0
//! ```
//!
//! The four pieces are what an ordinary container's `ACCC` part holds
//! (`victory_shader::native::fspx_program`), so a pipeline made from them
//! runs the same code with the same layout. [`assemble_fspx`] lays them out
//! as the console's tool does (entries of 16 bytes from +0x10, each piece
//! 16-aligned and the code 256-aligned, the whole padded to 16): checked
//! byte for byte against the 29 programs of the upscaler's library.
//!
//! `VICTORY_PRECOMPILED` (read once, off by default: the title falls back as
//! before):
//! - `fsr` (or `1`): a call from the upscaler's library gets a pipeline;
//!   the title's own calls still fail.
//! - `all`: every call whose pieces are known gets one.
//!
//! A call whose pieces are not what is described (a size, the metadata's
//! magic, a program that does not parse) fails as it always did, so a
//! different version of the library falls back by itself.
//!
//! `VICTORY_PRECOMPILED_CALLER=<text>`: the module name (lower case) that
//! counts as the upscaler (`fidelityfx`).
//!
//! `VICTORY_DUMP_PRECOMPILED=<folder>`: for each call (to 0x218 and 0x1c8),
//! a row of `calls.tsv` (the words of the description, the modules on the
//! stack, what each piece begins with and what the pointers after them point
//! at) and each piece as `<hash>.bin`, once. Diagnostic: a run's folder
//! belongs outside the repository. `VICTORY_DUMP_PRECOMPILED_MAX` (4000)
//! bounds the calls dumped, the upscaler's always count.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::object::{E_NOTIMPL, HRESULT, Iid, This};

const KIND_PACKET: u32 = 0;
const KIND_HASH: u32 = 1;
const KIND_CODE: u32 = 6;
const KIND_METADATA: u32 = 7;
const PACKET_BYTES: usize = 0x1c;
const HASH_BYTES: usize = 0x14;
/// The metadata's first word, and where its list of records begins.
const META_MAGIC: u32 = 0x403;
const META_HEADER: usize = 0x18;
const MAX_PIECE: usize = 16 << 20;
/// Words of the description read (the call's own are 13).
const WORDS: usize = 16;

/// The four pieces a call names, in the description's order.
struct Pieces {
    packet: Vec<u8>,
    hash: Vec<u8>,
    code: Vec<u8>,
    meta: Vec<u8>,
}

fn align(n: usize, to: usize) -> usize {
    n.next_multiple_of(to)
}

/// The `FSPX` blob of an ordinary container's `ACCC` part, made of the four
/// pieces: header (`FSPX`, version 1, entry count, size), entries of
/// `kind, offset, size, 0`, each piece at the next 16 (the code, 256), the
/// size to 16.
fn assemble_fspx(p: &Pieces) -> Vec<u8> {
    let parts = [
        (KIND_PACKET, &p.packet, 16),
        (KIND_HASH, &p.hash, 16),
        (KIND_CODE, &p.code, 256),
        (KIND_METADATA, &p.meta, 16),
    ];
    let mut out = vec![0u8; 16 + 16 * parts.len()];
    for (i, (kind, piece, to)) in parts.into_iter().enumerate() {
        let at = align(out.len(), to);
        out.resize(at, 0);
        let entry = 16 + 16 * i;
        out[entry..entry + 4].copy_from_slice(&kind.to_le_bytes());
        out[entry + 4..entry + 8].copy_from_slice(&(at as u32).to_le_bytes());
        out[entry + 8..entry + 12].copy_from_slice(&(piece.len() as u32).to_le_bytes());
        out.extend_from_slice(piece);
    }
    out.resize(align(out.len(), 16), 0);
    out[0..4].copy_from_slice(b"FSPX");
    out[4..8].copy_from_slice(&1u32.to_le_bytes());
    out[8..12].copy_from_slice(&(parts.len() as u32).to_le_bytes());
    let size = out.len() as u32;
    out[12..16].copy_from_slice(&size.to_le_bytes());
    out
}

/// The container of the pipeline: the `ACCC` part alone (what the
/// translation reads for a compute program).
fn container(p: &Pieces) -> Result<Vec<u8>, &'static str> {
    if p.packet.len() != PACKET_BYTES {
        return Err("the packet is not 0x1c bytes");
    }
    if p.hash.len() != HASH_BYTES {
        return Err("the hash is not 0x14 bytes");
    }
    if p.code.is_empty() || p.code.len() % 4 != 0 {
        return Err("the code is not whole dwords");
    }
    if p.meta.len() < META_HEADER || le32(&p.meta, 0) != META_MAGIC {
        return Err("the metadata is not of the known version");
    }
    let fspx = assemble_fspx(p);
    // The parse that translation will do: a program it cannot read is
    // refused here, where the title can still fall back.
    victory_shader::native::fspx_program(&fspx).map_err(|_| "the program does not parse")?;
    Ok(victory_shader::dxbc::build(&[(
        victory_shader::FourCC::ACCC,
        &fspx,
    )]))
}

fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map_or(0, |w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

/// Which calls get a pipeline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Off,
    Upscaler,
    All,
}

fn parse_mode(value: Option<&str>) -> Mode {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None | Some("" | "0" | "off") => Mode::Off,
        Some("1" | "fsr" | "upscaler") => Mode::Upscaler,
        Some("all") => Mode::All,
        Some(other) => {
            tracing::warn!(value = other, "VICTORY_PRECOMPILED is off, fsr or all: off");
            Mode::Off
        }
    }
}

fn mode() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| {
        let mode = parse_mode(std::env::var("VICTORY_PRECOMPILED").ok().as_deref());
        if mode != Mode::Off {
            tracing::info!(
                ?mode,
                "pipelines from the precompiled pieces (VICTORY_PRECOMPILED)"
            );
        }
        mode
    })
}

/// Whether one of the modules on a call's stack is the upscaler's.
fn from_upscaler(modules: &[String]) -> bool {
    static NAME: OnceLock<String> = OnceLock::new();
    let name = NAME.get_or_init(|| {
        std::env::var("VICTORY_PRECOMPILED_CALLER")
            .ok()
            .map(|v| v.to_ascii_lowercase())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "fidelityfx".to_owned())
    });
    modules.iter().any(|m| m.contains(name.as_str()))
}

fn answers(mode: Mode, upscaler: bool) -> bool {
    match mode {
        Mode::Off => false,
        Mode::Upscaler => upscaler,
        Mode::All => true,
    }
}

/// Bytes of title memory at `at`; `None` if any of it is not readable.
fn read_bytes(at: u64, len: usize) -> Option<Vec<u8>> {
    if at < 0x1_0000 || len == 0 || len > MAX_PIECE {
        return None;
    }
    let mut bytes = vec![0u8; len];
    #[cfg(windows)]
    let read = crate::host::read_into(at as usize as *const u8, &mut bytes);
    // SAFETY: the title names `len` bytes at `at`.
    #[cfg(not(windows))]
    let read = {
        unsafe { std::ptr::copy_nonoverlapping(at as usize as *const u8, bytes.as_mut_ptr(), len) };
        true
    };
    read.then_some(bytes)
}

/// The description's first words, as many as are readable (zeros after).
fn read_words(desc: *const u8) -> [u64; WORDS] {
    let mut bytes = [0u8; 8 * WORDS];
    #[cfg(windows)]
    crate::host::read_there(desc, &mut bytes);
    // SAFETY: the title's description, thirteen words long at least.
    #[cfg(not(windows))]
    unsafe {
        std::ptr::copy_nonoverlapping(desc, bytes.as_mut_ptr(), 8 * 13)
    };
    let mut words = [0u64; WORDS];
    for (w, c) in words.iter_mut().zip(bytes.chunks_exact(8)) {
        *w = u64::from_le_bytes(c.try_into().expect("8 bytes"));
    }
    words
}

/// The four pieces the words name.
fn read_pieces(words: &[u64; WORDS]) -> Option<Pieces> {
    let piece = |i: usize| read_bytes(words[1 + 2 * i], usize::try_from(words[2 + 2 * i]).ok()?);
    Some(Pieces {
        packet: piece(0)?,
        hash: piece(1)?,
        code: piece(2)?,
        meta: piece(3)?,
    })
}

/// The modules on the stack of the call: the return addresses above the
/// device method's own (file names, lower case).
#[inline(never)]
fn modules() -> Vec<String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
        use windows_sys::Win32::System::LibraryLoader::{
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            GetModuleFileNameW, GetModuleHandleExW,
        };
        let mut frames = [std::ptr::null_mut(); 4];
        // SAFETY: room for four frames; skips this function.
        let n =
            unsafe { RtlCaptureStackBackTrace(1, 4, frames.as_mut_ptr(), std::ptr::null_mut()) };
        let mut names: Vec<String> = Vec::new();
        for &frame in &frames[..usize::from(n)] {
            let mut module = std::ptr::null_mut();
            // SAFETY: looks up the module holding the address, no reference.
            let found = unsafe {
                GetModuleHandleExW(
                    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                        | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                    frame as *const u16,
                    &mut module,
                )
            };
            if found == 0 {
                continue;
            }
            let mut name = [0u16; 520];
            // SAFETY: a buffer of its stated length.
            let len = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) }
                as usize;
            let path = String::from_utf16_lossy(&name[..len.min(name.len())]);
            let file = path
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if !file.is_empty() && !names.contains(&file) {
                names.push(file);
            }
        }
        names
    }
    #[cfg(not(windows))]
    Vec::new()
}

/// Device method 0x218, `(this, description, interface id, out)`.
pub(crate) extern "system" fn deserialize_compute(
    this: This,
    desc: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // As before, and cheaply, unless the pieces are used or dumped.
    if mode() == Mode::Off && !dump::on() {
        return crate::device::unknown_pipeline::<0x218>(this, desc, iid, out);
    }
    let modules = modules();
    let hr = deserialize(desc, iid, out, mode(), &modules);
    if hr != E_NOTIMPL {
        return hr;
    }
    crate::device::unknown_pipeline::<0x218>(this, desc, iid, out)
}

/// Device method 0x1c8 (graphics): not made, only dumped.
pub(crate) extern "system" fn deserialize_graphics(
    this: This,
    desc: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if dump::on() && !desc.is_null() {
        dump::call(0x1c8, desc, &modules(), "not made");
    }
    crate::device::unknown_pipeline::<0x1c8>(this, desc, iid, out)
}

/// A call made by `modules`, in `mode`: the pipeline, or `E_NOTIMPL` (the
/// title then falls back).
fn deserialize(
    desc: *const u8,
    iid: *const Iid,
    out: *mut This,
    mode: Mode,
    modules: &[String],
) -> HRESULT {
    static ASKED: AtomicU32 = AtomicU32::new(0);
    static ANSWERED: AtomicU32 = AtomicU32::new(0);
    let upscaler = from_upscaler(modules);
    let wanted = answers(mode, upscaler);
    let dumping = dump::on() && !desc.is_null();
    if desc.is_null() || (!wanted && !dumping) {
        if upscaler {
            tracing::info!(mode = ?mode, "a call of the upscaler to the precompiled pipeline method: it falls back");
        }
        return E_NOTIMPL;
    }
    let asked = ASKED.fetch_add(1, Ordering::Relaxed) + 1;
    let pieces = if wanted {
        read_pieces(&read_words(desc))
    } else {
        None
    };
    let made = match (&pieces, wanted) {
        (_, false) => Err("not asked for (VICTORY_PRECOMPILED)"),
        (None, true) => Err("a piece is not readable"),
        (Some(p), true) => container(p),
    };
    let note = match &made {
        Ok(_) => "made",
        Err(why) => *why,
    };
    if dumping {
        dump::call(0x218, desc, modules, note);
    }
    match made {
        Ok(container) => {
            let n = ANSWERED.fetch_add(1, Ordering::Relaxed) + 1;
            let p = pieces.as_ref().expect("made from pieces");
            let digest = blake3::hash(&container).to_hex()[..16].to_owned();
            if upscaler || n <= 40 {
                let at =
                    |i: usize| u16::from_le_bytes([p.packet[12 + 2 * i], p.packet[13 + 2 * i]]);
                tracing::info!(
                    call = asked,
                    upscaler,
                    kind1 = %hex(&p.hash),
                    code = p.code.len(),
                    meta = p.meta.len(),
                    threads = ?[at(0), at(1), at(2)],
                    hash = %digest,
                    "a pipeline from the precompiled pieces"
                );
            }
            crate::pipeline::make_compute(&container, iid, out)
        }
        Err(why) => {
            if wanted && (upscaler || asked <= 8) {
                tracing::warn!(
                    call = asked,
                    upscaler,
                    why,
                    "the precompiled pieces are not used: the title falls back"
                );
            }
            E_NOTIMPL
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(2 * bytes.len()), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// `VICTORY_DUMP_PRECOMPILED`: what the title hands the methods, for the one
/// who reverses the format (see the module's text).
mod dump {
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    use super::{MAX_PIECE, WORDS, hex, read_bytes, read_words};

    pub(super) struct Dump {
        dir: PathBuf,
        max: u32,
        calls: u32,
        header: bool,
    }

    impl Dump {
        pub(super) fn new(dir: PathBuf, max: u32) -> Dump {
            Dump {
                dir,
                max,
                calls: 0,
                header: false,
            }
        }

        /// A pointer that can be read (user space).
        fn plausible(w: u64) -> bool {
            (0x1_0000..0x7fff_ffff_ffff).contains(&w)
        }

        /// One call's row, and its pieces as files (once each).
        pub(super) fn record(
            &mut self,
            slot: usize,
            desc: *const u8,
            modules: &[String],
            note: &str,
        ) {
            // (The upscaler's calls are all kept; the rest to the bound.)
            if self.calls >= self.max && !super::from_upscaler(modules) {
                return;
            }
            self.calls += 1;
            if std::fs::create_dir_all(&self.dir).is_err() {
                return;
            }
            let words = read_words(desc);
            let mut row = format!("{}\t{slot:#x}\t{}\t{note}\t", self.calls, modules.join("|"));
            row.push_str(
                &words
                    .iter()
                    .map(|w| format!("{w:x}"))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            // The pieces: pairs of (pointer, size) from word 1 while they make sense.
            let mut next = 1;
            while next + 1 < WORDS {
                let (at, size) = (words[next], words[next + 1]);
                if !Self::plausible(at) || size == 0 || size > MAX_PIECE as u64 {
                    break;
                }
                let Some(bytes) = read_bytes(at, size as usize) else {
                    break;
                };
                let name = &blake3::hash(&bytes).to_hex()[..16];
                let file = self.dir.join(format!("{name}.bin"));
                if !file.exists() {
                    let _ = std::fs::write(&file, &bytes);
                }
                let first = hex(&bytes[..bytes.len().min(32)]);
                row.push_str(&format!("\tp{}={name}:{size:#x}:{first}", (next - 1) / 2));
                next += 2;
            }
            // What the words around them point at: the root signature object, and the rest.
            for (j, &w) in words.iter().enumerate() {
                if (j == 0 || j >= next)
                    && Self::plausible(w)
                    && let Some(bytes) = read_bytes(w, 64)
                {
                    row.push_str(&format!("\tw{j}->{}", hex(&bytes)));
                }
            }
            row.push('\n');
            let table = self.dir.join("calls.tsv");
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(table)
            {
                if !self.header {
                    self.header = true;
                    let _ = file.write_all(
                        b"call\tslot\tmodules\tnote\twords\tpieces: p<i>=<hash>:<size>:<first 32 bytes>\tw<j>-><64 bytes at word j>\n",
                    );
                }
                let _ = file.write_all(row.as_bytes());
            }
        }
    }

    fn state() -> &'static Mutex<Option<Dump>> {
        static STATE: OnceLock<Mutex<Option<Dump>>> = OnceLock::new();
        STATE.get_or_init(|| {
            let dir = std::env::var_os("VICTORY_DUMP_PRECOMPILED")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from);
            let max = std::env::var("VICTORY_DUMP_PRECOMPILED_MAX")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4000);
            Mutex::new(dir.map(|dir| Dump::new(dir, max)))
        })
    }

    pub(super) fn on() -> bool {
        state().lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    pub(super) fn call(slot: usize, desc: *const u8, modules: &[String], note: &str) {
        if let Some(dump) = state().lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            dump.record(slot, desc, modules, note);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{S_OK, slot};
    use crate::pipeline::{PIPELINE_IIDS, containers_of};

    /// A program of `dwords` dwords of `s_endpgm`, as the four pieces name it: wave32, 8x16x1 threads.
    fn pieces(dwords: usize) -> Pieces {
        let mut packet = vec![0u8; PACKET_BYTES];
        packet[0x0c..0x0e].copy_from_slice(&8u16.to_le_bytes());
        packet[0x0e..0x10].copy_from_slice(&16u16.to_le_bytes());
        packet[0x10..0x12].copy_from_slice(&1u16.to_le_bytes());
        packet[0x12..0x14].copy_from_slice(&(1u16 << 4).to_le_bytes());
        packet[0x14..0x18].copy_from_slice(&1u32.to_le_bytes());
        packet[0x18..0x1c].copy_from_slice(&0x9cu32.to_le_bytes());
        let code: Vec<u8> = [0x00u8, 0x00, 0x81, 0xBF].repeat(dwords);
        let mut meta = vec![0u8; META_HEADER + 32];
        meta[0..4].copy_from_slice(&META_MAGIC.to_le_bytes());
        let listed = (meta.len() - 0x14) as u32;
        meta[4..8].copy_from_slice(&listed.to_le_bytes());
        meta[8..12].copy_from_slice(&(dwords as u32).to_le_bytes());
        meta[0x14] = 0x40;
        meta[META_HEADER] = 0x43;
        Pieces {
            packet,
            hash: vec![0x11; HASH_BYTES],
            code,
            meta,
        }
    }

    /// The description of a call: the root signature, the four pieces, two pointers, a key.
    fn description(p: &Pieces) -> [u64; WORDS] {
        let at = |v: &[u8]| v.as_ptr() as u64;
        let mut words = [0u64; WORDS];
        words[1..9].copy_from_slice(&[
            at(&p.packet),
            p.packet.len() as u64,
            at(&p.hash),
            p.hash.len() as u64,
            at(&p.code),
            p.code.len() as u64,
            at(&p.meta),
            p.meta.len() as u64,
        ]);
        // (Two pointers the call carries and a key: not read.)
        words[11] = 7;
        words
    }

    fn call(p: &Pieces, mode: Mode, modules: &[&str]) -> (HRESULT, This) {
        let words = description(p);
        let modules: Vec<String> = modules.iter().map(|m| (*m).to_owned()).collect();
        let mut out = std::ptr::null();
        let hr = deserialize(
            words.as_ptr().cast(),
            &PIPELINE_IIDS[0],
            &mut out,
            mode,
            &modules,
        );
        (hr, out)
    }

    fn release(p: This) {
        slot::<extern "system" fn(This) -> u32>(p, 0x10)(p);
    }

    /// The layout the console's tool gives the pieces of a 0x180-byte program (the sizes of one of
    /// the library's): packet at 0x50, hash at 0x70, code at 0x100, metadata after it.
    #[test]
    fn the_fspx_is_laid_out_as_the_consoles_tool_does() {
        let p = Pieces {
            code: vec![0; 0x180],
            meta: vec![0; 0x118],
            ..pieces(1)
        };
        let fspx = assemble_fspx(&p);
        let entry = |i: usize| {
            (
                le32(&fspx, 16 + 16 * i),
                le32(&fspx, 20 + 16 * i),
                le32(&fspx, 24 + 16 * i),
                le32(&fspx, 28 + 16 * i),
            )
        };
        assert_eq!(&fspx[..4], b"FSPX");
        assert_eq!((le32(&fspx, 4), le32(&fspx, 8)), (1, 4));
        assert_eq!(entry(0), (0, 0x50, 0x1c, 0));
        assert_eq!(entry(1), (1, 0x70, 0x14, 0));
        assert_eq!(entry(2), (6, 0x100, 0x180, 0));
        assert_eq!(entry(3), (7, 0x280, 0x118, 0));
        // 0x280 + 0x118 = 0x398, padded to 16.
        assert_eq!(fspx.len(), 0x3a0);
        assert_eq!(le32(&fspx, 12) as usize, fspx.len());
        // A size that is not a multiple of 16 pads the next piece and the end.
        let q = Pieces {
            code: vec![0; 0x84],
            meta: vec![0; 0x1b],
            ..pieces(1)
        };
        let fspx = assemble_fspx(&q);
        assert_eq!((le32(&fspx, 52), le32(&fspx, 56)), (0x100, 0x84));
        assert_eq!((le32(&fspx, 68), le32(&fspx, 72)), (0x190, 0x1b));
        assert_eq!(fspx.len(), 0x1b0);
    }

    #[test]
    fn the_pieces_make_the_program_the_translation_reads() {
        let p = pieces(3);
        let container = container(&p).expect("known pieces");
        let c = victory_shader::Container::parse(&container).unwrap();
        let programs =
            victory_shader::native_programs(c.part(victory_shader::FourCC::ACCC).unwrap().data)
                .unwrap();
        assert_eq!(programs.len(), 1);
        assert_eq!(programs[0].code, p.code);
        assert!(!programs[0].wave64);
        match &programs[0].io {
            victory_shader::native::StageIo::Cs(cs) => assert_eq!(cs.threads, [8, 16, 1]),
            _ => panic!("not a compute program"),
        }
    }

    #[test]
    fn pieces_of_another_version_are_refused() {
        let refused = |change: &dyn Fn(&mut Pieces)| {
            let mut p = pieces(2);
            change(&mut p);
            container(&p).is_err()
        };
        assert!(!refused(&|_| {}));
        assert!(refused(&|p| p.packet.push(0)));
        assert!(refused(&|p| p.hash.truncate(0x10)));
        assert!(refused(&|p| p.code.truncate(7)));
        assert!(refused(&|p| p.code.clear()));
        assert!(refused(
            &|p| p.meta[0..4].copy_from_slice(&0x404u32.to_le_bytes())
        ));
        assert!(refused(&|p| p.meta.truncate(8)));
        // Metadata that names another code length than the code has.
        assert!(refused(
            &|p| p.meta[8..12].copy_from_slice(&9u32.to_le_bytes())
        ));
    }

    #[test]
    fn the_mode_names() {
        assert_eq!(parse_mode(None), Mode::Off);
        assert_eq!(parse_mode(Some("")), Mode::Off);
        assert_eq!(parse_mode(Some("0")), Mode::Off);
        assert_eq!(parse_mode(Some("off")), Mode::Off);
        assert_eq!(parse_mode(Some("1")), Mode::Upscaler);
        assert_eq!(parse_mode(Some(" FSR ")), Mode::Upscaler);
        assert_eq!(parse_mode(Some("all")), Mode::All);
        assert_eq!(parse_mode(Some("maybe")), Mode::Off);
        assert!(!answers(Mode::Off, true));
        assert!(answers(Mode::Upscaler, true));
        assert!(!answers(Mode::Upscaler, false));
        assert!(answers(Mode::All, false));
    }

    #[test]
    fn the_upscaler_is_known_by_its_module() {
        assert!(from_upscaler(&[
            "victory.exe".into(),
            "amd_fidelityfx_upscaler_xbox.dll".into()
        ]));
        assert!(!from_upscaler(&["victory.exe".into(), "title.exe".into()]));
        assert!(!from_upscaler(&[]));
    }

    #[test]
    fn a_call_gets_a_pipeline_only_where_it_is_asked_for() {
        let p = pieces(4);
        let upscaler = ["victory.exe", "amd_fidelityfx_upscaler_xbox.dll"];
        let title = ["victory.exe", "title.exe"];
        // Off: the title falls back, as before.
        for modules in [&upscaler[..], &title[..]] {
            let (hr, out) = call(&p, Mode::Off, modules);
            assert_eq!(hr, E_NOTIMPL);
            assert!(out.is_null());
        }
        // The upscaler's mode: its calls only.
        let (hr, out) = call(&p, Mode::Upscaler, &title);
        assert_eq!((hr, out.is_null()), (E_NOTIMPL, true));
        let (hr, out) = call(&p, Mode::Upscaler, &upscaler);
        assert_eq!(hr, S_OK);
        assert!(!out.is_null());
        let containers = containers_of(out);
        assert_eq!(containers, [container(&p).unwrap()]);
        release(out);
        // All: the title's too.
        let (hr, out) = call(&p, Mode::All, &title);
        assert_eq!(hr, S_OK);
        release(out);
        // Pieces of another version fall back even when asked for.
        let mut other = pieces(4);
        other.meta[0..4].copy_from_slice(&0x500u32.to_le_bytes());
        let (hr, out) = call(&other, Mode::All, &upscaler);
        assert_eq!((hr, out.is_null()), (E_NOTIMPL, true));
    }

    #[test]
    fn a_null_description_falls_back() {
        let mut out = std::ptr::null();
        assert_eq!(
            deserialize(
                std::ptr::null(),
                &PIPELINE_IIDS[0],
                &mut out,
                Mode::All,
                &[]
            ),
            E_NOTIMPL
        );
    }

    /// A pipeline made twice from the same pieces is one container (one translation).
    #[test]
    fn the_same_pieces_are_one_container() {
        let p = pieces(2);
        let a = container(&p).unwrap();
        assert_eq!(a, container(&pieces(2)).unwrap());
        assert_ne!(a, container(&pieces(3)).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn the_dump_names_each_piece_by_its_content() {
        let folder = std::env::temp_dir().join(format!("precompiled-dump-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let mut dump = dump::Dump::new(folder.clone(), 10);
        let p = pieces(2);
        let words = description(&p);
        dump.record(
            0x218,
            words.as_ptr().cast(),
            &["victory.exe".into()],
            "made",
        );
        dump.record(
            0x218,
            words.as_ptr().cast(),
            &["victory.exe".into()],
            "made",
        );
        let rows = std::fs::read_to_string(folder.join("calls.tsv")).unwrap();
        let lines: Vec<&str> = rows.lines().collect();
        // A header and the two calls.
        assert_eq!(lines.len(), 3);
        let fields: Vec<&str> = lines[1].split('\t').collect();
        assert_eq!(
            (fields[0], fields[1], fields[2], fields[3]),
            ("1", "0x218", "victory.exe", "made")
        );
        let named: Vec<&&str> = fields.iter().filter(|f| f.starts_with('p')).collect();
        assert_eq!(named.len(), 4, "{fields:?}");
        // The packet's size and its first bytes.
        assert!(
            named[0].starts_with("p0=") && named[0].contains(":0x1c:"),
            "{}",
            named[0]
        );
        let code_name = named[2].split(['=', ':']).nth(1).unwrap();
        assert_eq!(
            std::fs::read(folder.join(format!("{code_name}.bin"))).unwrap(),
            p.code
        );
        // Both calls name the same four files.
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 5);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[cfg(windows)]
    #[test]
    fn the_dump_stops_at_its_bound_except_for_the_upscaler() {
        let folder = std::env::temp_dir().join(format!("precompiled-bound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let mut dump = dump::Dump::new(folder.clone(), 1);
        let p = pieces(2);
        let words = description(&p);
        let title = ["title.exe".to_owned()];
        let upscaler = ["amd_fidelityfx_upscaler_xbox.dll".to_owned()];
        dump.record(0x218, words.as_ptr().cast(), &title, "a");
        dump.record(0x218, words.as_ptr().cast(), &title, "b");
        dump.record(0x218, words.as_ptr().cast(), &upscaler, "c");
        let rows = std::fs::read_to_string(folder.join("calls.tsv")).unwrap();
        let notes: Vec<&str> = rows
            .lines()
            .skip(1)
            .map(|l| l.split('\t').nth(3).unwrap())
            .collect();
        assert_eq!(notes, ["a", "c"]);
        let _ = std::fs::remove_dir_all(&folder);
    }
}
