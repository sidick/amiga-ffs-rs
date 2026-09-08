//! Build two ADFs that differ only in block layout, for measuring what
//! layout actually costs a real ROM's FFS.
//!
//! Both volumes are `DOS\3`, both contain one file of identical bytes.
//! In `contiguous.adf` its data blocks are one ascending run; in
//! `fragmented.adf` the same file is threaded through holes left by
//! deleting every other file of a filler set, which is what a volume
//! that has been lived in looks like.
//!
//! `cargo run --example frag-bench -- outdir`
//!
//! Prints each file's run structure, so the experiment can state what it
//! actually compared rather than assuming the builder cooperated.

use amiga_ffs::format::FormatOptions;
use amiga_ffs::mutate::Mutator;
use amiga_ffs::populate::Metadata;
use amiga_ffs::{BlockSink, BlockSource, Variant, Volume};

const BLOCKS: u64 = 1760; // DD floppy
const PAYLOAD: usize = 200_000; // big enough for the difference to show

struct MemDisk(Vec<u8>);

impl BlockSource for MemDisk {
    type Error = std::convert::Infallible;
    fn block_size(&self) -> usize {
        512
    }
    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        let o = lba as usize * 512;
        buf.copy_from_slice(&self.0[o..o + 512]);
        Ok(())
    }
    fn block_count(&self) -> Option<u64> {
        Some((self.0.len() / 512) as u64)
    }
}
impl BlockSink for MemDisk {
    type Error = std::convert::Infallible;
    fn block_size(&self) -> usize {
        512
    }
    fn write_block(&mut self, lba: u64, buf: &[u8]) -> Result<(), Self::Error> {
        let o = lba as usize * 512;
        self.0[o..o + 512].copy_from_slice(buf);
        Ok(())
    }
    fn block_count(&self) -> Option<u64> {
        Some((self.0.len() / 512) as u64)
    }
}

fn payload() -> Vec<u8> {
    // Content is irrelevant to layout but must be identical in both
    // images, so the only variable is where the blocks sit.
    (0..PAYLOAD).map(|i| (i % 251) as u8).collect()
}

fn blank() -> MemDisk {
    MemDisk(vec![0u8; (BLOCKS * 512) as usize])
}

/// Describe a file's data-block layout: how many ascending runs it takes,
/// and how far the head would travel reading it in order.
fn describe(disk: MemDisk, name: &[u8]) -> (MemDisk, String) {
    let mut vol = Volume::open(disk, None).expect("open");
    let root = vol.root_lba();
    let entry = vol.lookup(root, name).expect("lookup").expect("present");
    let chain = vol.file_chain(entry.lba).expect("chain");
    let blocks = &chain.blocks;

    let mut runs = 1usize;
    let mut travel = 0u64;
    for w in blocks.windows(2) {
        if w[1] != w[0] + 1 {
            runs += 1;
        }
        travel += (w[1] as i64 - w[0] as i64).unsigned_abs();
    }
    let desc = format!(
        "{} blocks, {} run(s), first {}, last {}, total head travel {} blocks",
        blocks.len(),
        runs,
        blocks.first().copied().unwrap_or(0),
        blocks.last().copied().unwrap_or(0),
        travel
    );
    assert!(vol.validate().findings.is_empty(), "volume must be clean");
    (vol.into_inner(), desc)
}

fn build_contiguous() -> MemDisk {
    let mut disk = blank();
    let opts = FormatOptions::new(Variant::FfsIntl, BLOCKS, b"Contig");
    amiga_ffs::format(&mut disk, &opts).expect("format");
    let vol = Volume::open(disk, None).expect("open");
    let root = vol.root_lba();
    let mut m = Mutator::open(vol).expect("mutator");
    m.create_file(root, b"Payload", &Metadata::new(), &payload())
        .expect("create");
    m.into_volume().into_inner()
}

fn build_fragmented() -> MemDisk {
    let mut disk = blank();
    let opts = FormatOptions::new(Variant::FfsIntl, BLOCKS, b"Frag");
    amiga_ffs::format(&mut disk, &opts).expect("format");
    let vol = Volume::open(disk, None).expect("open");
    let root = vol.root_lba();
    let mut m = Mutator::open(vol).expect("mutator");

    // Lay down a filler set, then punch every other one out. The holes
    // are what the payload will be threaded through: this is wear, not
    // sabotage — it is the shape a volume takes after months of use.
    let filler = vec![0u8; 2_048];
    let mut names = Vec::new();
    for i in 0..180 {
        let name = format!("F{i:03}");
        m.create_file(root, name.as_bytes(), &Metadata::new(), &filler)
            .expect("filler");
        names.push(name);
    }
    for (i, name) in names.iter().enumerate() {
        if i % 2 == 0 {
            m.delete(root, name.as_bytes()).expect("delete");
        }
    }
    m.create_file(root, b"Payload", &Metadata::new(), &payload())
        .expect("create");
    m.into_volume().into_inner()
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: frag-bench <outdir>");
    std::fs::create_dir_all(&dir).expect("mkdir");

    for (label, disk) in [
        ("contiguous", build_contiguous()),
        ("fragmented", build_fragmented()),
    ] {
        let (disk, desc) = describe(disk, b"Payload");
        println!("{label}: {desc}");
        std::fs::write(format!("{dir}/{label}.adf"), &disk.0).expect("write");
    }
    println!("payload is {PAYLOAD} bytes in both images");
}
