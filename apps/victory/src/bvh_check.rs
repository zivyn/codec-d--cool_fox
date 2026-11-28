//! `victory bvh-check <folder>`: the ray-tracing structures in a dump of the
//! title memory by address (`VICTORY_MEMORY_DUMP=<folder>`, a file `go` in
//! it; the run writes `pages.txt` and `arena-<n>.bin`), checked one by one
//! with [`victory_shader::bvh_build::check`], and which of them overlap.
//!
//! The candidates are the addresses the dump names as walked (what the
//! trace programs were given) or built (what the structure builder wrote).
//! One that does not start with a header (version 6) is not a structure and
//! is counted apart. A structure still packed is not walked.

use std::collections::{BTreeSet, HashMap, hash_map::Entry};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use victory_shader::bvh_build::{self, Fault, Header, Report};

/// A page of title memory, as the dump holds them.
const PAGE: u64 = 1 << 16;

struct Dump {
    folder: PathBuf,
    /// Page address: the arena and where the page starts in it.
    pages: HashMap<u64, (usize, u64)>,
    candidates: BTreeSet<u64>,
    files: HashMap<usize, File>,
}

fn hex(text: &str) -> Option<u64> {
    u64::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}

impl Dump {
    fn load(folder: &Path) -> Result<Self> {
        let path = folder.join("pages.txt");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut dump = Self {
            folder: folder.to_owned(),
            pages: HashMap::new(),
            candidates: BTreeSet::new(),
            files: HashMap::new(),
        };
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("walked" | "built") => {
                    if let Some(address) = parts.next().and_then(hex) {
                        dump.candidates.insert(address);
                    }
                }
                Some("packed" | "frame") | None => {}
                Some(page) => {
                    let arena = parts.next().and_then(|a| a.parse().ok());
                    let at = parts.next().and_then(hex);
                    if let (Some(page), Some(arena), Some(at)) = (hex(page), arena, at) {
                        dump.pages.insert(page, (arena, at));
                    }
                }
            }
        }
        Ok(dump)
    }

    /// `out` from `inside` the page at `page`; whether the dump has it.
    fn fill(&mut self, page: u64, inside: u64, out: &mut [u8]) -> bool {
        let Some(&(arena, at)) = self.pages.get(&page) else {
            return false;
        };
        let file = match self.files.entry(arena) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(v) => match File::open(self.folder.join(format!("arena-{arena}.bin"))) {
                Ok(f) => v.insert(f),
                Err(_) => return false,
            },
        };
        file.seek(SeekFrom::Start(at + inside)).is_ok() && file.read_exact(out).is_ok()
    }

    /// `dwords` from `address` (pages the dump lacks read as zeros) and the
    /// number of page pieces it lacked.
    fn read(&mut self, address: u64, dwords: usize) -> (Vec<u32>, usize) {
        let mut bytes = vec![0u8; dwords * 4];
        let (mut done, mut holes) = (0usize, 0usize);
        while done < bytes.len() {
            let at = address + done as u64;
            let page = at & !(PAGE - 1);
            let inside = at - page;
            let n = ((PAGE - inside) as usize).min(bytes.len() - done);
            if !self.fill(page, inside, &mut bytes[done..done + n]) {
                holes += 1;
            }
            done += n;
        }
        let words = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        (words, holes)
    }
}

/// A structure found, and what its check said.
struct Found {
    address: u64,
    header: Header,
    /// `None`: still packed, not walked.
    verdict: Option<Result<Report, Fault>>,
    holes: usize,
}

impl Found {
    fn whole(&self) -> bool {
        matches!(&self.verdict, Some(Ok(r)) if r.loose == 0 && r.reached + 1 == r.size)
            && self.holes == 0
    }

    fn end(&self) -> u64 {
        self.address + self.header.nodes as u64 * 64
    }
}

/// The structures at the dump's candidate addresses, checked, and how many
/// candidates were not one.
fn survey(dump: &mut Dump) -> (Vec<Found>, usize) {
    let mut found = Vec::new();
    let mut not = 0;
    for address in dump.candidates.clone() {
        let (head, _) = dump.read(address, 16);
        let Some(header) = Header::read(&head) else {
            not += 1;
            continue;
        };
        let (words, holes) = dump.read(address, header.nodes * 16);
        let verdict = (!header.packed()).then(|| bvh_build::check(&words, header.root));
        found.push(Found {
            address,
            header,
            verdict,
            holes,
        });
    }
    (found, not)
}

