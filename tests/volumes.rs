//! Synthetic volumes, built byte by byte, read back through the crate.
//!
//! The builder here is deliberately *not* the crate's own writer (there
//! isn't one yet): it lays out blocks from the documented offsets and
//! hashes names with the crate's `name_hash`, so a test that passes says
//! the reader agrees with the format, not merely with itself. The one
//! thing it shares with the reader is the hash function — and that is
//! the point, since a directory chained by a different hash than the one
//! used to search it is exactly the bug these tests are watching for.

use amiga_ffs::layout::*;
use amiga_ffs::*;

// ---------------------------------------------------------------------------
// A Vec-backed block source
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemError {
    /// The caller sized its buffer for a different block size.
    BadBufferLen { got: usize, want: usize },
    /// Read past the end of the image.
    OutOfRange { lba: u64, blocks: u64 },
}

impl std::fmt::Display for MemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadBufferLen { got, want } => write!(f, "buffer of {got} bytes, want {want}"),
            Self::OutOfRange { lba, blocks } => write!(f, "lba {lba} of {blocks}"),
        }
    }
}

impl std::error::Error for MemError {}

pub struct MemDisk {
    bs: usize,
    data: Vec<u8>,
}

impl BlockSource for MemDisk {
    type Error = MemError;

    fn block_size(&self) -> usize {
        self.bs
    }

    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), MemError> {
        if buf.len() != self.bs {
            return Err(MemError::BadBufferLen {
                got: buf.len(),
                want: self.bs,
            });
        }
        let blocks = self.data.len() as u64 / self.bs as u64;
        if lba >= blocks {
            return Err(MemError::OutOfRange { lba, blocks });
        }
        let off = lba as usize * self.bs;
        buf.copy_from_slice(&self.data[off..off + self.bs]);
        Ok(())
    }

    fn block_count(&self) -> Option<u64> {
        Some(self.data.len() as u64 / self.bs as u64)
    }
}

// ---------------------------------------------------------------------------
// The builder
// ---------------------------------------------------------------------------

const RESERVED: u64 = 2;

pub struct Builder {
    bs: usize,
    nblocks: u64,
    variant: Variant,
    data: Vec<u8>,
    root_lba: u64,
    next_free: u64,
    touched: Vec<u64>,
}

