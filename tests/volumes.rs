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
use amiga_ffs::meta::*;
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

/// The same image as a *sink*: what `format()` writes into.
///
/// A separate impl from [`BlockSource`], as the crate's two traits are —
/// and this type implementing both is exactly the `S: BlockSource +
/// BlockSink` case, so a test can format an image and then open it
/// without moving a byte.
impl BlockSink for MemDisk {
    type Error = MemError;

    fn block_size(&self) -> usize {
        self.bs
    }

    fn write_block(&mut self, lba: u64, buf: &[u8]) -> Result<(), MemError> {
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
        self.data[off..off + self.bs].copy_from_slice(buf);
        Ok(())
    }

    fn block_count(&self) -> Option<u64> {
        Some(self.data.len() as u64 / self.bs as u64)
    }
}

impl MemDisk {
    /// An image of nothing: what a formatter is handed.
    pub fn blank(bs: usize, nblocks: u64) -> Self {
        Self {
            bs,
            data: vec![0u8; bs * nblocks as usize],
        }
    }

    /// An image full of a non-zero byte, so a test can tell "the
    /// formatter wrote this field" from "the field was already zero".
    pub fn filled(bs: usize, nblocks: u64, byte: u8) -> Self {
        Self {
            bs,
            data: vec![byte; bs * nblocks as usize],
        }
    }

    /// Read a block back out for byte-level assertions.
    pub fn block(&self, lba: u64) -> &[u8] {
        let off = lba as usize * self.bs;
        &self.data[off..off + self.bs]
    }

    /// Overwrite a block after the fact — for damage a test wants to
    /// apply *after* `finish()` has fixed the checksums up, so that what
    /// fails is the thing being tested and not a stale sum. Deliberately
    /// not [`BlockSink::write_block`]: it is not a write the volume made,
    /// and it does not check anything.
    pub fn poke_block(&mut self, lba: u64, buf: &[u8]) {
        let off = lba as usize * self.bs;
        self.data[off..off + self.bs].copy_from_slice(buf);
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
    /// Every block the builder handed out, plus the root: what a correct
    /// bitmap for this volume must mark allocated.
    used: Vec<u64>,
    /// Every entry written, so a dircache can be built from the same
    /// facts the hash chains were built from — and then deliberately
    /// diverged from.
    entries: Vec<EntryRec>,
}

/// What the builder remembers about an entry, for building dircaches.
#[derive(Debug, Clone)]
pub struct EntryRec {
    pub dir: u64,
    pub lba: u64,
    pub name: Vec<u8>,
    pub comment: Vec<u8>,
    pub secondary_type: i32,
    pub byte_size: u32,
}

/// One dircache record as the builder lays it down. Public and mutable
/// so a test can write a cache that has gone stale in one specific way.
#[derive(Debug, Clone)]
pub struct DcRecord {
    pub entry: u32,
    pub size: u32,
    pub protection: u32,
    pub uid: u16,
    pub gid: u16,
    pub days: u16,
    pub mins: u16,
    pub ticks: u16,
    pub etype: i8,
    pub name: Vec<u8>,
    pub comment: Vec<u8>,
}

impl DcRecord {
    fn len(&self) -> usize {
        dircache_record_len(self.name.len(), self.comment.len())
    }
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
            used: vec![root_lba],
            entries: vec![],
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

    fn alloc(&mut self) -> u64 {
        let lba = self.next_free;
        self.next_free += 1;
        assert!(lba < self.nblocks, "test volume too small");
        self.used.push(lba);
        lba
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
        let lba = self.alloc();
        self.write_entry(lba, dir_lba, name, comment, kind, byte_size);
        self.chain_in(dir_lba, name, lba);
        lba
    }

    fn write_entry(
        &mut self,
        lba: u64,
        dir_lba: u64,
        name: &[u8],
        comment: &[u8],
        kind: EntryKind,
        byte_size: u32,
    ) {
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
        self.entries.push(EntryRec {
            dir: dir_lba,
            lba,
            name: name.to_vec(),
            comment: comment.to_vec(),
            secondary_type: kind.secondary_type(),
            byte_size: if matches!(kind, EntryKind::File | EntryKind::LinkFile) {
                byte_size
            } else {
                0
            },
        });
    }

    /// Chain an already-written entry into its directory's hash table:
    /// head of the slot if empty, tail of the chain if not.
    fn chain_in(&mut self, dir_lba: u64, name: &[u8], lba: u64) {
        let bs = self.bs;
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
    }

    /// Add a file with real data behind it: data blocks (raw on FFS,
    /// headered on OFS), the header's backward-filled pointer table, and
    /// as many `T_LIST` extension blocks as the data needs.
    pub fn add_file(
        &mut self,
        dir_lba: u64,
        name: &[u8],
        comment: &[u8],
        data: &[u8],
    ) -> FileBlocks {
        let hdr = self.alloc();
        self.write_entry(
            hdr,
            dir_lba,
            name,
            comment,
            EntryKind::File,
            data.len() as u32,
        );

        let (bs, ffs) = (self.bs, self.variant.is_ffs());
        let payload = data_payload_size(bs, ffs);
        let cap = hash_table_size(bs) as usize;
        let count = data.len() / payload + usize::from(data.len() % payload != 0);

        // Allocated up front so each OFS data block can name its
        // successor before it is written.
        let lbas: Vec<u64> = (0..count).map(|_| self.alloc()).collect();

        for (i, chunk) in data.chunks(payload).enumerate() {
            let lba = lbas[i];
            let next = lbas.get(i + 1).copied().unwrap_or(0) as u32;
            let seq = i as u32 + 1;
            let blk = self.block_mut(lba);
            if ffs {
                // Raw: the whole block is payload, and a short final
                // block simply leaves the rest as it found it.
                blk[..chunk.len()].copy_from_slice(chunk);
            } else {
                wr32(blk, OFF_TYPE, T_DATA);
                wr32(blk, OFF_DATA_HEADER_KEY, hdr as u32);
                wr32(blk, OFF_DATA_SEQ, seq);
                wr32(blk, OFF_DATA_SIZE, chunk.len() as u32);
                wr32(blk, OFF_DATA_NEXT, next);
                blk[OFF_DATA_PAYLOAD..OFF_DATA_PAYLOAD + chunk.len()].copy_from_slice(chunk);
            }
            if !ffs {
                self.touched.push(lba);
            }
        }

        // Fill tables from the end backwards, spilling into extension
        // blocks once a table is full.
        let mut extensions: Vec<u64> = Vec::new();
        let mut owner = hdr;
        let mut idx = 0;
        while idx < lbas.len() {
            let n = (lbas.len() - idx).min(cap);
            for k in 0..n {
                let off = data_pointer_offset(bs, k as u32 + 1);
                wr32(self.block_mut(owner), off, lbas[idx + k] as u32);
            }
            wr32(self.block_mut(owner), OFF_HIGH_SEQ, n as u32);
            if owner == hdr {
                wr32(self.block_mut(owner), OFF_FIRST_DATA, lbas[0] as u32);
            }
            idx += n;
            if idx < lbas.len() {
                let ext = self.alloc();
                wr32(self.block_mut(owner), tail(bs, TL_EXTENSION), ext as u32);
                let blk = self.block_mut(ext);
                wr32(blk, OFF_TYPE, T_LIST);
                wr32(blk, OFF_OWN_KEY, ext as u32);
                wr32(blk, tail(bs, TL_PARENT), hdr as u32);
                wr32(blk, tail(bs, TL_SECONDARY_TYPE), ST_FILE as u32);
                self.touched.push(ext);
                extensions.push(ext);
                owner = ext;
            }
        }

        self.chain_in(dir_lba, name, hdr);
        FileBlocks {
            header: hdr,
            data: lbas,
            extensions,
        }
    }

    /// Add a hard link naming `target`, and splice it onto the target's
    /// `next_link` chain the way the filesystem does.
    pub fn add_hard_link(
        &mut self,
        dir_lba: u64,
        name: &[u8],
        kind: EntryKind,
        target: u64,
    ) -> u64 {
        let lba = self.add(dir_lba, name, b"", kind, 0);
        let bs = self.bs;
        wr32(self.block_mut(lba), tail(bs, TL_REAL_ENTRY), target as u32);
        let head = be32(self.block(target), tail(bs, TL_NEXT_LINK));
        wr32(self.block_mut(lba), tail(bs, TL_NEXT_LINK), head);
        wr32(self.block_mut(target), tail(bs, TL_NEXT_LINK), lba as u32);
        lba
    }

    /// Add a soft link: the path goes where a directory's hash table
    /// would, NUL-terminated.
    pub fn add_soft_link(&mut self, dir_lba: u64, name: &[u8], path: &[u8]) -> u64 {
        let lba = self.add(dir_lba, name, b"", EntryKind::SoftLink, 0);
        let blk = self.block_mut(lba);
        blk[OFF_SOFTLINK_PATH..OFF_SOFTLINK_PATH + path.len()].copy_from_slice(path);
        lba
    }

    /// Give `entry_lba` an LNFS overflow comment block, clearing whatever
    /// inline comment it had — which is exactly the on-disk state the
    /// filesystem leaves when a name and a comment will not both fit.
    pub fn add_comment_block(&mut self, entry_lba: u64, comment: &[u8]) -> u64 {
        let lba = self.alloc();
        let blk = self.block_mut(lba);
        wr32(blk, OFF_TYPE, T_COMMENT);
        wr32(blk, OFF_OWN_KEY, lba as u32);
        wr32(blk, OFF_COMMENT_HEADER_KEY, entry_lba as u32);
        wr_bcpl(blk, OFF_COMMENT_TEXT, comment);
        self.touched.push(lba);
        let bs = self.bs;
        wr32(
            self.block_mut(entry_lba),
            tail(bs, TL_COMMENT_BLOCK),
            lba as u32,
        );
        lba
    }

    /// The dircache records a *correct* cache of `dir_lba` would hold —
    /// the same facts the hash chains were built from. Tests take this
    /// and then bend one record, so what they are testing is the
    /// divergence and not the builder.
    pub fn dircache_records(&self, dir_lba: u64) -> Vec<DcRecord> {
        self.entries
            .iter()
            .filter(|e| e.dir == dir_lba)
            .map(|e| DcRecord {
                entry: e.lba as u32,
                size: e.byte_size,
                protection: 0x0000_FFF0 | 0x5,
                uid: 7,
                gid: 0x42,
                days: 1234,
                mins: 56,
                ticks: 7,
                etype: e.secondary_type as i8,
                name: e.name.clone(),
                comment: e.comment.clone(),
            })
            .collect()
    }

