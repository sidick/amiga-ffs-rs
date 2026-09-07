//! Arbitrary bytes as a volume: open it, walk all of it, validate it.
//!
//! The read side's contract is that it *refuses* rather than misbehaves,
//! and this is the only way to test that claim against inputs nobody
//! thought of. Every pointer in the format is an unvalidated 32-bit block
//! number off the disk, every name is a length byte, and every chain can
//! point at itself — so the failure modes worth ruling out are a panic
//! (an index or a slice arithmetic that assumed a field was sane), a hang
//! (a chain walk without a cycle guard) and an out-of-memory (an
//! allocation sized from a number the disk supplied).
//!
//! Three deliberate choices here:
//!
//! - **The image is capped**, at [`MAX_IMAGE`], so a large input does not
//!   turn into a slow one. The bugs live in the block *contents*, not in
//!   the block count.
//! - **Files are read with [`Volume::read_file_with`]**, never
//!   `read_file`. The convenient form allocates `byte_size` bytes, and
//!   `byte_size` is a longword the disk supplies — that is a documented
//!   footgun, not a bug, and a fuzzer that triggered it would be
//!   reporting the documentation.
//! - **The walk keeps its own visited set.** `read_dir` refuses a chain
//!   that revisits a block *within one directory*; a directory whose
//!   child's child is the directory again is a legal-looking cycle across
//!   two of them, and only the walker can see it.

#![no_main]

use libfuzzer_sys::fuzz_target;

mod mem;

use amiga_ffs::{Variant, Volume};

/// The largest image a single run will build. Big enough for a root, a
/// bitmap and a plausible tree; small enough that a chain walk's
/// quadratic visited-set check stays fast.
const MAX_IMAGE: usize = 256 * 1024;

/// The most entries the walk will visit before giving up. A cap, not a
/// correctness claim: the point is to bound the run, and a volume that
/// needs more than this to crash us can be found with a longer one.
const MAX_ENTRIES: usize = 20_000;

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    // The first byte chooses the geometry the caller would have supplied
    // (block size is a mount parameter, not something in the image), and
    // the rest is the medium.
    let bs = match data[0] % 3 {
        0 => 512,
        1 => 1024,
        _ => 4096,
    };
    let expect = Variant::from_dostype(0x444F_5300 | u32::from(data[1] % 8));
    let body = &data[2..];
    let body = &body[..body.len().min(MAX_IMAGE)];
    let blocks = body.len() / bs;
    if blocks < 3 {
        return;
    }

    let disk = mem::MemDisk::new(bs, body[..blocks * bs].to_vec());
    // `None` first, so an image whose boot block carries a real dostype is
    // opened the way a real caller would open it; the forced variant is
    // the fallback that keeps random input reaching the parser at all.
    let mut vol = match Volume::open_with(disk, None, blocks as u64, 2) {
        Ok(v) => v,
        Err(_) => {
            let disk = mem::MemDisk::new(bs, body[..blocks * bs].to_vec());
            match Volume::open_at_root(
                disk,
                expect.unwrap_or(Variant::FfsIntl),
                blocks as u64,
                amiga_ffs::canonical_root_lba(blocks as u64, 2).unwrap_or(0),
            ) {
                Ok(v) => v,
                Err(_) => return,
            }
        }
    };

    let root = vol.root_lba();
    let mut seen: Vec<u64> = vec![root];
    let mut queue: Vec<u64> = vec![root];
    let mut visited = 0usize;

    while let Some(dir) = queue.pop() {
        let entries = match vol.read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries {
            visited += 1;
            if visited > MAX_ENTRIES {
                queue.clear();
                break;
            }
            // Every accessor a consumer would reach for, on an entry the
            // volume may have made up entirely.
            let _ = vol.comment(&entry);
            let _ = vol.resolve_link(&entry);
            match entry.kind {
                amiga_ffs::EntryKind::Directory => {
                    if !seen.contains(&entry.lba) {
                        seen.push(entry.lba);
                        queue.push(entry.lba);
                    }
                    let _ = vol.dircache_head(entry.lba);
                    let _ = vol.read_dircache(entry.lba);
                }
                amiga_ffs::EntryKind::File => {
                    let mut total: u64 = 0;
                    let _ = vol.read_file_with(entry.lba, |chunk| total += chunk.len() as u64);
                    let _ = vol.file_chain(entry.lba);
                }
                amiga_ffs::EntryKind::SoftLink => {
                    let _ = vol.read_softlink(entry.lba);
                }
                _ => {}
            }
            // Looking the entry up by the name it claims must find
            // something or refuse; it must never diverge.
            let _ = vol.lookup(dir, &entry.name);
        }
    }

    let _ = vol.read_dircache(root);
    let _ = vol.read_bitmap();
    // The whole-volume walk, which is the widest surface of all: it reads
    // every reachable block of every kind and is required never to error.
    let report = vol.validate();
    let _ = report.is_clean();
});