fn wr32(data: &mut [u8], off: usize, v: u32) {
    data[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

fn wr_bcpl(data: &mut [u8], off: usize, s: &[u8]) {
    data[off] = s.len() as u8;
    data[off + 1..off + 1 + s.len()].copy_from_slice(s);
}

impl Builder {
    pub fn new(variant: Variant, bs: usize, nblocks: u64, volume_name: &[u8]) -> Self {
        let root_lba = canonical_root_lba(nblocks, RESERVED).unwrap();
        let mut b = Builder {
            bs,
            nblocks,
            variant,
            data: vec![0u8; bs * nblocks as usize],
            root_lba,
            next_free: root_lba + 1,
            touched: vec![],
        };

        // Boot block: the dostype is all this crate reads from it.
        let dostype = variant.dostype();
        wr32(b.block_mut(0), 0, dostype);

        // Root block.
        let (bs_, long) = (bs, variant.has_long_names());
        let root = b.block_mut(root_lba);
        wr32(root, OFF_TYPE, T_HEADER);
        wr32(root, OFF_HASH_TABLE_SIZE, hash_table_size(bs_));
        wr32(root, tail(bs_, TL_BITMAP_FLAG), 0xFFFF_FFFF); // -1: valid
        wr32(root, tail(bs_, TL_BITMAP_PAGES), 1); // one bitmap page, block 1
        wr32(root, tail(bs_, TL_ROOT_DIR_ALTERED), 1000);
        wr32(root, tail(bs_, TL_ROOT_DIR_ALTERED) + 4, 60);
        wr32(root, tail(bs_, TL_ROOT_DIR_ALTERED) + 8, 25);
        wr_bcpl(root, tail(bs_, TL_ROOT_NAME), volume_name);
        wr32(root, tail(bs_, TL_ROOT_DISK_MADE), 900);
        wr32(root, tail(bs_, TL_SECONDARY_TYPE), ST_ROOT as u32);
        if long {
            wr32(root, tail(bs_, TL_ROOT_FS_TYPE), dostype);
            wr32(root, tail(bs_, TL_ROOT_NUM_BLOCKS_USED), 4);
        }
        b.touched.push(root_lba);
        b
    }

    fn block_mut(&mut self, lba: u64) -> &mut [u8] {
        let off = lba as usize * self.bs;
        &mut self.data[off..off + self.bs]
    }

    fn block(&self, lba: u64) -> &[u8] {
        let off = lba as usize * self.bs;
        &self.data[off..off + self.bs]
    }

    pub fn root_lba(&self) -> u64 {
        self.root_lba
    }

    /// Add an entry to the directory at `dir_lba`, chaining it into the
    /// right hash slot by the volume's own hash function.
    pub fn add(
        &mut self,
        dir_lba: u64,
        name: &[u8],
        comment: &[u8],
        kind: EntryKind,
        byte_size: u32,
    ) -> u64 {
        let lba = self.next_free;
        self.next_free += 1;
        assert!(lba < self.nblocks, "test volume too small");

        let (bs, long) = (self.bs, self.variant.has_long_names());
        let blk = self.block_mut(lba);
        wr32(blk, OFF_TYPE, T_HEADER);
        wr32(blk, OFF_OWN_KEY, lba as u32);
        wr32(
            blk,
            tail(bs, TL_SECONDARY_TYPE),
            kind.secondary_type() as u32,
        );
        wr32(blk, tail(bs, TL_PARENT), dir_lba as u32);
        wr32(blk, tail(bs, TL_PROTECTION), 0x0000_FFF0 | 0x5);
        wr32(blk, tail(bs, TL_OWNER), 0x0007_0042); // uid 7, gid 0x42
        if matches!(kind, EntryKind::File | EntryKind::LinkFile) {
            wr32(blk, tail(bs, TL_BYTE_SIZE), byte_size);
        }
        if long {
            // NaC: name then comment, two BCPL strings end to end.
            let off = tail(bs, TL_NAC);
            wr_bcpl(blk, off, name);
            let after = off + 1 + name.len();
            if after + 1 + comment.len() <= off + NAC_LEN {
                wr_bcpl(blk, after, comment);
            }
            wr32(blk, tail(bs, TL_DATE_LONG), 1234);
            wr32(blk, tail(bs, TL_DATE_LONG) + 4, 56);
            wr32(blk, tail(bs, TL_DATE_LONG) + 8, 7);
        } else {
            wr_bcpl(blk, tail(bs, TL_NAME), name);
            wr_bcpl(blk, tail(bs, TL_COMMENT), comment);
            wr32(blk, tail(bs, TL_DATE), 1234);
            wr32(blk, tail(bs, TL_DATE) + 4, 56);
            wr32(blk, tail(bs, TL_DATE) + 8, 7);
        }
        self.touched.push(lba);

        // Chain it in: head of the slot if empty, tail of the chain if not.
        let slot = name_hash(name, self.variant.fold(), hash_table_size(bs)) as usize;
        let slot_off = OFF_HASH_TABLE + slot * 4;
        let head = be32(self.block(dir_lba), slot_off);
        if head == 0 {
            wr32(self.block_mut(dir_lba), slot_off, lba as u32);
        } else {
            let mut cur = head as u64;
            loop {
                let next = be32(self.block(cur), tail(bs, TL_HASH_CHAIN));
                if next == 0 {
                    break;
                }
                cur = next as u64;
            }
            wr32(self.block_mut(cur), tail(bs, TL_HASH_CHAIN), lba as u32);
        }
        lba
    }

    /// Overwrite an arbitrary longword — for building damaged volumes.
    /// Checksums are still fixed up afterwards, so what the reader
    /// refuses is the *structure*, not a stale checksum.
    pub fn poke(&mut self, lba: u64, off: usize, v: u32) {
        wr32(self.block_mut(lba), off, v);
    }

    pub fn finish(mut self) -> MemDisk {
        for i in 0..self.touched.len() {
            let lba = self.touched[i];
            let ck = checksum_compute(self.block(lba), CHECKSUM_INDEX);
            wr32(self.block_mut(lba), OFF_CHECKSUM, ck);
        }
        MemDisk {
            bs: self.bs,
            data: self.data,
        }
    }

    pub fn finish_without_checksums(self) -> MemDisk {
        MemDisk {
            bs: self.bs,
            data: self.data,
        }
    }
}

fn names(entries: &[Entry]) -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = entries.iter().map(|e| e.name.clone()).collect();
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// Root location
// ---------------------------------------------------------------------------

#[test]
fn canonical_root_is_the_number_every_adf_tool_hardcodes() {
    // A DD floppy: 80 cylinders x 2 heads x 11 sectors = 1760 blocks.
    assert_eq!(canonical_root_lba(1760, 2), Some(880));
    // An HD floppy: twice the blocks, and the midpoint moves with them.
    assert_eq!(canonical_root_lba(3520, 2), Some(1760));
    // Reserved blocks shift the usable range, not just its start.
    assert_eq!(canonical_root_lba(1760, 4), Some(881));
    // Too small to hold one.
    assert_eq!(canonical_root_lba(2, 2), None);
    assert_eq!(canonical_root_lba(0, 2), None);
}

// ---------------------------------------------------------------------------
// Classic variants
// ---------------------------------------------------------------------------

#[test]
fn dos1_classic_root_and_traversal() {
    let mut b = Builder::new(Variant::Ffs, 512, 1760, b"Workbench");
    let root = b.root_lba();
    b.add(root, b"C", b"", EntryKind::Directory, 0);
    b.add(
        root,
        b"Startup-Sequence",
        b"the boot script",
        EntryKind::File,
        4711,
    );
    let dir_c = b.add(root, b"Devs", b"", EntryKind::Directory, 0);
    b.add(dir_c, b"system-configuration", b"", EntryKind::File, 232);
    let disk = b.finish();

    let mut vol = Volume::open(disk, Some(Variant::Ffs)).unwrap();
    assert_eq!(vol.root_lba(), 880);
    assert_eq!(vol.root().name, b"Workbench");
    assert_eq!(vol.root().bitmap_flag, -1);
    assert_eq!(vol.root().bitmap_pages[0], 1);
    assert_eq!(vol.root().bitmap_ext, 0);
    assert_eq!(
        vol.root().dir_altered,
        DateStamp {
            days: 1000,
            mins: 60,
            ticks: 25
        }
    );
    assert_eq!(vol.root().disk_made.days, 900);
    // Classic roots leave the LNFS-only fields alone.
    assert_eq!(vol.root().fs_type, None);
    assert_eq!(vol.root().blocks_used, None);

    let listing = vol.read_dir(root).unwrap();
    assert_eq!(
        names(&listing),
        vec![
            b"C".to_vec(),
            b"Devs".to_vec(),
            b"Startup-Sequence".to_vec()
        ]
    );

    // Lookup is case-insensitive, and metadata survives the round trip.
    let s = vol.lookup(root, b"startup-SEQUENCE").unwrap().unwrap();
    assert_eq!(s.name, b"Startup-Sequence");
    assert_eq!(s.kind, EntryKind::File);
    assert_eq!(s.byte_size, 4711);
    assert_eq!(s.comment, b"the boot script");
    assert_eq!(s.protection, 0x0000_FFF5);
    assert_eq!(s.owner, 0x0007_0042);
    assert_eq!(s.parent as u64, root);
    assert_eq!(s.own_key as u64, s.lba);
    assert_eq!(
        s.date,
        DateStamp {
            days: 1234,
            mins: 56,
            ticks: 7
        }
    );

    // A directory has no byte size, whatever longword -47 holds.
    let d = vol.lookup(root, b"devs").unwrap().unwrap();
    assert_eq!(d.kind, EntryKind::Directory);
    assert_eq!(d.byte_size, 0);

    // Subdirectory traversal, and a path.
    let sub = vol.read_dir(d.lba).unwrap();
    assert_eq!(names(&sub), vec![b"system-configuration".to_vec()]);
    let by_path = vol.lookup_path(root, b"Devs/system-configuration").unwrap();
    assert_eq!(by_path.unwrap().byte_size, 232);
    assert!(vol.lookup_path(root, b"Devs/nope").unwrap().is_none());
    assert!(vol.lookup(root, b"NoSuchFile").unwrap().is_none());
}

#[test]
fn dos1_does_not_fold_latin1_but_dos3_does() {
    // The same name, the same volume shape, two fold tables.
    for (variant, folds) in [(Variant::Ffs, false), (Variant::FfsIntl, true)] {
        let mut b = Builder::new(variant, 512, 512, b"Cafe");
        let root = b.root_lba();
        b.add(root, b"caf\xE9", b"", EntryKind::File, 1);
        let mut vol = Volume::open(b.finish(), Some(variant)).unwrap();

        // Exact match works on both.
        assert!(vol.lookup(root, b"caf\xE9").unwrap().is_some());
        // The upper-case Latin-1 form is the same name only under intl
        // folding — and on DOS\1 it does not even hash to the same slot,
        // which is why this has to be a lookup test and not a compare.
        assert_eq!(
            vol.lookup(root, b"CAF\xC9").unwrap().is_some(),
            folds,
            "{variant:?}"
        );
    }
}

#[test]
fn hash_collisions_walk_the_chain() {
    // Find three names that land in one slot at 512 bytes, then check
    // every one of them is still findable through the chain.
    let slots = hash_table_size(512);
    let mut buckets: std::collections::HashMap<u32, Vec<Vec<u8>>> = Default::default();
    let mut colliding = None;
    for i in 0..5000u32 {
        let name = format!("file{i}").into_bytes();
        let h = name_hash(&name, Variant::Ffs.fold(), slots);
        let e = buckets.entry(h).or_default();
        e.push(name);
        if e.len() == 3 {
            colliding = Some((h, e.clone()));
            break;
        }
    }
    let (slot, group) = colliding.expect("three names must collide within 5000");

    let mut b = Builder::new(Variant::Ffs, 512, 512, b"Collide");
    let root = b.root_lba();
    for n in &group {
        b.add(root, n, b"", EntryKind::File, n.len() as u32);
    }
    let mut vol = Volume::open(b.finish(), None).unwrap();

    // They really are one chain: one non-empty slot, three entries.
    assert_eq!(
        vol.root().hash_table.iter().filter(|&&s| s != 0).count(),
        1,
        "expected a single occupied slot ({slot})"
    );
    assert_eq!(vol.read_dir(root).unwrap().len(), 3);
    for n in &group {
        let e = vol.lookup(root, n).unwrap().expect("chained entry");
        assert_eq!(&e.name, n);
        assert_eq!(e.byte_size, n.len() as u32);
    }
}

// ---------------------------------------------------------------------------
// Long filenames — the regression this crate exists to not have
// ---------------------------------------------------------------------------

#[test]
fn dos7_every_name_is_non_empty() {
    let long40 = b"a-directory-name-of-forty-characters-abc".to_vec();
    assert_eq!(long40.len(), 40);
    let long107: Vec<u8> = (0..MAX_NAME_LONG)
        .map(|i| b"abcdefghijklmnopqrstuvwxyz0123456789"[i % 36])
        .collect();
    assert_eq!(long107.len(), 107);

    let mut b = Builder::new(Variant::FfsIntlLongname, 512, 1024, b"LongVolume");
    let root = b.root_lba();
    b.add(root, b"S", b"short and sweet", EntryKind::File, 10);
    b.add(root, &long40, b"a comment too", EntryKind::Directory, 0);
    b.add(root, &long107, b"", EntryKind::File, 99);
    b.add(root, b"caf\xE9-r\xE9sum\xE9", b"", EntryKind::File, 3);
    let disk = b.finish();

    let mut vol = Volume::open(disk, Some(Variant::FfsIntlLongname)).unwrap();
    assert_eq!(vol.variant(), Variant::FfsIntlLongname);
    assert_eq!(vol.max_name_len(), 107);
    // The LNFS root repeats the dostype; the volume name stays classic.
    assert_eq!(vol.root().fs_type, Some(0x444F_5307));
    assert_eq!(vol.root().name, b"LongVolume");

    let listing = vol.read_dir(root).unwrap();
    assert_eq!(listing.len(), 4);
    // The assertion, stated plainly: not one empty name.
    for e in &listing {
        assert!(!e.name.is_empty(), "empty name at block {}", e.lba);
    }
    assert_eq!(names(&listing), {
        let mut want = vec![
            b"S".to_vec(),
            long40.clone(),
            long107.clone(),
            b"caf\xE9-r\xE9sum\xE9".to_vec(),
        ];
        want.sort();
        want
    });

    // Names past the classic 30-byte limit round-trip, both ways.
    let e = vol.lookup(root, &long107).unwrap().unwrap();
    assert_eq!(e.name, long107);
    assert_eq!(e.byte_size, 99);
    let e = vol.lookup(root, &long40).unwrap().unwrap();
    assert_eq!(e.name, long40);
    assert_eq!(e.kind, EntryKind::Directory);
    // The comment shares the field with the name and still comes back.
    assert_eq!(e.comment, b"a comment too");
    assert_eq!(e.comment_block, 0);
    assert_eq!(
        e.date,
        DateStamp {
            days: 1234,
            mins: 56,
            ticks: 7
        }
    );
    // Intl folding applies on DOS\7 as on DOS\3.
    assert!(vol.lookup(root, b"CAF\xC9-R\xC9SUM\xC9").unwrap().is_some());

    // And a name longer than the variant allows is refused, not truncated.
    let too_long = vec![b'x'; 108];
    assert!(matches!(
        vol.lookup(root, &too_long),
        Err(Error::NameTooLong { len: 108, max: 107 })
    ));
}

#[test]
fn dos7_classic_offset_would_have_read_an_empty_name() {
    // This is the failure mode, reproduced deliberately: a reader that
    // looks for a BCPL name at longword -20 on an LNFS volume lands 104
    // bytes into the merged NaC field, which for any ordinary name is
    // padding. Length byte zero, name empty, directory apparently gone.
    let mut b = Builder::new(Variant::FfsIntlLongname, 512, 512, b"Trap");
    let root = b.root_lba();
    let lba = b.add(root, b"Startup-Sequence", b"", EntryKind::File, 1);
    let mut disk = b.finish();

    let mut raw = vec![0u8; 512];
    disk.read_block(lba, &mut raw).unwrap();
    assert_eq!(
        raw[tail(512, TL_NAME)],
        0,
        "the classic name offset is padding"
    );
    // The merged field, meanwhile, has it at the very front.
    assert_eq!(raw[tail(512, TL_NAC)], 16);

    // The reader gets it right because it uses the layout, not the habit.
    let mut vol = Volume::open(disk, None).unwrap();
    assert_eq!(
        vol.lookup(root, b"Startup-Sequence").unwrap().unwrap().name,
        b"Startup-Sequence"
    );
}

// ---------------------------------------------------------------------------
// Block sizes other than 512
// ---------------------------------------------------------------------------

#[test]
fn tail_offsets_scale_with_block_size() {
    for bs in [512usize, 1024, 4096] {
        for variant in [Variant::FfsIntl, Variant::FfsIntlLongname] {
            let mut b = Builder::new(variant, bs, 400, b"Scaled");
            let root = b.root_lba();
            let dir = b.add(root, b"Utilities", b"tools", EntryKind::Directory, 0);
            b.add(dir, b"More", b"", EntryKind::Directory, 0);
            b.add(root, b"README.info", b"", EntryKind::File, 1234);
            let mut vol = Volume::open(b.finish(), Some(variant)).unwrap();

            assert_eq!(vol.block_size(), bs);
            assert_eq!(vol.root().name, b"Scaled", "bs {bs} {variant:?}");
            assert_eq!(vol.root().hash_table.len(), hash_table_size(bs) as usize);
            let listing = vol.read_dir(root).unwrap();
            assert_eq!(
                names(&listing),
                vec![b"README.info".to_vec(), b"Utilities".to_vec()]
            );
            let u = vol.lookup(root, b"utilities").unwrap().unwrap();
            assert_eq!(u.comment, b"tools");
            assert_eq!(names(&vol.read_dir(u.lba).unwrap()), vec![b"More".to_vec()]);
            let r = vol.lookup(root, b"README.INFO").unwrap().unwrap();
            assert_eq!(r.byte_size, 1234);
        }
    }
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[test]
fn a_chain_pointing_at_itself_errors_rather_than_hangs() {
    let mut b = Builder::new(Variant::Ffs, 512, 512, b"Damaged");
    let root = b.root_lba();
    let lba = b.add(root, b"Loop", b"", EntryKind::File, 0);
    // Point the entry's hash chain at itself.
    b.poke(lba, tail(512, TL_HASH_CHAIN), lba as u32);
    let mut vol = Volume::open(b.finish(), None).unwrap();

    // Enumeration must not loop.
    assert!(matches!(vol.read_dir(root), Err(Error::ChainCycle { .. })));
    // Nor a lookup that has to walk past the looping entry.
    assert!(matches!(
        vol.lookup(root, b"Nothing"),
        Err(Error::ChainCycle { .. }) | Ok(None)
    ));
    // A lookup that walks *into* the loop definitely does not hang.
    let mut b2 = Builder::new(Variant::Ffs, 512, 512, b"Damaged");
    let root2 = b2.root_lba();
    let a = b2.add(root2, b"Loop", b"", EntryKind::File, 0);
    b2.poke(a, tail(512, TL_HASH_CHAIN), a as u32);
    let mut vol2 = Volume::open(b2.finish(), None).unwrap();
    let miss = format!("Loop{}", "x".repeat(3)).into_bytes();
    let _ = vol2.lookup(root2, &miss); // must return, whatever it returns
}

#[test]
fn a_pointer_outside_the_volume_is_refused() {
    let mut b = Builder::new(Variant::Ffs, 512, 512, b"Wild");
    let root = b.root_lba();
    let lba = b.add(root, b"Wild", b"", EntryKind::File, 0);
    b.poke(lba, tail(512, TL_HASH_CHAIN), 100_000);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert!(matches!(
        vol.read_dir(root),
        Err(Error::LbaOutOfRange { lba: 100_000, .. })
    ));
}

#[test]
fn an_unknown_secondary_type_is_refused_not_guessed() {
    let mut b = Builder::new(Variant::Ffs, 512, 512, b"Strange");
    let root = b.root_lba();
    let lba = b.add(root, b"Pipe", b"", EntryKind::File, 0);
    b.poke(lba, tail(512, TL_SECONDARY_TYPE), (-5i32) as u32); // ST_PIPEFILE
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert!(matches!(
        vol.read_dir(root),
        Err(Error::UnknownSecondaryType { found: -5, .. })
    ));
}

#[test]
fn link_types_are_classified_even_though_they_are_not_resolved() {
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Links");
    let root = b.root_lba();
    b.add(root, b"soft", b"", EntryKind::SoftLink, 0);
    b.add(root, b"harddir", b"", EntryKind::LinkDir, 0);
    b.add(root, b"hardfile", b"", EntryKind::LinkFile, 77);
    let mut vol = Volume::open(b.finish(), None).unwrap();

    let mut kinds: Vec<(Vec<u8>, EntryKind)> = vol
        .read_dir(root)
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.kind))
        .collect();
    kinds.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        kinds,
        vec![
            (b"harddir".to_vec(), EntryKind::LinkDir),
            (b"hardfile".to_vec(), EntryKind::LinkFile),
            (b"soft".to_vec(), EntryKind::SoftLink),
        ]
    );
    // A hard link to a directory has no hash table of its own.
    let hd = vol.lookup(root, b"harddir").unwrap().unwrap();
    assert!(matches!(
        vol.read_dir(hd.lba),
        Err(Error::NotADirectory { found: 4, .. })
    ));
}