    /// Lay down a dircache chain for `dir_lba` and point longword −2 at
    /// it. Records spill into as many `T_DIRCACHE` blocks as they need,
    /// which is the only way to exercise the chain.
    pub fn add_dircache(&mut self, dir_lba: u64, records: &[DcRecord]) -> Vec<u64> {
        let bs = self.bs;
        let capacity = bs - OFF_DIRCACHE_RECORDS_START;

        // Split into blockfuls first, so each block can name its
        // successor as it is written.
        let mut groups: Vec<Vec<&DcRecord>> = vec![vec![]];
        let mut used = 0usize;
        for r in records {
            if used + r.len() > capacity && !groups.last().unwrap().is_empty() {
                groups.push(vec![]);
                used = 0;
            }
            used += r.len();
            groups.last_mut().unwrap().push(r);
        }

        let lbas: Vec<u64> = (0..groups.len()).map(|_| self.alloc()).collect();
        for (i, group) in groups.iter().enumerate() {
            let lba = lbas[i];
            let next = lbas.get(i + 1).copied().unwrap_or(0) as u32;
            let group: Vec<DcRecord> = group.iter().map(|r| (*r).clone()).collect();
            let blk = self.block_mut(lba);
            wr32(blk, OFF_TYPE, T_DIRCACHE);
            wr32(blk, OFF_OWN_KEY, lba as u32);
            wr32(blk, OFF_DIRCACHE_PARENT, dir_lba as u32);
            wr32(blk, OFF_DIRCACHE_RECORDS, group.len() as u32);
            wr32(blk, OFF_DIRCACHE_NEXT, next);
            let mut off = OFF_DIRCACHE_RECORDS_START;
            for r in &group {
                wr32(blk, off + DC_ENTRY, r.entry);
                wr32(blk, off + DC_SIZE, r.size);
                wr32(blk, off + DC_PROTECTION, r.protection);
                blk[off + DC_UID..off + DC_UID + 2].copy_from_slice(&r.uid.to_be_bytes());
                blk[off + DC_GID..off + DC_GID + 2].copy_from_slice(&r.gid.to_be_bytes());
                blk[off + DC_DAYS..off + DC_DAYS + 2].copy_from_slice(&r.days.to_be_bytes());
                blk[off + DC_MINS..off + DC_MINS + 2].copy_from_slice(&r.mins.to_be_bytes());
                blk[off + DC_TICKS..off + DC_TICKS + 2].copy_from_slice(&r.ticks.to_be_bytes());
                blk[off + DC_TYPE] = r.etype as u8;
                blk[off + DC_NAME_LEN] = r.name.len() as u8;
                let n = off + DIRCACHE_RECORD_FIXED;
                blk[n..n + r.name.len()].copy_from_slice(&r.name);
                blk[n + r.name.len()] = r.comment.len() as u8;
                let c = n + r.name.len() + 1;
                blk[c..c + r.comment.len()].copy_from_slice(&r.comment);
                off += r.len();
            }
            self.touched.push(lba);
        }
        wr32(
            self.block_mut(dir_lba),
            tail(bs, TL_EXTENSION),
            lbas[0] as u32,
        );
        lbas
    }