/// For each structure, whether its bytes overlap another's.
fn overlapping(found: &[Found]) -> Vec<bool> {
    found
        .iter()
        .enumerate()
        .map(|(i, a)| {
            found
                .iter()
                .enumerate()
                .any(|(j, b)| i != j && a.address < b.end() && b.address < a.end())
        })
        .collect()
}

pub(crate) fn run(folder: &Path, list: bool) -> Result<()> {
    let mut dump = Dump::load(folder)?;
    println!(
        "{} pages in the dump, {} candidate addresses",
        dump.pages.len(),
        dump.candidates.len()
    );
    let (found, not) = survey(&mut dump);
    let overlaps = overlapping(&found);
    let (mut whole, mut packed, mut faults, mut loose, mut orphans, mut holes) = (0, 0, 0, 0, 0, 0);
    for f in &found {
        match &f.verdict {
            None => packed += 1,
            Some(Err(_)) => faults += 1,
            Some(Ok(r)) => {
                loose += usize::from(r.loose > 0);
                orphans += usize::from(r.reached + 1 != r.size);
            }
        }
        whole += usize::from(f.whole());
        holes += usize::from(f.holes > 0);
    }
    println!(
        "{} structures ({not} candidates are not one): {whole} whole, {packed} still packed, \
         {faults} with a fault, {loose} with boxes that do not hold what is under them, \
         {orphans} with nodes nothing reaches, {holes} with pages the dump lacks, \
         {} overlapping another",
        found.len(),
        overlaps.iter().filter(|&&o| o).count()
    );
    if list {
        for (f, over) in found.iter().zip(&overlaps) {
            let what = match &f.verdict {
                None => "packed".to_owned(),
                Some(Err(fault)) => format!("{fault:?}"),
                Some(Ok(r)) => format!(
                    "reached {} of {} (boxes {}+{}, triangles {} in {} nodes, instances {}), \
                     depth {}, loose {}, dead {}",
                    r.reached,
                    r.size,
                    r.box16,
                    r.box32,
                    r.triangles,
                    r.triangle_nodes,
                    r.instances,
                    r.depth,
                    r.loose,
                    r.dead
                ),
            };
            println!(
                "{:#x} {} marks {:#x} nodes {} elements {} holes {}{}: {what}",
                f.address,
                if f.header.bottom() { "bottom" } else { "top" },
                f.header.marks,
                f.header.nodes,
                f.header.elements,
                f.holes,
                if *over { " OVERLAPS" } else { "" },
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_shader::bvh_build::{Options, Triangle};

    fn triangles(n: usize, seed: u32) -> Vec<Triangle> {
        let f = |k: u32| {
            ((k.wrapping_mul(2654435761).wrapping_add(seed * 97) >> 8) % 2000) as f32 / 100.0
        };
        (0..n as u32)
            .map(|i| {
                let c = [f(i * 3), f(i * 3 + 1), f(i * 3 + 2)];
                [c, [c[0] + 1.0, c[1], c[2]], [c[0], c[1] + 1.0, c[2] + 0.5]]
            })
            .collect()
    }

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn a_structure_written_over_another_is_told_by_the_dump() {
        let folder = std::env::temp_dir().join(format!("victory-bvh-check-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let a = bvh_build::build(&triangles(300, 1), Options::default());
        let b = bvh_build::build(&triangles(60, 2), Options::default());
        assert!(a.nodes() * 64 > 0x3000 + b.nodes() * 64, "{}", a.nodes());
        assert!(a.nodes() * 64 < 0x10000, "{}", a.nodes());
        // One arena: `a` from its start, `b` written from 0x3000, over it.
        let mut arena = bytes(&a.words);
        arena.resize(0x10000, 0);
        arena[0x3000..0x3000 + b.nodes() * 64].copy_from_slice(&bytes(&b.words));
        std::fs::write(folder.join("arena-0.bin"), &arena).unwrap();
        std::fs::write(
            folder.join("pages.txt"),
            "walked 0x200000000\nbuilt 0x200003000\nbuilt 0x200009000\nframe 5\n0x200000000 0 0x0\n",
        )
        .unwrap();
        let mut dump = Dump::load(&folder).unwrap();
        let (found, not) = survey(&mut dump);
        assert_eq!(
            (found.len(), not),
            (2, 1),
            "the third candidate holds no header"
        );
        assert!(!found[0].whole(), "the first was written over");
        assert!(found[1].whole(), "{:?}", found[1].verdict);
        assert_eq!(overlapping(&found), [true, true]);
        run(&folder, true).unwrap();
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
