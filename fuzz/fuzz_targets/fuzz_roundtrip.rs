//! The writer's side of the same question: **any tree the writer accepts
//! must read back identical**.
//!
//! `tests/volumes.rs` asks this of trees a seeded generator produces,
//! which are the trees somebody thought to generate. This asks it of
//! trees the fuzzer produces, which are not — names at every length with
//! every byte the writer permits, files at every size around a block and
//! a table boundary, directories nested until the input runs out.
//!
//! The fuzz input is read as a little program: a byte of opcode, then its
//! operands. Names come out of the input raw and are then *sanitised*
//! rather than rejected — a byte the format forbids becomes `x` — so the
//! fuzzer spends its budget on lengths and collisions rather than on
//! rediscovering that `/` is refused, which `tests/volumes.rs` already
//! asserts directly.
//!
//! Two errors are expected and skipped, because they are the writer
//! working: [`PopulateError::DuplicateName`] (the fuzzer will generate
//! collisions) and [`PopulateError::VolumeFull`] (it will generate more
//! data than the image holds). **Every other error is a failure**: they
//! are all statements about the caller's input, and the caller's input
//! here has already been made legal.

#![no_main]

use libfuzzer_sys::fuzz_target;

mod mem;

use amiga_ffs::populate::{PopulateError, Populator};
use amiga_ffs::{EntryKind, FormatOptions, Metadata, Variant, Volume, MAX_NAME_CLASSIC, MAX_NAME_LONG};

/// Blocks in the image. 2 MB at 512 bytes — enough for a tree with files
/// past one header's pointer table, small enough to build thousands of
/// times a second.
const BLOCKS: u64 = 4096;
const MAX_ENTRIES: usize = 400;
/// Total file bytes one run will write. The image is the real limit;
/// this keeps a run fast rather than merely finite.
const MAX_BYTES: usize = 400_000;

const ALL_VARIANTS: [Variant; 8] = [
    Variant::Ofs,
    Variant::Ffs,
    Variant::OfsIntl,
    Variant::FfsIntl,
    Variant::OfsIntlDircache,
    Variant::FfsIntlDircache,
    Variant::OfsIntlLongname,
    Variant::FfsIntlLongname,
];

/// What was asked for, kept so it can be compared with what came back.
enum Node {
    Dir(Vec<u8>, Vec<Node>),
    File(Vec<u8>, u8, usize),
}

/// A cursor over the fuzz input.
struct Input<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Input<'a> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.data.get(self.at)?;
        self.at += 1;
        Some(b)
    }

    fn bytes(&mut self, n: usize) -> &'a [u8] {
        let end = (self.at + n).min(self.data.len());
        let out = &self.data[self.at.min(end)..end];
        self.at = end;
        out
    }
}

/// The deterministic contents of a file, from one seed byte: a pattern
/// with no short period, so a chain read out of order or off by a block
/// cannot round-trip.
fn contents(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(7) ^ (i >> 8) ^ seed as usize) as u8)
        .collect()
}