#[test]
fn a_dostype_mismatch_is_an_error_not_a_preference() {
    // The exact shape of the shipped bug: a DOS\7 volume opened as
    // classic FFS. Refused, rather than read with empty names.
    let b = Builder::new(Variant::FfsIntlLongname, 512, 512, b"Seven");
    let disk = b.finish();
    assert!(matches!(
        Volume::open(disk, Some(Variant::Ffs)),
        Err(Error::VariantMismatch {
            found: Variant::FfsIntlLongname,
            expected: Variant::Ffs,
            source: DostypeSource::BootBlock,
        })
    ));

    // A boot block with no recognisable dostype and no expectation.
    let mut b = Builder::new(Variant::Ffs, 512, 512, b"Blank");
    b.poke(0, 0, 0);
    assert!(matches!(
        Volume::open(b.finish(), None),
        Err(Error::UnknownDosType(0))
    ));

    // ...but with an expectation, the caller's dostype carries it. This
    // is the ordinary hard-disk case: the RDB knows, the boot block is
    // blank.
    let mut b = Builder::new(Variant::Ffs, 512, 512, b"Blank");
    b.poke(0, 0, 0);
    let vol = Volume::open(b.finish(), Some(Variant::Ffs)).unwrap();
    assert_eq!(vol.root().name, b"Blank");
}