    /// Write an allocation bitmap covering every block the builder has
    /// handed out, and point the root at it.
    ///
    /// Deliberately built from the builder's own allocation record rather
    /// than by walking what it wrote: a bitmap derived from the same walk
    /// the validator does would agree with the validator by construction
    /// and prove nothing.
    ///
    /// `valid` sets the root's `bitmap_flag`; `false` is the shape an
    /// unclean unmount leaves.
    pub fn add_bitmap(&mut self, valid: bool) -> Vec<u64> {
        let (bs, nblocks) = (self.bs, self.nblocks);
        let per_page = bitmap_bits_per_block(bs);
        let need = nblocks - RESERVED;
        let pages = (need / per_page + u64::from(need % per_page != 0)) as usize;
        let lbas: Vec<u64> = (0..pages).map(|_| self.alloc()).collect();

        let used = self.used.clone();
        let words = amiga_ffs::bitmap::pack_bits(RESERVED, nblocks, &used, bs);
        let words_per_page = (bs - OFF_BITMAP_BITS) / 4;
        for (i, &lba) in lbas.iter().enumerate() {
            let blk = self.block_mut(lba);
            for w in 0..words_per_page {
                let v = words
                    .get(i * words_per_page + w)
                    .copied()
                    .unwrap_or(u32::MAX);
                wr32(blk, OFF_BITMAP_BITS + w * 4, v);
            }
            // Longword 0, not longword 5 -- the format's one exception,
            // and the checksums are fixed up here rather than in finish()
            // for exactly that reason.
            let ck = checksum_compute(self.block(lba), BITMAP_CHECKSUM_INDEX);
            wr32(self.block_mut(lba), 0, ck);
        }

        let root = self.root_lba;
        wr32(
            self.block_mut(root),
            tail(bs, TL_BITMAP_FLAG),
            if valid { 0xFFFF_FFFF } else { 0 },
        );
        // Clear the placeholder page list before writing the real one.
        for i in 0..BITMAP_PAGES {
            wr32(self.block_mut(root), tail(bs, TL_BITMAP_PAGES) + i * 4, 0);
        }
        assert!(
            lbas.len() <= BITMAP_PAGES,
            "test volume needs bitmap extension blocks"
        );
        for (i, &lba) in lbas.iter().enumerate() {
            wr32(
                self.block_mut(root),
                tail(bs, TL_BITMAP_PAGES) + i * 4,
                lba as u32,
            );
        }
        lbas
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

/// Where a file the builder wrote actually landed, so a test can corrupt
/// one specific block of it.
pub struct FileBlocks {
    pub header: u64,
    pub data: Vec<u64>,
    pub extensions: Vec<u64>,
}

/// A byte pattern with no period short enough to hide a swapped block:
/// every 512-byte window is distinct, so reading the chain out of order
/// or off by one cannot round-trip.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + i / 251) % 251) as u8).collect()
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

// ---------------------------------------------------------------------------
// File data
// ---------------------------------------------------------------------------

/// 40000 bytes at 512-byte blocks is 79 data blocks, and a header's table
/// holds 72 — so this file *must* cross into an extension block. A test
/// that stays under 36 KB proves nothing about extension blocks at all.
const BIG: usize = 40_000;

#[test]
fn an_ffs_file_crosses_an_extension_block_and_round_trips() {
    let data = pattern(BIG);
    let mut b = Builder::new(Variant::FfsIntl, 512, 400, b"Big");
    let root = b.root_lba();
    let f = b.add_file(root, b"BigFile", b"", &data);
    b.add_file(root, b"Empty", b"", b"");
    assert_eq!(f.data.len(), 79, "79 raw 512-byte blocks");
    assert_eq!(f.extensions.len(), 1, "72 fit in the header, 7 spill");

    let mut vol = Volume::open(b.finish(), None).unwrap();
    let e = vol.lookup(root, b"bigfile").unwrap().unwrap();
    assert_eq!(e.byte_size, BIG as u32);

    // The chain the reader recovers is the chain the builder laid down,
    // in file order -- which is the reverse of the table's slot order.
    let chain = vol.file_chain(e.lba).unwrap();
    assert_eq!(chain.byte_size, BIG as u32);
    assert_eq!(chain.blocks.len(), 79);
    assert_eq!(chain.extensions.len(), 1);
    assert_eq!(
        chain.blocks,
        f.data.iter().map(|&l| l as u32).collect::<Vec<_>>()
    );

    // Every byte, in order.
    assert_eq!(vol.read_file(e.lba).unwrap(), data);

    // Streaming gives the same bytes, and the last block is short: 78
    // full blocks plus 40000 - 78*512 = 64 bytes.
    let mut sizes = Vec::new();
    let mut streamed = Vec::new();
    let n = vol
        .read_file_with(e.lba, |c| {
            sizes.push(c.len());
            streamed.extend_from_slice(c);
        })
        .unwrap();
    assert_eq!(n, BIG as u64);
    assert_eq!(streamed, data);
    assert_eq!(sizes.len(), 79);
    assert!(sizes[..78].iter().all(|&s| s == 512));
    assert_eq!(sizes[78], BIG - 78 * 512);
    assert_eq!(sizes[78], 64);

    // A zero-length file is zero blocks, not one empty one.
    let empty = vol.lookup(root, b"Empty").unwrap().unwrap();
    assert_eq!(vol.file_chain(empty.lba).unwrap().blocks.len(), 0);
    assert_eq!(vol.read_file(empty.lba).unwrap(), b"");
}

#[test]
fn an_ofs_file_crosses_an_extension_block_and_round_trips() {
    let data = pattern(BIG);
    let mut b = Builder::new(Variant::OfsIntl, 512, 400, b"BigOfs");
    let root = b.root_lba();
    let f = b.add_file(root, b"BigFile", b"", &data);
    // 488 payload bytes a block: 82 blocks, so two tables again.
    assert_eq!(f.data.len(), 82);
    assert_eq!(f.extensions.len(), 1);

    let mut vol = Volume::open(b.finish(), None).unwrap();
    let e = vol.lookup(root, b"BigFile").unwrap().unwrap();
    assert_eq!(vol.read_file(e.lba).unwrap(), data);

    let mut sizes = Vec::new();
    vol.read_file_with(e.lba, |c| sizes.push(c.len())).unwrap();
    assert_eq!(sizes.len(), 82);
    assert!(sizes[..81].iter().all(|&s| s == 488));
    assert_eq!(sizes[81], BIG - 81 * 488);
}

#[test]
fn the_same_bytes_occupy_different_blocks_on_ofs_and_ffs() {
    // One variant byte apart, and the *only* difference that byte makes
    // to file data is 24 bytes of header per block -- which changes how
    // many blocks the same file needs, and therefore where every one of
    // them lives.
    let data = pattern(5000);
    let mut counts = Vec::new();
    for variant in [Variant::OfsIntl, Variant::FfsIntl] {
        let mut b = Builder::new(variant, 512, 400, b"Same");
        let root = b.root_lba();
        let f = b.add_file(root, b"Payload", b"", &data);
        counts.push(f.data.len());
        let mut vol = Volume::open(b.finish(), None).unwrap();
        let e = vol.lookup(root, b"Payload").unwrap().unwrap();
        assert_eq!(vol.read_file(e.lba).unwrap(), data, "{variant:?}");
    }
    assert_eq!(counts, vec![11, 10], "OFS pays 24 bytes a block");
}

#[test]
fn file_data_survives_every_block_size() {
    for bs in [512usize, 1024, 4096] {
        for variant in [Variant::OfsIntl, Variant::FfsIntlLongname] {
            // Enough to cross an extension block at 512 and to stay well
            // inside one at 4096: both paths, same assertion.
            let data = pattern(40_000);
            let mut b = Builder::new(variant, bs, 400, b"Sized");
            let root = b.root_lba();
            b.add_file(root, b"Payload", b"", &data);
            let mut vol = Volume::open(b.finish(), None).unwrap();
            let e = vol.lookup(root, b"Payload").unwrap().unwrap();
            assert_eq!(vol.read_file(e.lba).unwrap(), data, "bs {bs} {variant:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// File refusals -- the checks OFS headers exist to make possible
// ---------------------------------------------------------------------------

/// Build a small OFS file and hand back the disk plus its block map, so
/// each corruption test can damage exactly one longword.
fn ofs_file(damage: impl FnOnce(&mut Builder, &FileBlocks)) -> (MemDisk, u64, u64) {
    let mut b = Builder::new(Variant::OfsIntl, 512, 400, b"Verify");
    let root = b.root_lba();
    let f = b.add_file(root, b"Payload", b"", &pattern(3000));
    damage(&mut b, &f);
    (b.finish(), root, f.header)
}

#[test]
fn an_ofs_sequence_number_out_of_place_is_refused() {
    // Data block 2 claiming to be block 7. The header's table is
    // authoritative about *order*; the block's own sequence number is the
    // cross-check the format is offering, and taking it is the whole
    // point of OFS costing 24 bytes a block.
    let (disk, _root, hdr) = ofs_file(|b, f| b.poke(f.data[1], OFF_DATA_SEQ, 7));
    let mut vol = Volume::open(disk, None).unwrap();
    assert!(matches!(
        vol.read_file(hdr),
        Err(Error::DataBlockSequence {
            found: 7,
            expected: 2,
            ..
        })
    ));
}

#[test]
fn an_ofs_header_key_naming_another_file_is_refused() {
    let (disk, _root, hdr) = ofs_file(|b, f| b.poke(f.data[0], OFF_DATA_HEADER_KEY, 3));
    let mut vol = Volume::open(disk, None).unwrap();
    match vol.read_file(hdr) {
        Err(Error::BlockOwnerMismatch {
            found, expected, ..
        }) => {
            assert_eq!(found, 3);
            assert_eq!(expected as u64, hdr);
        }
        other => panic!("expected an owner mismatch, got {other:?}"),
    }
}

#[test]
fn an_ofs_data_size_that_is_short_in_the_middle_is_refused() {
    // Only the *last* block of a file may be short. A short block
    // anywhere else would silently produce a file with a hole in it.
    let (disk, _root, hdr) = ofs_file(|b, f| b.poke(f.data[0], OFF_DATA_SIZE, 100));
    let mut vol = Volume::open(disk, None).unwrap();
    assert!(matches!(
        vol.read_file(hdr),
        Err(Error::DataBlockSize {
            found: 100,
            expected: 488,
            ..
        })
    ));

    // And a size past the block's own capacity, which would read into
    // the next block if it were believed.
    let (disk, _root, hdr) = ofs_file(|b, f| b.poke(f.data[0], OFF_DATA_SIZE, 5000));
    let mut vol = Volume::open(disk, None).unwrap();
    assert!(matches!(
        vol.read_file(hdr),
        Err(Error::DataBlockSize { found: 5000, .. })
    ));
}

#[test]
fn an_ofs_data_block_that_is_not_a_data_block_is_refused() {
    let (disk, _root, hdr) = ofs_file(|b, f| b.poke(f.data[0], OFF_TYPE, T_HEADER));
    let mut vol = Volume::open(disk, None).unwrap();
    assert!(matches!(
        vol.read_file(hdr),
        Err(Error::WrongBlockType {
            found: 2,
            expected: 8,
            ..
        })
    ));
}

#[test]
fn byte_size_and_the_chain_must_agree_in_both_directions() {
    // A header claiming a kilobyte while holding 3000 bytes of blocks.
    let (disk, _root, hdr) = ofs_file(|b, f| b.poke(f.header, tail(512, TL_BYTE_SIZE), 1000));
    let mut vol = Volume::open(disk, None).unwrap();
    assert!(matches!(
        vol.read_file(hdr),
        Err(Error::FileSizeMismatch {
            byte_size: 1000,
            blocks: 7,
            expected: 3,
            ..
        })
    ));

    // And the other way: a header claiming more data than it points at.
    let (disk, _root, hdr) = ofs_file(|b, f| b.poke(f.header, tail(512, TL_BYTE_SIZE), 60_000));
    let mut vol = Volume::open(disk, None).unwrap();
    assert!(matches!(
        vol.read_file(hdr),
        Err(Error::FileSizeMismatch {
            byte_size: 60_000,
            blocks: 7,
            ..
        })
    ));
}

#[test]
fn a_broken_data_pointer_table_is_refused() {
    // high_seq past the table's own slots.
    let (disk, _root, hdr) = ofs_file(|b, f| b.poke(f.header, OFF_HIGH_SEQ, 500));
    let mut vol = Volume::open(disk, None).unwrap();
    assert!(matches!(
        vol.file_chain(hdr),
        Err(Error::DataPointerCount {
            high_seq: 500,
            max: 72,
            ..
        })
    ));

    // A hole where high_seq promised a block. The format has no sparse
    // files, so a zero here is a truncated write, not an empty extent.
    let (disk, _root, hdr) = ofs_file(|b, f| b.poke(f.header, data_pointer_offset(512, 2), 0));
    let mut vol = Volume::open(disk, None).unwrap();
    assert!(matches!(
        vol.file_chain(hdr),
        Err(Error::DataPointerHole { seq: 2, .. })
    ));
}

#[test]
fn an_extension_block_must_verify_structurally() {
    let data = pattern(BIG);
    // Wrong primary type.
    let mut b = Builder::new(Variant::FfsIntl, 512, 400, b"Ext");
    let root = b.root_lba();
    let f = b.add_file(root, b"BigFile", b"", &data);
    b.poke(f.extensions[0], OFF_TYPE, T_HEADER);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert!(matches!(
        vol.file_chain(f.header),
        Err(Error::WrongBlockType {
            found: 2,
            expected: 16,
            ..
        })
    ));

    // An extension block belonging to a different file.
    let mut b = Builder::new(Variant::FfsIntl, 512, 400, b"Ext");
    let root = b.root_lba();
    let f = b.add_file(root, b"BigFile", b"", &data);
    b.poke(f.extensions[0], tail(512, TL_PARENT), 7);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert!(matches!(
        vol.file_chain(f.header),
        Err(Error::BlockOwnerMismatch { found: 7, .. })
    ));

    // An extension block that does not know its own number.
    let mut b = Builder::new(Variant::FfsIntl, 512, 400, b"Ext");
    let root = b.root_lba();
    let f = b.add_file(root, b"BigFile", b"", &data);
    b.poke(f.extensions[0], OFF_OWN_KEY, 999);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert!(matches!(
        vol.file_chain(f.header),
        Err(Error::OwnKeyMismatch { found: 999, .. })
    ));

    // An extension chain that loops back on itself: refused, not hung.
    let mut b = Builder::new(Variant::FfsIntl, 512, 400, b"Ext");
    let root = b.root_lba();
    let f = b.add_file(root, b"BigFile", b"", &data);
    b.poke(
        f.extensions[0],
        tail(512, TL_EXTENSION),
        f.extensions[0] as u32,
    );
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert!(matches!(
        vol.file_chain(f.header),
        Err(Error::ChainCycle { .. })
    ));
}

#[test]
fn reading_a_directory_as_a_file_is_refused() {
    let mut b = Builder::new(Variant::FfsIntl, 512, 400, b"NotAFile");
    let root = b.root_lba();
    let dir = b.add(root, b"C", b"", EntryKind::Directory, 0);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert!(matches!(
        vol.read_file(dir),
        Err(Error::WrongSecondaryType {
            found: 2,
            expected: -3,
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// Metadata: protection bits and dates as they come off a real entry
// ---------------------------------------------------------------------------

#[test]
fn protection_bits_read_inverted_for_the_owner_and_normal_for_everyone_else() {
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Prot");
    let root = b.root_lba();
    // Fresh-file protection: zero. Owner may do everything.
    let open = b.add(root, b"Open", b"", EntryKind::File, 0);
    b.poke(open, tail(512, TL_PROTECTION), 0);
    // Owner denied everything, group granted read+write, others nothing,
    // plus the script bit and a user byte.
    let locked = b.add(root, b"Locked", b"", EntryKind::File, 0);
    b.poke(
        locked,
        tail(512, TL_PROTECTION),
        0x00AB_0000 | FIBF_GRP_READ | FIBF_GRP_WRITE | FIBF_SCRIPT | 0xF,
    );
    let mut vol = Volume::open(b.finish(), None).unwrap();

    let p = vol.entry_at(open).unwrap().protection_bits();
    assert_eq!(p.bits(), 0);
    assert!(p.readable() && p.writable() && p.executable() && p.deletable());
    assert!(!p.group_readable() && !p.other_readable());
    assert_eq!(p.to_string(), "----rwed");

    let e = vol.entry_at(locked).unwrap();
    let p = e.protection_bits();
    assert!(!p.readable() && !p.writable() && !p.executable() && !p.deletable());
    assert!(p.group_readable() && p.group_writable());
    assert!(!p.group_executable() && !p.group_deletable());
    assert!(!p.other_readable() && !p.other_writable());
    assert!(p.script() && !p.hidden() && !p.pure() && !p.archived());
    assert_eq!(p.user_bits(), 0xAB);
    assert_eq!(p.to_string(), "-s------");
    // The raw longword survives untouched, upper bits and all.
    assert_eq!(p.bits(), e.protection);
    assert_eq!(p.bits(), 0x00AB_0C4F);
}

#[test]
fn the_owner_longword_splits_into_uid_and_gid() {
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Owned");
    let root = b.root_lba();
    let lba = b.add(root, b"File", b"", EntryKind::File, 0);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    let e = vol.entry_at(lba).unwrap();
    assert_eq!(e.owner, 0x0007_0042);
    assert_eq!(e.uid(), 7);
    assert_eq!(e.gid(), 0x42);
}

#[test]
fn an_entrys_datestamp_converts_to_a_calendar_date() {
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Dated");
    let root = b.root_lba();
    // 1994-10-30 17:03:12.15 -- an ordinary Amiga file date.
    let want = CalendarDate {
        year: 1994,
        month: 10,
        day: 30,
        hour: 17,
        minute: 3,
        second: 12,
        tick: 15,
    };
    let stamp = DateStamp::from_calendar(want).unwrap();
    let lba = b.add(root, b"File", b"", EntryKind::File, 0);
    b.poke(lba, tail(512, TL_DATE), stamp.days);
    b.poke(lba, tail(512, TL_DATE) + 4, stamp.mins);
    b.poke(lba, tail(512, TL_DATE) + 8, stamp.ticks);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    let e = vol.entry_at(lba).unwrap();
    assert_eq!(e.date, stamp);
    assert_eq!(e.date.to_calendar(), want);
    // And the root's own dates convert too.
    assert_eq!(vol.root().disk_made.to_calendar().year, 1980);
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

#[test]
fn a_hard_link_resolves_to_the_real_object() {
    let data = pattern(1000);
    let mut b = Builder::new(Variant::FfsIntl, 512, 400, b"Linked");
    let root = b.root_lba();
    let f = b.add_file(root, b"Original", b"the real one", &data);
    let dir = b.add(root, b"RealDir", b"", EntryKind::Directory, 0);
    b.add(dir, b"Inside", b"", EntryKind::File, 1);
    let link = b.add_hard_link(root, b"AnotherName", EntryKind::LinkFile, f.header);
    let link2 = b.add_hard_link(root, b"AThirdName", EntryKind::LinkFile, f.header);
    let dlink = b.add_hard_link(root, b"DirAlias", EntryKind::LinkDir, dir);
    let mut vol = Volume::open(b.finish(), None).unwrap();

    let le = vol.entry_at(link).unwrap();
    assert_eq!(le.kind, EntryKind::LinkFile);
    assert_eq!(le.real_entry as u64, f.header);
    let real = vol.resolve_link(&le).unwrap();
    assert_eq!(real.lba, f.header);
    assert_eq!(real.name, b"Original");
    assert_eq!(real.kind, EntryKind::File);
    // ...and the target's data is reachable through the resolved entry.
    assert_eq!(vol.read_file(real.lba).unwrap(), data);

    // The link chain threads every name the object has. The builder
    // inserts at the head, so the newest link is first.
    let target = vol.entry_at(f.header).unwrap();
    assert_eq!(target.next_link as u64, link2);
    assert_eq!(vol.entry_at(link2).unwrap().next_link as u64, link);
    assert_eq!(vol.entry_at(link).unwrap().next_link, 0);

    // A hard link to a directory resolves to a directory that can then
    // be listed -- which the link block itself cannot be.
    let dl = vol.entry_at(dlink).unwrap();
    assert!(matches!(
        vol.read_dir(dl.lba),
        Err(Error::NotADirectory { found: 4, .. })
    ));
    let rd = vol.resolve_link(&dl).unwrap();
    assert_eq!(rd.name, b"RealDir");
    assert_eq!(
        names(&vol.read_dir(rd.lba).unwrap()),
        vec![b"Inside".to_vec()]
    );

    // Resolving something already real is a no-op, so callers can pipe
    // every listing entry through it.
    let same = vol.resolve_link(&real).unwrap();
    assert_eq!(same, real);

    // Plain entries carry no link target, whatever longword -11 holds.
    assert_eq!(real.real_entry, 0);
}

#[test]
fn a_link_to_a_link_is_followed_and_a_loop_is_refused() {
    let mut b = Builder::new(Variant::FfsIntl, 512, 400, b"Chained");
    let root = b.root_lba();
    let f = b.add_file(root, b"Original", b"", b"hello");
    let one = b.add_hard_link(root, b"One", EntryKind::LinkFile, f.header);
    let two = b.add_hard_link(root, b"Two", EntryKind::LinkFile, one);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    let e = vol.entry_at(two).unwrap();
    assert_eq!(vol.resolve_link(&e).unwrap().lba, f.header);

    // Now make it a ring: two -> one -> two.
    let mut b = Builder::new(Variant::FfsIntl, 512, 400, b"Looped");
    let root = b.root_lba();
    let f = b.add_file(root, b"Original", b"", b"hello");
    let one = b.add_hard_link(root, b"One", EntryKind::LinkFile, f.header);
    let two = b.add_hard_link(root, b"Two", EntryKind::LinkFile, one);
    b.poke(one, tail(512, TL_REAL_ENTRY), two as u32);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    let e = vol.entry_at(two).unwrap();
    assert!(matches!(
        vol.resolve_link(&e),
        Err(Error::ChainCycle { .. })
    ));

    // A link pointing at itself is the same refusal, one step sooner.
    let mut b = Builder::new(Variant::FfsIntl, 512, 400, b"Selfish");
    let root = b.root_lba();
    let f = b.add_file(root, b"Original", b"", b"hello");
    let one = b.add_hard_link(root, b"One", EntryKind::LinkFile, f.header);
    b.poke(one, tail(512, TL_REAL_ENTRY), one as u32);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    let e = vol.entry_at(one).unwrap();
    assert!(matches!(
        vol.resolve_link(&e),
        Err(Error::ChainCycle { lba } ) if lba == one
    ));

    // And a link that points at nothing at all.
    let mut b = Builder::new(Variant::FfsIntl, 512, 400, b"Dangling");
    let root = b.root_lba();
    let f = b.add_file(root, b"Original", b"", b"hello");
    let one = b.add_hard_link(root, b"One", EntryKind::LinkFile, f.header);
    b.poke(one, tail(512, TL_REAL_ENTRY), 0);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    let e = vol.entry_at(one).unwrap();
    assert!(matches!(
        vol.resolve_link(&e),
        Err(Error::LinkTargetMissing { lba }) if lba == one
    ));
}

#[test]
fn a_soft_link_surfaces_its_path_and_is_not_resolved_behind_the_caller() {
    let path = b"Work:Tools/Editor/Ed";
    let mut b = Builder::new(Variant::FfsIntlLongname, 512, 400, b"Soft");
    let root = b.root_lba();
    let sl = b.add_soft_link(root, b"Ed", path);
    let mut vol = Volume::open(b.finish(), None).unwrap();

    assert_eq!(vol.read_softlink(sl).unwrap(), path);

    // lookup_path returns the link itself, not whatever the path names:
    // resolving it may cross volumes, and that is the consumer's call.
    let e = vol.lookup_path(root, b"Ed").unwrap().unwrap();
    assert_eq!(e.kind, EntryKind::SoftLink);
    assert_eq!(e.lba, sl);
    assert!(matches!(
        vol.resolve_link(&e),
        Err(Error::SoftLinkNotResolved { lba }) if lba == sl
    ));

    // A path filling the field to its capacity still reads back whole,
    // and one that is not a soft link is refused rather than guessed at.
    let long = vec![b'p'; softlink_path_capacity(512) - 1];
    let mut b = Builder::new(Variant::FfsIntl, 512, 400, b"Soft");
    let root = b.root_lba();
    let sl = b.add_soft_link(root, b"Long", &long);
    let f = b.add_file(root, b"Real", b"", b"x");
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert_eq!(vol.read_softlink(sl).unwrap(), long);
    assert!(matches!(
        vol.read_softlink(f.header),
        Err(Error::WrongSecondaryType {
            found: -3,
            expected: 3,
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// LNFS overflow comments
// ---------------------------------------------------------------------------

#[test]
fn an_lnfs_comment_that_did_not_fit_is_read_from_its_own_block() {
    // A 107-byte name leaves four bytes of the 112-byte NaC field, so a
    // 60-character comment cannot live beside it. The filesystem writes
    // a T_COMMENT block and leaves the inline comment empty -- which is
    // the trap: empty inline does not mean no comment.
    let long107: Vec<u8> = (0..MAX_NAME_LONG)
        .map(|i| b"abcdefghijklmnopqrstuvwxyz0123456789"[i % 36])
        .collect();
    let overflow = b"a comment far too long to share a field with that name".to_vec();

    let mut b = Builder::new(Variant::FfsIntlLongname, 512, 400, b"Comments");
    let root = b.root_lba();
    let big = b.add(root, &long107, b"", EntryKind::File, 5);
    let cb = b.add_comment_block(big, &overflow);
    b.add(root, b"Short", b"fits inline", EntryKind::File, 5);
    let mut vol = Volume::open(b.finish(), None).unwrap();

    let e = vol.lookup(root, &long107).unwrap().unwrap();
    assert_eq!(e.name, long107);
    assert!(e.comment.is_empty(), "inline comment is empty...");
    assert_eq!(e.comment_block as u64, cb, "...because it overflowed");
    assert_eq!(vol.comment(&e).unwrap(), overflow);

    // An entry whose comment did fit needs no second read.
    let s = vol.lookup(root, b"Short").unwrap().unwrap();
    assert_eq!(s.comment_block, 0);
    assert_eq!(vol.comment(&s).unwrap(), b"fits inline");

    // A comment block written for a different entry is not this entry's
    // comment, whatever the pointer says.
    let mut b = Builder::new(Variant::FfsIntlLongname, 512, 400, b"Comments");
    let root = b.root_lba();
    let big = b.add(root, &long107, b"", EntryKind::File, 5);
    let cb = b.add_comment_block(big, &overflow);
    b.poke(cb, OFF_COMMENT_HEADER_KEY, 4);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    let e = vol.lookup(root, &long107).unwrap().unwrap();
    assert!(matches!(
        vol.comment(&e),
        Err(Error::BlockOwnerMismatch { found: 4, .. })
    ));

    // And a block that is not a comment block at all.
    let mut b = Builder::new(Variant::FfsIntlLongname, 512, 400, b"Comments");
    let root = b.root_lba();
    let big = b.add(root, &long107, b"", EntryKind::File, 5);
    let cb = b.add_comment_block(big, &overflow);
    b.poke(cb, OFF_TYPE, T_HEADER);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    let e = vol.lookup(root, &long107).unwrap().unwrap();
    assert!(matches!(
        vol.comment(&e),
        Err(Error::WrongBlockType {
            found: 2,
            expected: 64,
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// Dircache blocks -- read, and never believed
// ---------------------------------------------------------------------------

/// A DOS\5 volume with a handful of entries and an accurate cache, plus
/// the entry LBAs, so each stale-cache test can bend exactly one record.
fn dircache_volume(bend: impl FnOnce(&mut Vec<DcRecord>, &Builder)) -> (MemDisk, u64) {
    let mut b = Builder::new(Variant::FfsIntlDircache, 512, 512, b"Cached");
    let root = b.root_lba();
    b.add(root, b"Foo", b"", EntryKind::Directory, 0);
    b.add_file(root, b"bar.txt", b"a comment", &pattern(12));
    b.add_file(root, b"xyzzyx", b"", &pattern(3));
    let mut records = b.dircache_records(root);
    bend(&mut records, &b);
    b.add_dircache(root, &records);
    b.add_bitmap(true);
    (b.finish(), root)
}

#[test]
fn a_dircache_reads_back_record_for_record() {
    let (disk, root) = dircache_volume(|_, _| {});
    let mut vol = Volume::open(disk, None).unwrap();
    assert_eq!(vol.variant(), Variant::FfsIntlDircache);

    let cache = vol.read_dircache(root).unwrap();
    assert_eq!(cache.dir_lba, root);
    assert_eq!(
        cache.blocks.len(),
        1,
        "three short records fit in one block"
    );
    assert_eq!(cache.records.len(), 3);

    let names: Vec<Vec<u8>> = cache.records.iter().map(|r| r.name.clone()).collect();
    assert_eq!(
        names,
        vec![b"Foo".to_vec(), b"bar.txt".to_vec(), b"xyzzyx".to_vec()]
    );
    let bar = &cache.records[1];
    assert_eq!(bar.size, 12);
    assert_eq!(bar.comment, b"a comment");
    assert_eq!(bar.entry_type, ST_FILE as i8);
    assert_eq!(bar.uid, 7);
    assert_eq!(bar.gid, 0x42);
    assert_eq!(bar.owner(), 0x0007_0042);
    assert_eq!(bar.protection, 0x0000_FFF5);
    // The DateStamp is three *words* in a dircache record, widened back
    // out on the way in.
    assert_eq!(
        bar.date,
        DateStamp {
            days: 1234,
            mins: 56,
            ticks: 7
        }
    );
    // Records are word-aligned: 25 + name + comment, rounded up. "Foo"
    // with no comment is 28 bytes, so the second record starts at 52.
    assert_eq!(cache.records[0].offset, OFF_DIRCACHE_RECORDS_START);
    assert_eq!(cache.records[1].offset, 52);

    // ...and a cache that agrees with the chains produces no findings.
    let report = vol.validate();
    assert!(report.is_clean(), "{:?}", report.findings);
    assert_eq!(report.summary.dircache_blocks, 1);
}

#[test]
fn a_dircache_that_needs_more_than_one_block_chains() {
    // 30-character names: 25 + 30 = 55, padded to 56, so eight records a
    // 512-byte block. Twenty of them is three blocks.
    let mut b = Builder::new(Variant::FfsIntlDircache, 512, 512, b"Chained");
    let root = b.root_lba();
    for i in 0..20u32 {
        let name = format!("entry-with-a-long-name-{i:07}").into_bytes();
        assert_eq!(name.len(), 30);
        b.add_file(root, &name, b"", &pattern(i as usize));
    }
    let records = b.dircache_records(root);
    let blocks = b.add_dircache(root, &records);
    b.add_bitmap(true);
    assert_eq!(blocks.len(), 3, "8 + 8 + 4 records");

    let mut vol = Volume::open(b.finish(), None).unwrap();
    let cache = vol.read_dircache(root).unwrap();
    assert_eq!(cache.blocks, blocks);
    assert_eq!(cache.records.len(), 20);
    for (i, r) in cache.records.iter().enumerate() {
        assert_eq!(r.size, i as u32);
    }
    let report = vol.validate();
    assert!(report.is_clean(), "{:?}", report.findings);
    assert_eq!(report.summary.dircache_blocks, 3);
}

#[test]
fn a_variant_without_dircaches_reports_none_rather_than_guessing() {
    // Longword -2 exists on every variant; on DOS\3 it is not a dircache
    // pointer, and following it would be a guess.
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"NoCache");
    let root = b.root_lba();
    b.add(root, b"File", b"", EntryKind::File, 1);
    b.poke(root, tail(512, TL_EXTENSION), 300); // whatever happens to be there
    b.add_bitmap(true);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert_eq!(vol.dircache_head(root).unwrap(), 0);
    assert_eq!(vol.read_dircache(root).unwrap().records, vec![]);
}

#[test]
fn a_stale_dircache_is_reported_and_never_believed() {
    // An entry deleted from the cache but still in the chains: a listing
    // served from this cache would not show the file at all.
    let (disk, root) = dircache_volume(|recs, _| {
        recs.retain(|r| r.name != b"bar.txt");
    });
    let mut vol = Volume::open(disk, None).unwrap();
    // Lookup still finds it, because lookup never consults the cache.
    assert!(vol.lookup(root, b"bar.txt").unwrap().is_some());
    let report = vol.validate();
    assert!(
        report.findings.iter().any(|f| matches!(
            f,
            Finding::DircacheStale {
                detail: DircacheDiscrepancy::Missing { .. },
                ..
            }
        )),
        "{:?}",
        report.findings
    );

    // A record for something the chains do not hold: a listing served
    // from this cache shows a file that is not there.
    let (disk, _) = dircache_volume(|recs, _| {
        let mut ghost = recs[0].clone();
        ghost.entry = 400;
        ghost.name = b"deleted-but-cached".to_vec();
        recs.push(ghost);
    });
    let mut vol = Volume::open(disk, None).unwrap();
    let report = vol.validate();
    assert!(report.findings.iter().any(|f| matches!(
        f,
        Finding::DircacheStale {
            detail: DircacheDiscrepancy::Extra { entry: 400 },
            ..
        }
    )));

    // A rename the cache did not see.
    let (disk, _) = dircache_volume(|recs, _| recs[0].name = b"OldName".to_vec());
    let mut vol = Volume::open(disk, None).unwrap();
    let report = vol.validate();
    assert!(report.findings.iter().any(|f| matches!(
        f,
        Finding::DircacheStale {
            detail: DircacheDiscrepancy::NameMismatch { .. },
            ..
        }
    )));

    // ...but a *case* difference is not a rename: the filesystem folds,
    // so `FOO` and `Foo` are the same name and the cache is not stale.
    let (disk, _) = dircache_volume(|recs, _| recs[0].name = b"FOO".to_vec());
    let mut vol = Volume::open(disk, None).unwrap();
    assert!(vol.validate().is_clean());

    // A file that grew after the cache was written.
    let (disk, _) = dircache_volume(|recs, _| recs[1].size = 99);
    let mut vol = Volume::open(disk, None).unwrap();
    let report = vol.validate();
    assert!(report.findings.iter().any(|f| matches!(
        f,
        Finding::DircacheStale {
            detail: DircacheDiscrepancy::SizeMismatch {
                cached: 99,
                actual: 12,
                ..
            },
            ..
        }
    )));

    // The same entry cached twice.
    let (disk, _) = dircache_volume(|recs, _| {
        let dup = recs[2].clone();
        recs.push(dup);
    });
    let mut vol = Volume::open(disk, None).unwrap();
    let report = vol.validate();
    assert!(report.findings.iter().any(|f| matches!(
        f,
        Finding::DircacheStale {
            detail: DircacheDiscrepancy::Duplicate { .. },
            ..
        }
    )));

    // A directory cached as a file.
    let (disk, _) = dircache_volume(|recs, _| recs[0].etype = ST_FILE as i8);
    let mut vol = Volume::open(disk, None).unwrap();
    let report = vol.validate();
    assert!(report.findings.iter().any(|f| matches!(
        f,
        Finding::DircacheStale {
            detail: DircacheDiscrepancy::TypeMismatch {
                cached: -3,
                actual: 2,
                ..
            },
            ..
        }
    )));

    // ...but a *zero* type byte means "not recorded", not "wrong": some
    // writers lay down the whole record and never fill it in, and
    // flagging every entry of every image they make would be useless.
    let (disk, _) = dircache_volume(|recs, _| {
        for r in recs.iter_mut() {
            r.etype = 0;
        }
    });
    let mut vol = Volume::open(disk, None).unwrap();
    assert!(vol.validate().is_clean());
}

#[test]
fn a_dircache_block_must_verify_structurally() {
    // A cache block that names another directory as its parent.
    let mut b = Builder::new(Variant::FfsIntlDircache, 512, 512, b"Bad");
    let root = b.root_lba();
    b.add(root, b"File", b"", EntryKind::File, 1);
    let recs = b.dircache_records(root);
    let dc = b.add_dircache(root, &recs)[0];
    b.poke(dc, OFF_DIRCACHE_PARENT, 7);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert!(matches!(
        vol.read_dircache(root),
        Err(Error::BlockOwnerMismatch { found: 7, .. })
    ));

    // A block that is not a dircache block at all.
    let mut b = Builder::new(Variant::FfsIntlDircache, 512, 512, b"Bad");
    let root = b.root_lba();
    b.add(root, b"File", b"", EntryKind::File, 1);
    let recs = b.dircache_records(root);
    let dc = b.add_dircache(root, &recs)[0];
    b.poke(dc, OFF_TYPE, T_HEADER);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert!(matches!(
        vol.read_dircache(root),
        Err(Error::WrongBlockType {
            found: 2,
            expected: 33,
            ..
        })
    ));

    // A record count promising more records than the block can hold: the
    // count and the two length bytes are three separate chances to walk
    // off the end of a 512-byte buffer, and none of them is trusted.
    let mut b = Builder::new(Variant::FfsIntlDircache, 512, 512, b"Bad");
    let root = b.root_lba();
    b.add(root, b"File", b"", EntryKind::File, 1);
    let recs = b.dircache_records(root);
    let dc = b.add_dircache(root, &recs)[0];
    b.poke(dc, OFF_DIRCACHE_RECORDS, 500);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert!(matches!(
        vol.read_dircache(root),
        Err(Error::DircacheRecordOverflow { .. })
    ));

    // A name length byte reaching past the block's end.
    let mut b = Builder::new(Variant::FfsIntlDircache, 512, 512, b"Bad");
    let root = b.root_lba();
    b.add(root, b"File", b"", EntryKind::File, 1);
    let recs = b.dircache_records(root);
    let dc = b.add_dircache(root, &recs)[0];
    let mut disk = b.finish();
    let mut raw = vec![0u8; 512];
    disk.read_block(dc, &mut raw).unwrap();
    raw[OFF_DIRCACHE_RECORDS_START + DC_NAME_LEN] = 255;
    disk.poke_block(dc, &raw);
    let mut vol = Volume::open(disk, None).unwrap();
    let e = vol.read_dircache(root);
    assert!(
        matches!(
            e,
            Err(Error::DircacheRecordOverflow { .. }) | Err(Error::Checksum { .. })
        ),
        "{e:?}"
    );

    // A cache chain that points back at itself.
    let mut b = Builder::new(Variant::FfsIntlDircache, 512, 512, b"Bad");
    let root = b.root_lba();
    b.add(root, b"File", b"", EntryKind::File, 1);
    let recs = b.dircache_records(root);
    let dc = b.add_dircache(root, &recs)[0];
    b.poke(dc, OFF_DIRCACHE_NEXT, dc as u32);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert!(matches!(
        vol.read_dircache(root),
        Err(Error::ChainCycle { .. })
    ));
}

// ---------------------------------------------------------------------------
// The bitmap
// ---------------------------------------------------------------------------

#[test]
fn the_bitmap_marks_exactly_the_blocks_the_volume_uses() {
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Allocated");
    let root = b.root_lba();
    let dir = b.add(root, b"Dir", b"", EntryKind::Directory, 0);
    b.add(dir, b"Inner", b"", EntryKind::File, 0);
    let f = b.add_file(root, b"Payload", b"", &pattern(3000));
    let pages = b.add_bitmap(true);
    let mut vol = Volume::open(b.finish(), None).unwrap();

    let bm = vol.read_bitmap().unwrap();
    assert!(bm.valid());
    assert_eq!(bm.pages(), &pages[..]);
    assert_eq!(bm.ext_blocks(), &[] as &[u64]);
    assert_eq!(bm.first_block(), 2);
    assert_eq!(bm.end_block(), 512);
    assert!(bm.covers_whole_volume());

    // The root, the bitmap page, the directory tree and every data block.
    for lba in [root, pages[0], dir, f.header] {
        assert_eq!(bm.is_allocated(lba), Some(true), "block {lba}");
        assert_eq!(bm.is_free(lba), Some(false));
    }
    for &d in &f.data {
        assert_eq!(bm.is_allocated(d), Some(true), "data block {d}");
    }
    // A block nothing uses is free.
    assert_eq!(bm.is_allocated(500), Some(false));
    assert_eq!(bm.is_free(500), Some(true));

    // The two boot blocks have no bit at all: they are not allocatable,
    // so the bitmap does not answer for them -- which is deliberately
    // not the same answer as "free".
    assert!(!bm.covers(0));
    assert!(!bm.covers(1));
    assert_eq!(bm.is_allocated(0), None);
    assert_eq!(bm.is_allocated(512), None);

    // Counts, and the iterators that back them.
    assert_eq!(bm.covered_count(), 510);
    assert_eq!(bm.allocated_count() + bm.free_count(), 510);
    assert_eq!(bm.allocated().count() as u64, bm.allocated_count());
    assert_eq!(bm.free().count() as u64, bm.free_count());
    assert_eq!(bm.covered().count(), 510);
    let allocated: Vec<u64> = bm.allocated().collect();
    assert!(allocated.contains(&root) && allocated.contains(&f.header));
    assert!(!allocated.contains(&500));
    // 1 = free, so an almost-empty volume is almost all ones.
    assert!(bm.free_count() > bm.allocated_count());
}

#[test]
fn an_invalid_bitmap_flag_is_surfaced_rather_than_hidden() {
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Dirty");
    let root = b.root_lba();
    b.add(root, b"File", b"", EntryKind::File, 0);
    b.add_bitmap(false);
    let mut vol = Volume::open(b.finish(), None).unwrap();

    assert_eq!(vol.root().bitmap_flag, 0);
    // The bits are still readable -- a recovery tool wants to see what
    // the interrupted update left -- and still marked untrustworthy.
    let bm = vol.read_bitmap().unwrap();
    assert!(!bm.valid());
    assert_eq!(bm.is_allocated(root), Some(true));

    // validate() says so, and then declines to compare against it: every
    // disagreement with a bitmap mid-update is noise.
    let report = vol.validate();
    assert!(report
        .findings
        .iter()
        .any(|f| matches!(f, Finding::BitmapInvalid)));
    assert!(!report
        .findings
        .iter()
        .any(|f| matches!(f, Finding::OrphanBlock { .. })));
    assert!(report
        .findings
        .iter()
        .any(|f| f.to_string().contains("must not be trusted")));
}

#[test]
fn a_bitmap_page_with_a_bad_checksum_is_refused() {
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Broken");
    let root = b.root_lba();
    b.add(root, b"File", b"", EntryKind::File, 0);
    let pages = b.add_bitmap(true);
    // One flipped bit anywhere in the page breaks the sum -- and the sum
    // is over the whole block including longword 0, which is where a
    // bitmap block keeps its checksum rather than longword 5.
    b.poke(pages[0], 64, 0x1234_5678);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    assert!(matches!(
        vol.read_bitmap(),
        Err(Error::Checksum { lba }) if lba == pages[0]
    ));
}

#[test]
fn the_bitmap_checksum_lives_in_longword_zero() {
    // Stated as an assertion because it is the format's one exception and
    // the failure mode is silent: verification never needs the index, so
    // a writer using longword 5 produces blocks that verify against
    // themselves and against no other implementation.
    assert_eq!(BITMAP_CHECKSUM_INDEX, 0);
    assert_ne!(BITMAP_CHECKSUM_INDEX, CHECKSUM_INDEX);
    let mut page = vec![0u8; 512];
    page[64] = 0x42;
    let ck = checksum_compute(&page, BITMAP_CHECKSUM_INDEX);
    page[0..4].copy_from_slice(&ck.to_be_bytes());
    assert!(checksum_ok(&page));
}

// ---------------------------------------------------------------------------
// validate()
// ---------------------------------------------------------------------------

/// A volume with a bit of everything: two directory levels, a file big
/// enough to need an extension block, a hard link and a soft link.
fn healthy_volume(variant: Variant) -> (MemDisk, u64) {
    let mut b = Builder::new(variant, 512, 512, b"Healthy");
    let root = b.root_lba();
    let dir = b.add(root, b"Devs", b"", EntryKind::Directory, 0);
    b.add(dir, b"system-configuration", b"", EntryKind::File, 0);
    let f = b.add_file(root, b"Payload", b"a comment", &pattern(3000));
    b.add_hard_link(root, b"Alias", EntryKind::LinkFile, f.header);
    b.add_soft_link(root, b"Elsewhere", b"Work:Tools/Ed");
    if variant.has_dircache() {
        for d in [root, dir] {
            let recs = b.dircache_records(d);
            b.add_dircache(d, &recs);
        }
    }
    b.add_bitmap(true);
    (b.finish(), root)
}

#[test]
fn a_healthy_volume_validates_clean_on_every_variant() {
    for variant in [
        Variant::Ofs,
        Variant::Ffs,
        Variant::FfsIntl,
        Variant::OfsIntlDircache,
        Variant::FfsIntlDircache,
        Variant::FfsIntlLongname,
    ] {
        let (disk, _root) = healthy_volume(variant);
        let mut vol = Volume::open(disk, None).unwrap();
        let report = vol.validate();
        assert!(report.is_clean(), "{variant:?}: {:?}", report.findings);
        assert!(!report.truncated);

        let s = report.summary;
        assert_eq!(s.directories, 2, "{variant:?}");
        assert_eq!(s.files, 2);
        assert_eq!(s.hard_links, 1);
        assert_eq!(s.soft_links, 1);
        assert_eq!(
            s.dircache_blocks,
            if variant.has_dircache() { 2 } else { 0 }
        );
        assert_eq!(s.bitmap_blocks, 1);
        // Every reachable block is allocated, and nothing else is.
        assert_eq!(s.reachable, s.allocated, "{variant:?}");
        assert_eq!(s.orphans, 0);
        assert_eq!(s.reachable_but_free, 0);
        assert_eq!(s.allocated + s.free, 510);
        // FFS data blocks are counted reachable without being verified:
        // there is nothing in one to verify.
        assert!(s.data_blocks > 0);
        assert_eq!(s.extension_blocks, 0, "3000 bytes fits one table");
    }
}

#[test]
fn validate_reports_a_leaked_block_and_a_double_allocation_separately() {
    // An orphan: allocated, reached by nothing. Space lost, and harmless
    // until a validator frees it.
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Leaky");
    let root = b.root_lba();
    b.add(root, b"File", b"", EntryKind::File, 0);
    let stray = b.add(root, b"Stray", b"", EntryKind::File, 0);
    // ...unchained from the directory, but still marked allocated.
    let slot = name_hash(b"Stray", Variant::FfsIntl.fold(), hash_table_size(512)) as usize;
    b.poke(root, OFF_HASH_TABLE + slot * 4, 0);
    b.add_bitmap(true);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    let report = vol.validate();
    assert!(
        report
            .findings
            .iter()
            .any(|f| matches!(f, Finding::OrphanBlock { lba } if *lba == stray)),
        "{:?}",
        report.findings
    );
    assert_eq!(report.summary.orphans, 1);
    assert_eq!(report.summary.reachable_but_free, 0);

    // The other direction, and the dangerous one: a block a file is
    // using, marked free. The next allocation hands it to a second owner.
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Doomed");
    let root = b.root_lba();
    let f = b.add_file(root, b"Payload", b"", &pattern(3000));
    let pages = b.add_bitmap(true);
    // Free the third data block by hand, then re-checksum the page.
    let victim = f.data[2];
    let bit = victim - 2;
    let off = OFF_BITMAP_BITS + (bit / 32) as usize * 4;
    let mut disk = b.finish();
    let mut page = vec![0u8; 512];
    disk.read_block(pages[0], &mut page).unwrap();
    let w = be32(&page, off) | 1 << (bit % 32);
    page[off..off + 4].copy_from_slice(&w.to_be_bytes());
    page[0..4].copy_from_slice(&0u32.to_be_bytes());
    let ck = checksum_compute(&page, BITMAP_CHECKSUM_INDEX);
    page[0..4].copy_from_slice(&ck.to_be_bytes());
    disk.poke_block(pages[0], &page);

    let mut vol = Volume::open(disk, None).unwrap();
    let report = vol.validate();
    assert!(
        report
            .findings
            .iter()
            .any(|f| matches!(f, Finding::ReachableButFree { lba } if *lba == victim)),
        "{:?}",
        report.findings
    );
    assert_eq!(report.summary.reachable_but_free, 1);
    assert_eq!(report.summary.orphans, 0);
}

#[test]
fn validate_finds_an_entry_in_the_wrong_hash_slot() {
    // The entry parses perfectly and enumeration lists it; only lookup by
    // name will never find it, because lookup walks one chain.
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Misfiled");
    let root = b.root_lba();
    let lba = b.add(root, b"Hidden", b"", EntryKind::File, 0);
    let right = name_hash(b"Hidden", Variant::FfsIntl.fold(), hash_table_size(512)) as usize;
    let wrong = (right + 1) % hash_table_size(512) as usize;
    b.poke(root, OFF_HASH_TABLE + right * 4, 0);
    b.poke(root, OFF_HASH_TABLE + wrong * 4, lba as u32);
    b.add_bitmap(true);
    let mut vol = Volume::open(b.finish(), None).unwrap();

    // The symptom, first: enumeration sees it, lookup does not.
    assert_eq!(vol.read_dir(root).unwrap().len(), 1);
    assert!(vol.lookup(root, b"Hidden").unwrap().is_none());

    let report = vol.validate();
    let found = report
        .findings
        .iter()
        .find(|f| matches!(f, Finding::WrongChainSlot { lba: l, .. } if *l == lba));
    match found {
        Some(Finding::WrongChainSlot {
            found, hashes_to, ..
        }) => {
            assert_eq!(*found as usize, wrong);
            assert_eq!(*hashes_to as usize, right);
        }
        other => panic!("expected a WrongChainSlot finding, got {other:?}"),
    }
    assert!(report
        .findings
        .iter()
        .any(|f| f.to_string().contains("lookup never will")));
}

#[test]
fn validate_keeps_going_past_a_corrupt_block() {
    // Three files, the middle one's header corrupted. A validator that
    // stopped here would say nothing about the third -- and recovery
    // needs the read side most of all.
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Damaged");
    let root = b.root_lba();
    b.add(root, b"First", b"", EntryKind::File, 0);
    let broken = b.add(root, b"Second", b"", EntryKind::File, 0);
    b.add(root, b"Third", b"", EntryKind::File, 0);
    let dir = b.add(root, b"Sub", b"", EntryKind::Directory, 0);
    b.add(dir, b"Inside", b"", EntryKind::File, 0);
    b.add_bitmap(true);
    let mut disk = b.finish();
    // Break the checksum *after* finish, so what fails is the sum and not
    // the structure.
    let mut raw = vec![0u8; 512];
    disk.read_block(broken, &mut raw).unwrap();
    raw[100] ^= 0xFF;
    disk.poke_block(broken, &raw);

    let mut vol = Volume::open(disk, None).unwrap();
    let report = vol.validate();
    assert!(report
        .findings
        .iter()
        .any(|f| matches!(f, Finding::Checksum { lba } if *lba == broken)));
    // The walk carried on: the subdirectory and its file were still
    // reached, so the summary counts them.
    assert_eq!(report.summary.directories, 2);
    assert!(report.summary.files >= 2);
    // ...and the block it could not read is now an orphan, correctly.
    assert!(report
        .findings
        .iter()
        .any(|f| matches!(f, Finding::OrphanBlock { lba } if *lba == broken)));
}

#[test]
fn validate_names_the_structural_disagreements_individually() {
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Structural");
    let root = b.root_lba();
    let bad_key = b.add(root, b"BadKey", b"", EntryKind::File, 0);
    let bad_parent = b.add(root, b"BadParent", b"", EntryKind::File, 0);
    let no_name = b.add(root, b"Nameless", b"", EntryKind::File, 0);
    let dangling = b.add_hard_link(root, b"Dangling", EntryKind::LinkFile, bad_key);
    b.poke(bad_key, OFF_OWN_KEY, 999);
    b.poke(bad_parent, tail(512, TL_PARENT), 3);
    b.poke(no_name, tail(512, TL_NAME), 0); // length byte to zero
    b.poke(dangling, tail(512, TL_REAL_ENTRY), 0);
    b.add_bitmap(true);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    let report = vol.validate();
    let fs = &report.findings;

    assert!(fs
        .iter()
        .any(|f| matches!(f, Finding::OwnKeyMismatch { lba, found: 999 } if *lba == bad_key)));
    assert!(fs.iter().any(
        |f| matches!(f, Finding::ParentMismatch { lba, found: 3, expected } if *lba == bad_parent && *expected == root)
    ));
    assert!(fs
        .iter()
        .any(|f| matches!(f, Finding::EmptyName { lba } if *lba == no_name)));
    assert!(fs
        .iter()
        .any(|f| matches!(f, Finding::LinkTargetMissing { lba } if *lba == dangling)));
    // An empty name hashes to slot 0, so it is also in the wrong chain --
    // but that is reported only when there is a name to hash.
    assert!(!fs
        .iter()
        .any(|f| matches!(f, Finding::WrongChainSlot { lba, .. } if *lba == no_name)));
}

#[test]
fn validate_reports_rather_than_erroring_on_an_out_of_range_pointer() {
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Wild");
    let root = b.root_lba();
    let f = b.add_file(root, b"Payload", b"", &pattern(1000));
    b.poke(f.header, data_pointer_offset(512, 1), 100_000);
    b.add_bitmap(true);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    let report = vol.validate();
    assert!(
        report.findings.iter().any(|f| matches!(
            f,
            Finding::Unreadable {
                error: Error::LbaOutOfRange { lba: 100_000, .. },
                ..
            }
        )),
        "{:?}",
        report.findings
    );
    // ...and it did not stop there: the bitmap comparison still ran.
    assert!(report.summary.allocated > 0);
}

#[test]
fn validate_counts_extension_and_ofs_data_blocks() {
    for variant in [Variant::FfsIntl, Variant::OfsIntl] {
        let mut b = Builder::new(variant, 512, 400, b"Big");
        let root = b.root_lba();
        let f = b.add_file(root, b"BigFile", b"", &pattern(BIG));
        b.add_bitmap(true);
        let mut vol = Volume::open(b.finish(), None).unwrap();
        let report = vol.validate();
        assert!(report.is_clean(), "{variant:?}: {:?}", report.findings);
        assert_eq!(report.summary.extension_blocks, 1);
        assert_eq!(report.summary.data_blocks as usize, f.data.len());
    }

    // An OFS data block whose own header disagrees with the table is a
    // finding; the FFS equivalent cannot exist, because there is no
    // header to disagree.
    let mut b = Builder::new(Variant::OfsIntl, 512, 400, b"Checked");
    let root = b.root_lba();
    let f = b.add_file(root, b"Payload", b"", &pattern(3000));
    b.poke(f.data[1], OFF_DATA_SEQ, 7);
    b.add_bitmap(true);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    let report = vol.validate();
    assert!(report.findings.iter().any(|f| matches!(
        f,
        Finding::Unreadable {
            error: Error::DataBlockSequence { found: 7, .. },
            ..
        }
    )));
}

#[test]
fn validate_refuses_to_report_the_same_block_twice_over() {
    // Two directory entries pointing at one header block: whichever is
    // wrong, one of them is writing over the other.
    let mut b = Builder::new(Variant::FfsIntl, 512, 512, b"Shared");
    let root = b.root_lba();
    let one = b.add(root, b"One", b"", EntryKind::File, 0);
    let two = b.add(root, b"Two", b"", EntryKind::File, 0);
    // Point Two's slot at One's block as well.
    let slot = name_hash(b"Two", Variant::FfsIntl.fold(), hash_table_size(512)) as usize;
    b.poke(root, OFF_HASH_TABLE + slot * 4, one as u32);
    b.add_bitmap(true);
    let mut vol = Volume::open(b.finish(), None).unwrap();
    let report = vol.validate();
    assert!(
        report
            .findings
            .iter()
            .any(|f| matches!(f, Finding::DoublyReachable { lba, .. } if *lba == one)),
        "{:?}",
        report.findings
    );
    // Two is now unreachable and its block leaks.
    assert!(report
        .findings
        .iter()
        .any(|f| matches!(f, Finding::OrphanBlock { lba } if *lba == two)));
}

#[test]
fn findings_display_with_the_consequence_stated() {
    let f: Finding<MemError> = Finding::ReachableButFree { lba: 42 };
    assert!(f.to_string().contains("hand it out twice"));
    let f: Finding<MemError> = Finding::OrphanBlock { lba: 42 };
    assert!(f.to_string().contains("leaked"));
    let f: Finding<MemError> = Finding::BitmapInvalid;
    assert!(f.to_string().contains("must not be trusted"));
    let f: Finding<MemError> = Finding::DircacheStale {
        dir: 880,
        block: 866,
        detail: DircacheDiscrepancy::Missing { entry: 900 },
    };
    assert!(f.to_string().contains("entry 900 is not cached"));
    let f: Finding<MemError> = Finding::Unreadable {
        lba: 5,
        error: Error::ChainCycle { lba: 5 },
    };
    assert!(f.to_string().contains("revisits block 5"));
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Every variant, for a matrix that must not quietly skip one.
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

/// Format an image and assert the thing every other assertion depends on:
/// it opens as the variant asked for, it validates with **zero**
/// findings, its root is empty, and the bitmap marks exactly the blocks
/// the layout says it allocated and not one more.
///
/// A helper rather than a test so that every cell of the matrix below
/// gets the same scrutiny — the failure a formatter has is never in the
/// case somebody wrote a bespoke assertion for.
fn format_and_check(variant: Variant, bs: usize, nblocks: u64, name: &[u8]) -> FormatLayout {
    // Pre-filled with 0xA5: any field the formatter fails to write shows
    // up as garbage rather than as a plausible zero.
    let mut disk = MemDisk::filled(bs, nblocks, 0xA5);
    let created = DateStamp {
        days: 9000,
        mins: 611,
        ticks: 2999,
    };
    let opts = FormatOptions::new(variant, nblocks, name).created(created);
    let layout = amiga_ffs::format(&mut disk, &opts).expect("format");

    let mut vol = Volume::open_with(disk, None, nblocks, 2).expect("open");
    assert_eq!(vol.variant(), variant, "{variant:?} @{bs}");
    assert_eq!(vol.root().name, name, "{variant:?} @{bs}");
    assert_eq!(vol.root_lba(), layout.root_lba);
    assert_eq!(vol.root().bitmap_flag, -1);
    assert_eq!(vol.root().dir_altered, created);
    assert_eq!(vol.root().disk_altered, created);
    assert_eq!(vol.root().disk_made, created);
    assert_eq!(
        vol.root().fs_type,
        variant.has_long_names().then(|| variant.dostype()),
        "{variant:?}: the FileSystemType longword is LNFS-only"
    );
    assert_eq!(
        vol.root().blocks_used,
        variant
            .has_long_names()
            .then(|| layout.blocks_used() as u32),
        "{variant:?}: NumBlocksUsed is LNFS-only"
    );

    // An empty volume: no entries, and every hash slot zero.
    let root = vol.root_lba();
    assert!(vol.read_dir(root).unwrap().is_empty(), "{variant:?} @{bs}");
    assert!(vol.root().hash_table.iter().all(|&s| s == 0));
    assert!(vol.lookup(root, b"anything").unwrap().is_none());

    // The dircache decision, asserted in both directions.
    assert_eq!(
        layout.dircache.is_some(),
        variant.has_dircache(),
        "{variant:?}: a dircache volume carries a cache from birth"
    );
    if variant.has_dircache() {
        let cache = vol.read_dircache(root).unwrap();
        assert_eq!(cache.blocks, vec![layout.dircache.unwrap()]);
        assert!(cache.records.is_empty());
    }

    // The bitmap: allocated is exactly the layout's set, free is
    // everything else the bitmap covers, and the counts are exact.
    let bm = vol.read_bitmap().unwrap();
    assert!(bm.valid());
    assert!(bm.covers_whole_volume(), "{variant:?} @{bs}");
    assert_eq!(bm.pages(), layout.bitmap_pages, "{variant:?} @{bs}");
    assert_eq!(bm.ext_blocks(), layout.bitmap_ext, "{variant:?} @{bs}");
    assert_eq!(
        bm.allocated().collect::<Vec<_>>(),
        layout.allocated(),
        "{variant:?} @{bs}"
    );
    assert_eq!(bm.allocated_count(), layout.blocks_used());
    assert_eq!(bm.allocated_count() + bm.free_count(), nblocks - 2);
    for lba in 0..2 {
        assert_eq!(bm.is_allocated(lba), None, "reserved blocks have no bit");
    }

    // And the whole-volume walk: no findings at all, which is the single
    // statement this whole helper exists to make.
    let report = vol.validate();
    assert!(
        report.is_clean(),
        "{variant:?} @{bs}: {:#?}",
        report
            .findings
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
    );
    let s = report.summary;
    assert_eq!(s.directories, 1);
    assert_eq!(s.files, 0);
    assert_eq!(s.reachable, layout.blocks_used());
    assert_eq!(s.allocated, layout.blocks_used());
    assert_eq!(s.orphans, 0);
    assert_eq!(s.reachable_but_free, 0);
    layout
}

#[test]
fn every_variant_formats_at_every_block_size() {
    for variant in ALL_VARIANTS {
        for bs in [512usize, 1024, 4096] {
            // ~900 KB of volume whatever the block size, so the bitmap is
            // one page at 512 and still one at 4 KB.
            let nblocks = 1_800_000 / bs as u64;
            format_and_check(variant, bs, nblocks, b"Formatted");
        }
    }
}

#[test]
fn a_32k_block_volume_formats_too() {
    // The largest block size the format has, where the hash table is 8136
    // slots and one bitmap page covers 262 016 blocks. Once, because the
    // image is 16 MB of test memory.
    format_and_check(Variant::FfsIntlLongname, 32768, 512, b"Big Blocks");
    format_and_check(Variant::OfsIntlDircache, 32768, 512, b"Big Blocks");
}

#[test]
fn a_volume_that_needs_bitmap_extension_blocks_gets_them() {
    // 400 MB at 512 bytes: 202 pages, so the root's 25 pointers are not
    // enough and two extension blocks carry the other 177. The exact
    // layout an image xdftool formatted has.
    let layout = format_and_check(Variant::FfsIntl, 512, 819_200, b"Extended");
    assert_eq!(layout.bitmap_ext.len(), 2);
    assert_eq!(layout.bitmap_pages.len(), 202);
    assert_eq!(layout.bitmap_ext, vec![409_601, 409_602]);
    assert_eq!(layout.bitmap_pages[0], 409_603);
}

#[test]
fn the_smallest_possible_volume_still_formats() {
    // Root, one bitmap page, two reserved blocks: four is the floor for a
    // variant without a dircache, five with one.
    format_and_check(Variant::Ffs, 512, 4, b"Tiny");
    format_and_check(Variant::FfsIntlDircache, 512, 6, b"Tiny");
}

#[test]
fn the_boot_block_carries_the_dostype_and_no_checksum_by_default() {
    for variant in ALL_VARIANTS {
        let mut disk = MemDisk::filled(512, 1760, 0xA5);
        let opts = FormatOptions::new(variant, 1760, b"Booted");
        let layout = amiga_ffs::format(&mut disk, &opts).unwrap();

        let boot = disk.block(0);
        assert_eq!(be32(boot, 0), variant.dostype());
        // Deliberately zero: a boot block that checksums and holds no
        // code is one the ROM accepts and jumps into. `Format` leaves it
        // for `Install`, and so does xdftool.
        assert_eq!(be32(boot, 4), 0, "{variant:?}");
        assert_eq!(be32(boot, 8), layout.root_lba as u32);
        assert!(boot[12..].iter().all(|&b| b == 0), "no boot code");
        // The second reserved block is written too -- zeroed, not left as
        // whatever the image held.
        assert!(disk.block(1).iter().all(|&b| b == 0));

        // Opting in produces a checksum that verifies over the 1024-byte
        // boot area, which at 512-byte blocks spans blocks 0 and 1.
        let mut disk = MemDisk::filled(512, 1760, 0xA5);
        let opts = FormatOptions {
            boot_checksum: true,
            ..FormatOptions::new(variant, 1760, b"Booted")
        };
        amiga_ffs::format(&mut disk, &opts).unwrap();
        let mut area = disk.block(0).to_vec();
        area.extend_from_slice(disk.block(1));
        assert_eq!(area.len(), BOOT_AREA_LEN);
        let mut sum: u32 = 0;
        for off in (0..BOOT_AREA_LEN).step_by(4) {
            let (s, carry) = sum.overflowing_add(be32(&area, off));
            sum = s.wrapping_add(carry as u32);
        }
        assert_eq!(sum, 0xFFFF_FFFF, "{variant:?}: end-around-carry sum");
    }
}

#[test]
fn at_larger_block_sizes_the_boot_area_lives_inside_block_zero() {
    // The boot area is two 512-byte *sectors* -- what the ROM reads --
    // which is a fixed 1024 bytes and not two filesystem blocks. At 4 KB
    // it is the front of block 0, and block 1 is just another zeroed
    // reserved block.
    let mut disk = MemDisk::filled(4096, 440, 0xA5);
    let opts = FormatOptions {
        boot_checksum: true,
        ..FormatOptions::new(Variant::FfsIntl, 440, b"Wide")
    };
    amiga_ffs::format(&mut disk, &opts).unwrap();
    let boot = disk.block(0).to_vec();
    let mut sum: u32 = 0;
    for off in (0..BOOT_AREA_LEN).step_by(4) {
        let (s, carry) = sum.overflowing_add(be32(&boot, off));
        sum = s.wrapping_add(carry as u32);
    }
    assert_eq!(sum, 0xFFFF_FFFF);
    assert!(boot[BOOT_AREA_LEN..].iter().all(|&b| b == 0));
    assert!(disk.block(1).iter().all(|&b| b == 0));
}

#[test]
fn format_leaves_free_blocks_exactly_as_it_found_them() {
    // Formatting a 2 GB image must not write 2 GB. Everything outside the
    // layout keeps the byte it had, and is marked free.
    let mut disk = MemDisk::filled(512, 1760, 0xA5);
    let opts = FormatOptions::new(Variant::FfsIntl, 1760, b"Untouched");
    let layout = amiga_ffs::format(&mut disk, &opts).unwrap();
    let written: Vec<u64> = layout.allocated();
    for lba in 2..1760u64 {
        if !written.contains(&lba) {
            assert!(
                disk.block(lba).iter().all(|&b| b == 0xA5),
                "block {lba} was written for no reason"
            );
        }
    }
}

#[test]
fn format_refuses_what_it_cannot_write() {
    let mut disk = MemDisk::blank(512, 1760);

    let bad_name = |name: &[u8]| {
        let opts = FormatOptions::new(Variant::Ffs, 1760, name);
        let mut d = MemDisk::blank(512, 1760);
        amiga_ffs::format(&mut d, &opts).unwrap_err()
    };
    assert!(matches!(bad_name(b""), FormatError::NameEmpty));
    assert!(matches!(
        bad_name(&[b'x'; 31]),
        FormatError::NameTooLong { len: 31, max: 30 }
    ));
    // 30 bytes is the limit on *every* variant: the root's name field did
    // not move on LNFS volumes, only directory entries' did.
    let opts = FormatOptions::new(Variant::FfsIntlLongname, 1760, &[b'x'; 31]);
    assert!(matches!(
        amiga_ffs::format(&mut MemDisk::blank(512, 1760), &opts).unwrap_err(),
        FormatError::NameTooLong { .. }
    ));
    assert!(matches!(
        bad_name(b"Work:bench"),
        FormatError::NameInvalidByte {
            byte: b':',
            index: 4
        }
    ));
    assert!(matches!(
        bad_name(b"a/b"),
        FormatError::NameInvalidByte { byte: b'/', .. }
    ));

    // Geometry.
    let opts = FormatOptions::new(Variant::Ffs, 1760, b"Vol").reserved(1760);
    assert!(matches!(
        amiga_ffs::format(&mut disk, &opts).unwrap_err(),
        FormatError::BadReserved { .. }
    ));
    let opts = FormatOptions::new(Variant::Ffs, 3, b"Vol");
    assert!(matches!(
        amiga_ffs::format(&mut disk, &opts).unwrap_err(),
        FormatError::VolumeTooSmall { needed: 4, .. }
    ));
    // The sink knows it is too small, and says so before block one.
    let opts = FormatOptions::new(Variant::Ffs, 4000, b"Vol");
    assert!(matches!(
        amiga_ffs::format(&mut disk, &opts).unwrap_err(),
        FormatError::SinkTooSmall {
            block_count: 4000,
            sink_blocks: 1760
        }
    ));
    // And nothing was written while refusing any of that.
    assert!(disk.block(880).iter().all(|&b| b == 0));

    // A volume no 32-bit block pointer could name, refused before the
    // sink is even asked how big it is.
    let opts = FormatOptions::new(Variant::Ffs, 1 << 33, b"Vol");
    assert!(matches!(
        amiga_ffs::format(&mut disk, &opts).unwrap_err(),
        FormatError::VolumeTooLarge { .. }
    ));

    // A block size the format has no hash-table size for.
    let mut odd = MemDisk::blank(256, 64);
    let opts = FormatOptions::new(Variant::Ffs, 64, b"Vol");
    assert!(matches!(
        amiga_ffs::format(&mut odd, &opts).unwrap_err(),
        FormatError::BadBlockSize(256)
    ));
}

#[test]
fn a_formatted_volume_is_the_shape_the_oracle_writes() {
    // The block numbers a `DOS\1` ADF xdftool formats actually contains:
    // root 880, one bitmap page at 881, the boot block naming the root.
    // Asserted here as well as in the differential suite so the layout
    // decision is pinned even where amitools is not installed.
    let mut disk = MemDisk::blank(512, 1760);
    let opts = FormatOptions::new(Variant::Ffs, 1760, b"Empty");
    let layout = amiga_ffs::format(&mut disk, &opts).unwrap();
    assert_eq!(layout.root_lba, 880);
    assert_eq!(layout.bitmap_pages, vec![881]);
    assert_eq!(layout.blocks_used(), 2);

    let root = disk.block(880);
    assert_eq!(be32(root, OFF_TYPE), T_HEADER);
    assert_eq!(be32(root, OFF_OWN_KEY), 0);
    assert_eq!(be32(root, OFF_HASH_TABLE_SIZE), 72);
    assert!(checksum_ok(root));
    assert_eq!(be32(root, tail(512, TL_BITMAP_FLAG)) as i32, -1);
    assert_eq!(be32(root, tail(512, TL_BITMAP_PAGES)), 881);
    assert_eq!(be32(root, tail(512, TL_BITMAP_EXT)), 0);
    assert_eq!(be32(root, tail(512, TL_SECONDARY_TYPE)) as i32, ST_ROOT);

    // The bitmap page: checksum at longword 0, 1 = free, and exactly two
    // bits clear.
    let page = disk.block(881);
    assert!(checksum_ok(page));
    let clear: Vec<u64> = (0..1758u64)
        .filter(|i| {
            let w = be32(page, OFF_BITMAP_BITS + (*i as usize / 32) * 4);
            w >> (i % 32) & 1 == 0
        })
        .map(|i| i + 2)
        .collect();
    assert_eq!(clear, vec![880, 881]);
}