/// A name the format will accept, out of whatever bytes the fuzzer had.
fn sanitise(raw: &[u8], max: usize) -> Vec<u8> {
    let mut name: Vec<u8> = raw
        .iter()
        .take(max)
        .map(|&b| match b {
            b':' | b'/' => b'x',
            0x00..=0x20 => b'_',
            0x7F..=0x9F => b'y',
            other => other,
        })
        .collect();
    if name.is_empty() {
        name.push(b'a');
    }
    name
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let variant = ALL_VARIANTS[(data[0] % 8) as usize];
    let bs = match data[1] % 3 {
        0 => 512,
        1 => 1024,
        _ => 4096,
    };
    let max_name = if variant.has_long_names() {
        MAX_NAME_LONG
    } else {
        MAX_NAME_CLASSIC
    };
    let mut input = Input {
        data: &data[2..],
        at: 0,
    };

    let disk = mem::MemDisk::blank(bs, BLOCKS);
    let opts = FormatOptions::new(variant, BLOCKS, b"Fuzzed");
    let mut pop = match Populator::new(disk, &opts) {
        Ok(p) => p,
        Err(e) => panic!("format failed on a blank image: {e:?}"),
    };
    let root = pop.root_lba();

    // The model tree, and the stack of open directories it is built
    // through. `path` mirrors `stack` so a node can be attached to the
    // right parent when it closes.
    let mut tree: Vec<Node> = Vec::new();
    let mut stack: Vec<(u64, Vec<u8>, Vec<Node>)> = Vec::new();
    let mut entries = 0usize;
    let mut bytes = 0usize;

    while let Some(op) = input.byte() {
        if entries >= MAX_ENTRIES {
            break;
        }
        let here = stack.last().map(|(lba, _, _)| *lba).unwrap_or(root);
        match op % 4 {
            // Open a directory.
            0 | 1 => {
                let n = input.byte().unwrap_or(1) as usize % max_name + 1;
                let name = sanitise(input.bytes(n), max_name);
                match pop.create_dir(here, &name, &Metadata::new()) {
                    Ok(lba) => {
                        entries += 1;
                        stack.push((lba, name, Vec::new()));
                    }
                    Err(PopulateError::DuplicateName { .. })
                    | Err(PopulateError::VolumeFull { .. }) => {}
                    Err(e) => panic!("create_dir({name:?}): {e:?}"),
                }
            }
            // Close one.
            2 => {
                if let Some((_, name, children)) = stack.pop() {
                    let into = match stack.last_mut() {
                        Some((_, _, siblings)) => siblings,
                        None => &mut tree,
                    };
                    into.push(Node::Dir(name, children));
                }
            }
            // A file.
            _ => {
                let n = input.byte().unwrap_or(1) as usize % max_name + 1;
                let name = sanitise(input.bytes(n), max_name);
                // Sizes clustered around the boundaries that matter: a
                // block, a full pointer table, and zero.
                let hi = input.byte().unwrap_or(0) as usize;
                let lo = input.byte().unwrap_or(0) as usize;
                let len = ((hi << 8 | lo) * 3).min(MAX_BYTES.saturating_sub(bytes));
                let seed = input.byte().unwrap_or(0);
                match pop.create_file(here, &name, &Metadata::new(), &contents(seed, len)) {
                    Ok(_) => {
                        entries += 1;
                        bytes += len;
                        let into = match stack.last_mut() {
                            Some((_, _, siblings)) => siblings,
                            None => &mut tree,
                        };
                        into.push(Node::File(name, seed, len));
                    }
                    Err(PopulateError::DuplicateName { .. })
                    | Err(PopulateError::VolumeFull { .. }) => {}
                    Err(e) => panic!("create_file({name:?}, {len}): {e:?}"),
                }
            }
        }
    }
    // Close whatever is still open.
    while let Some((_, name, children)) = stack.pop() {
        let into = match stack.last_mut() {
            Some((_, _, siblings)) => siblings,
            None => &mut tree,
        };
        into.push(Node::Dir(name, children));
    }

    let disk = pop.finish().expect("finish");
    let mut vol = Volume::open_with(disk, None, BLOCKS, 2).expect("the volume we just wrote");
    check(&mut vol, root, &tree);

    let report = vol.validate();
    assert!(
        report.is_clean(),
        "{variant:?} @{bs}: {:?}",
        report
            .findings
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
    );
});

fn check(vol: &mut Volume<mem::MemDisk>, dir: u64, nodes: &[Node]) {
    let listed = vol.read_dir(dir).expect("read_dir");
    assert_eq!(listed.len(), nodes.len(), "entry count in directory {dir}");
    for node in nodes {
        let (name, want_dir) = match node {
            Node::Dir(name, _) => (name, true),
            Node::File(name, _, _) => (name, false),
        };
        let entry = vol
            .lookup(dir, name)
            .expect("lookup")
            .unwrap_or_else(|| panic!("{name:?} was written and cannot be found"));
        // Byte for byte: the disk preserves case, and a writer that
        // folded what it stored would still pass every lookup.
        assert_eq!(&entry.name, name, "name not preserved");
        assert_eq!(entry.parent as u64, dir);
        match node {
            Node::Dir(_, children) => {
                assert_eq!(entry.kind, EntryKind::Directory);
                check(vol, entry.lba, children);
            }
            Node::File(_, seed, len) => {
                assert_eq!(entry.kind, EntryKind::File);
                assert_eq!(entry.byte_size as usize, *len, "size of {name:?}");
                assert_eq!(
                    vol.read_file(entry.lba).expect("read_file"),
                    contents(*seed, *len),
                    "contents of {name:?}"
                );
            }
        }
        assert!(want_dir == entry.kind.is_directory());
    }
}