#[test]
fn the_lnfs_root_dostype_is_cross_checked_too() {
    // Boot block says DOS\7, the root's own FileSystemType says DOS\6.
    let mut b = Builder::new(Variant::FfsIntlLongname, 512, 512, b"Split");
    let root = b.root_lba();
    b.poke(
        root,
        tail(512, TL_ROOT_FS_TYPE),
        Variant::OfsIntlLongname.dostype(),
    );
    assert!(matches!(
        Volume::open(b.finish(), None),
        Err(Error::VariantMismatch {
            found: Variant::OfsIntlLongname,
            expected: Variant::FfsIntlLongname,
            source: DostypeSource::RootBlock,
        })
    ));
}

#[test]
fn a_root_block_must_verify_structurally() {
    // Right place, no checksum.
    let b = Builder::new(Variant::Ffs, 512, 512, b"Unsummed");
    let disk = b.finish_without_checksums();
    assert!(matches!(
        Volume::open(disk, None),
        Err(Error::Checksum { .. })
    ));

    // Right place, wrong secondary type.
    let mut b = Builder::new(Variant::Ffs, 512, 512, b"NotRoot");
    let root = b.root_lba();
    b.poke(root, tail(512, TL_SECONDARY_TYPE), ST_USERDIR as u32);
    assert!(matches!(
        Volume::open(b.finish(), None),
        Err(Error::WrongSecondaryType {
            found: 2,
            expected: 1,
            ..
        })
    ));

    // Right place, wrong primary type.
    let mut b = Builder::new(Variant::Ffs, 512, 512, b"NotHeader");
    let root = b.root_lba();
    b.poke(root, OFF_TYPE, T_LIST);
    assert!(matches!(
        Volume::open(b.finish(), None),
        Err(Error::NotHeader { found: 16, .. })
    ));
}

#[test]
fn errors_display_and_carry_the_transport_error() {
    let e: Error<MemError> = Error::Io(MemError::OutOfRange { lba: 9, blocks: 4 });
    assert!(e.to_string().contains("lba 9 of 4"));
    let e: Error<MemError> = Error::VariantMismatch {
        found: Variant::FfsIntlLongname,
        expected: Variant::Ffs,
        source: DostypeSource::BootBlock,
    };
    let s = e.to_string();
    assert!(s.contains("boot block") && s.contains("0x444f5307"), "{s}");
    let e: Error<MemError> = Error::ChainCycle { lba: 3 };
    assert!(e.to_string().contains("revisits block 3"));
}
