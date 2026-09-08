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
    /// The medium stopped accepting writes: what a crash looks like from
    /// inside the crate, injected by [`MemDisk::fail_after`].
    Crashed { lba: u64 },
}

impl std::fmt::Display for MemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadBufferLen { got, want } => write!(f, "buffer of {got} bytes, want {want}"),
            Self::OutOfRange { lba, blocks } => write!(f, "lba {lba} of {blocks}"),
            Self::Crashed { lba } => write!(f, "the medium died writing lba {lba}"),
        }
    }
}

impl std::error::Error for MemError {}

#[derive(Clone)]
pub struct MemDisk {
    bs: usize,
    data: Vec<u8>,
    /// Every block written since the log was last cleared, in order —
    /// so a test can assert *which* blocks a flush touched, not merely
    /// that the result is right.
    writes: Vec<u64>,
    /// Every block *read* since the log was last cleared. A ranged read's
    /// whole claim is that it touches only the blocks the range covers,
    /// and counting them is the only way to assert it — the bytes come
    /// back right either way.
    reads: Vec<u64>,
    /// Refuse every write after this many. A crash, made deterministic:
    /// stepping it from 0 upward replays every prefix of a write
    /// sequence, which is the only way to check a crash-ordering claim
    /// at every point rather than at one convenient one.
    fail_after: Option<usize>,
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
        self.reads.push(lba);
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
        if let Some(n) = self.fail_after {
            if self.writes.len() >= n {
                return Err(MemError::Crashed { lba });
            }
        }
        self.writes.push(lba);
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
            writes: Vec::new(),
            reads: Vec::new(),
            fail_after: None,
        }
    }

    /// An image full of a non-zero byte, so a test can tell "the
    /// formatter wrote this field" from "the field was already zero".
    pub fn filled(bs: usize, nblocks: u64, byte: u8) -> Self {
        Self {
            bs,
            data: vec![byte; bs * nblocks as usize],
            writes: Vec::new(),
            reads: Vec::new(),
            fail_after: None,
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

    /// The blocks written since the last [`MemDisk::clear_log`], in
    /// order.
    pub fn write_log(&self) -> &[u64] {
        &self.writes
    }

    pub fn clear_log(&mut self) {
        self.writes.clear();
    }

    /// The blocks read since the last [`MemDisk::clear_read_log`], in
    /// order.
    pub fn read_log(&self) -> &[u64] {
        &self.reads
    }

    pub fn clear_read_log(&mut self) {
        self.reads.clear();
    }

    /// Start refusing writes once `n` more have been accepted.
    pub fn fail_after(&mut self, n: usize) {
        self.fail_after = Some(self.writes.len() + n);
    }

    /// The medium is fixed: stop refusing writes. What a crash-sweep test
    /// does before asking a repair to finish the job the interrupted
    /// write left behind — the disk itself is not still failing, only the
    /// operation that was running on it was interrupted once.
    pub fn stop_failing(&mut self) {
        self.fail_after = None;
    }

    /// Enlarge the medium to at least `new_blocks` blocks, zero-filled —
    /// what a partition table's `high_cyl` growing looks like from this
    /// crate's side of the seam, and the precondition
    /// [`amiga_ffs::Volume::resize`] documents for growing: the medium
    /// already reports the new size before the filesystem is asked to.
    pub fn grow(&mut self, new_blocks: u64) {
        let want = new_blocks as usize * self.bs;
        if want > self.data.len() {
            self.data.resize(want, 0);
        }
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
            writes: Vec::new(),
            reads: Vec::new(),
            fail_after: None,
        }
    }

    pub fn finish_without_checksums(self) -> MemDisk {
        MemDisk {
            bs: self.bs,
            data: self.data,
            writes: Vec::new(),
            reads: Vec::new(),
            fail_after: None,
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

// ---------------------------------------------------------------------------
// Populating
// ---------------------------------------------------------------------------

/// A seeded xorshift64*, so every tree below is the same tree on every
/// machine and a failure names a case somebody can reproduce.
///
/// No dependency, on purpose: a crate whose selling point is having none
/// does not acquire one for a test's random numbers, and a generator
/// whose sequence is fixed here is a generator that cannot change under a
/// `cargo update`.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform enough in `0..n` for generating test data.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// The tree a round trip is checked against: what was asked for, kept
/// separately from what the volume then contains.
#[derive(Debug, Clone)]
enum Node {
    Dir {
        name: Vec<u8>,
        meta: OwnedMeta,
        children: Vec<Node>,
    },
    File {
        name: Vec<u8>,
        meta: OwnedMeta,
        data: Vec<u8>,
    },
}

#[derive(Debug, Clone, Default)]
struct OwnedMeta {
    protection: u32,
    comment: Vec<u8>,
    date: DateStamp,
    owner: u32,
}

impl OwnedMeta {
    fn borrow(&self) -> amiga_ffs::Metadata<'_> {
        amiga_ffs::Metadata::new()
            .protection(self.protection)
            .comment(&self.comment)
            .date(self.date)
            .owner(self.owner)
    }
}

impl Node {
    fn name(&self) -> &[u8] {
        match self {
            Self::Dir { name, .. } | Self::File { name, .. } => name,
        }
    }
}

/// Every byte a name may legally hold, including the Latin-1 half that
/// only the intl fold table folds — which is exactly where a volume
/// written with the wrong `toupper` stops finding its own files.
const NAME_ALPHABET: &[u8] =
    b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .-_\xE9\xC9\xE0\xFF\xDF";

fn random_name(rng: &mut Rng, max: usize, used: &[Vec<u8>], fold: fn(u8) -> u8) -> Vec<u8> {
    loop {
        // Weighted towards short names, but reaching the variant's limit
        // often enough to matter: on LNFS that means names past 30, the
        // ones a classic-offset writer cannot store at all.
        let len = 1 + rng.below(max);
        let name: Vec<u8> = (0..len)
            .map(|_| NAME_ALPHABET[rng.below(NAME_ALPHABET.len())])
            .collect();
        // A trailing space is legal on disk but not worth the argument;
        // duplicates under the volume's own fold are refused by the
        // writer, so the generator must not produce them.
        if name.last() == Some(&b' ') || name.first() == Some(&b' ') {
            continue;
        }
        if used.iter().any(|u| names_equal(u, &name, fold)) {
            continue;
        }
        return name;
    }
}

fn random_meta(rng: &mut Rng, max_comment: usize) -> OwnedMeta {
    let clen = rng.below(max_comment + 1);
    OwnedMeta {
        // The whole longword, group and other nibbles included: they are
        // first-class on every variant here, so a writer that dropped
        // them would be caught.
        protection: rng.next() as u32 & 0x00FF_FFFF,
        comment: (0..clen)
            .map(|_| NAME_ALPHABET[rng.below(NAME_ALPHABET.len())])
            .collect(),
        date: DateStamp {
            days: (rng.below(20_000)) as u32,
            mins: (rng.below(1440)) as u32,
            ticks: (rng.below(3000)) as u32,
        },
        owner: rng.next() as u32,
    }
}

/// Grow a tree with a byte budget, so the same generator produces a tree
/// that fits at 512 bytes a block and at 4096.
fn random_tree(rng: &mut Rng, variant: Variant, depth: usize, budget: &mut usize) -> Vec<Node> {
    let max_name = if variant.has_long_names() {
        MAX_NAME_LONG
    } else {
        MAX_NAME_CLASSIC
    };
    let fold = variant.fold();
    let mut used: Vec<Vec<u8>> = Vec::new();
    let mut out = Vec::new();
    let count = 1 + rng.below(if depth == 0 { 6 } else { 4 });
    for _ in 0..count {
        let name = random_name(rng, max_name, &used, fold);
        used.push(name.clone());
        let meta = random_meta(rng, COMMENT_MAX);
        if depth < 3 && rng.below(3) == 0 {
            let children = random_tree(rng, variant, depth + 1, budget);
            out.push(Node::Dir {
                name,
                meta,
                children,
            });
        } else {
            // Sizes reaching ~100 KB: enough to cross an extension block
            // at 512 and 1024 bytes a block, and to include the empty
            // file and the one-byte file that are their own edge cases.
            let want = match rng.below(8) {
                0 => 0,
                1 => 1,
                2 => 1 + rng.below(600),
                3..=5 => rng.below(20_000),
                _ => 40_000 + rng.below(60_000),
            };
            let len = want.min(*budget);
            *budget -= len;
            out.push(Node::File {
                name,
                meta,
                data: pattern(len),
            });
        }
    }
    out
}

fn write_tree<S: amiga_ffs::BlockMedium>(pop: &mut Populator<S>, dir: u64, nodes: &[Node])
where
    amiga_ffs::populate::Transport<S>: std::fmt::Debug,
{
    for node in nodes {
        match node {
            Node::Dir {
                name,
                meta,
                children,
            } => {
                let lba = pop
                    .create_dir(dir, name, &meta.borrow())
                    .unwrap_or_else(|e| panic!("create_dir {:?}: {e:?}", Latin1(name)));
                write_tree(pop, lba, children);
            }
            Node::File { name, meta, data } => {
                pop.create_file(dir, name, &meta.borrow(), data)
                    .unwrap_or_else(|e| panic!("create_file {:?}: {e:?}", Latin1(name)));
            }
        }
    }
}

/// Latin-1 bytes in a panic message, escaped rather than lossily decoded.
struct Latin1<'a>(&'a [u8]);

impl std::fmt::Debug for Latin1<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(self.0))
    }
}

/// Read the tree back and compare it, field by field, to what was asked
/// for. Names are compared **byte for byte**, not under the fold table:
/// case is preserved on disk, and a writer that upper-cased what it
/// stored would still pass every lookup.
fn check_tree(vol: &mut Volume<MemDisk>, dir: u64, nodes: &[Node], path: &str) {
    let listed = vol.read_dir(dir).expect("read_dir");
    assert_eq!(
        listed.len(),
        nodes.len(),
        "{path}: entry count ({:?} vs asked for {:?})",
        listed.iter().map(|e| Latin1(&e.name)).collect::<Vec<_>>(),
        nodes.iter().map(|n| Latin1(n.name())).collect::<Vec<_>>()
    );

    for node in nodes {
        let name = node.name();
        let entry = vol
            .lookup(dir, name)
            .expect("lookup")
            .unwrap_or_else(|| panic!("{path}: {:?} not found by hash lookup", Latin1(name)));
        assert_eq!(entry.name, name, "{path}: name is not case-preserved");
        assert_eq!(entry.parent as u64, dir, "{path}/{:?}", Latin1(name));

        let (meta, kind) = match node {
            Node::Dir { meta, .. } => (meta, EntryKind::Directory),
            Node::File { meta, .. } => (meta, EntryKind::File),
        };
        assert_eq!(entry.kind, kind, "{path}/{:?}", Latin1(name));
        assert_eq!(
            entry.protection,
            meta.protection,
            "{path}/{:?} protection",
            Latin1(name)
        );
        assert_eq!(entry.owner, meta.owner, "{path}/{:?} owner", Latin1(name));
        assert_eq!(entry.date, meta.date, "{path}/{:?} date", Latin1(name));
        // Through `comment()`, which is the only accessor that is right
        // in both places a comment can live -- an empty inline comment on
        // an LNFS volume does not mean there isn't one.
        assert_eq!(
            vol.comment(&entry).expect("comment"),
            meta.comment,
            "{path}/{:?} comment",
            Latin1(name)
        );

        let sub = format!("{path}/{}", String::from_utf8_lossy(name));
        match node {
            Node::Dir { children, .. } => check_tree(vol, entry.lba, children, &sub),
            Node::File { data, .. } => {
                assert_eq!(entry.byte_size as usize, data.len(), "{sub} size");
                assert_eq!(&vol.read_file(entry.lba).expect("read_file"), data, "{sub}");
            }
        }
    }
}

fn biggest_file(nodes: &[Node]) -> usize {
    nodes
        .iter()
        .map(|n| match n {
            Node::Dir { children, .. } => biggest_file(children),
            Node::File { data, .. } => data.len(),
        })
        .max()
        .unwrap_or(0)
}

fn count_nodes(nodes: &[Node]) -> (u64, u64) {
    let (mut dirs, mut files) = (0, 0);
    for node in nodes {
        match node {
            Node::Dir { children, .. } => {
                dirs += 1;
                let (d, f) = count_nodes(children);
                dirs += d;
                files += f;
            }
            Node::File { .. } => files += 1,
        }
    }
    (dirs, files)
}

/// The property this whole milestone is for: for every variant at every
/// block size, a tree written by this crate reads back identical and the
/// volume validates with zero findings.
#[test]
fn a_populated_volume_round_trips_on_every_variant_and_block_size() {
    for (v, variant) in ALL_VARIANTS.into_iter().enumerate() {
        for (b, bs) in [512usize, 1024, 4096].into_iter().enumerate() {
            // 8 MB of volume whatever the block size, so a 100 KB file is
            // never the thing that runs it out of space.
            let nblocks = 8 * 1024 * 1024 / bs as u64;
            let mut rng = Rng::new(0x5EED_0000 + (v * 16 + b) as u64);
            let mut budget = 700_000usize;
            let tree = random_tree(&mut rng, variant, 0, &mut budget);

            let disk = MemDisk::filled(bs, nblocks, 0xA5);
            let opts = FormatOptions::new(variant, nblocks, b"Populated");
            let mut pop = Populator::new(disk, &opts).expect("populate");
            let root = pop.root_lba();
            write_tree(&mut pop, root, &tree);
            let used = pop.blocks_used();
            let disk = pop.finish().expect("finish");

            let mut vol = Volume::open_with(disk, None, nblocks, 2).expect("open");
            assert_eq!(vol.variant(), variant);
            check_tree(&mut vol, root, &tree, "");

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
            let (dirs, files) = count_nodes(&tree);
            let s = report.summary;
            assert_eq!(s.directories, dirs + 1, "{variant:?} @{bs}");
            assert_eq!(s.files, files, "{variant:?} @{bs}");
            // Reachable, allocated and the populator's own count are three
            // independent tallies of the same fact.
            assert_eq!(s.reachable, s.allocated, "{variant:?} @{bs}");
            assert_eq!(s.allocated, used, "{variant:?} @{bs}");
            assert_eq!(s.orphans, 0);
            assert_eq!(s.reachable_but_free, 0);
            assert_eq!(
                s.dircache_blocks > 0,
                variant.has_dircache(),
                "{variant:?} @{bs}"
            );
            // Extension blocks appear exactly when a file outgrows one
            // header's pointer table -- 36 KB of FFS data at 512 bytes a
            // block, 3.9 MB at 4096 -- so the assertion is the arithmetic,
            // not a constant. It is here rather than at a fixed block size
            // because "the writer never needed an extension block" is the
            // way this test would silently stop testing them.
            let table_span = hash_table_size(bs) as usize * data_payload_size(bs, variant.is_ffs());
            assert_eq!(
                s.extension_blocks >= 1,
                biggest_file(&tree) > table_span,
                "{variant:?} @{bs}: {} extension blocks for a {}-byte file, table span {table_span}",
                s.extension_blocks,
                biggest_file(&tree)
            );

            // And the bitmap the single `finish()` pass wrote is valid
            // again -- the flag was 0 for the whole session.
            let bm = vol.read_bitmap().unwrap();
            assert!(bm.valid());
            assert!(bm.covers_whole_volume());
            assert_eq!(bm.allocated_count(), used);
        }
    }
}

/// While a populator is running the volume says so: the bitmap flag is 0,
/// which is what stops anything else allocating out from under it.
#[test]
fn an_unfinished_populate_leaves_the_bitmap_marked_untrustworthy() {
    let nblocks = 1760;
    let disk = MemDisk::blank(512, nblocks);
    let opts = FormatOptions::new(Variant::FfsIntl, nblocks, b"Interrupted");
    let mut pop = Populator::new(disk, &opts).unwrap();
    let root = pop.root_lba();
    pop.create_file(root, b"Half", &amiga_ffs::Metadata::new(), &pattern(5000))
        .unwrap();
    // Abandoned, not finished: what an interrupted image build leaves.
    let disk = pop.abandon();

    let mut vol = Volume::open_with(disk, None, nblocks, 2).unwrap();
    let bm = vol.read_bitmap().unwrap();
    assert!(
        !bm.valid(),
        "an interrupted populate must not claim a valid bitmap"
    );
    // The file itself is entirely there -- the data went down before
    // anything pointed at it, and the header before the parent's slot.
    let half = vol.lookup(root, b"Half").unwrap().unwrap().lba;
    assert_eq!(vol.read_file(half).unwrap(), pattern(5000));
    let report = vol.validate();
    // The one finding is the flag itself; the allocated/free comparison
    // that would otherwise produce noise is skipped, because comparing a
    // trustworthy walk against an untrusted bitmap means nothing.
    assert_eq!(
        report.findings.len(),
        1,
        "{:#?}",
        report
            .findings
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
    );
    assert!(matches!(report.findings[0], Finding::BitmapInvalid));
}

/// A helper for the targeted cases below: a small volume, populated by a
/// closure, finished, and reopened.
fn populated(
    variant: Variant,
    bs: usize,
    nblocks: u64,
    build: impl FnOnce(&mut Populator<MemDisk>),
) -> Volume<MemDisk> {
    let disk = MemDisk::filled(bs, nblocks, 0xA5);
    let opts = FormatOptions::new(variant, nblocks, b"Target");
    let mut pop = Populator::new(disk, &opts).expect("format");
    build(&mut pop);
    let disk = pop.finish().expect("finish");
    Volume::open_with(disk, None, nblocks, 2).expect("open")
}

/// Head insertion, matching the oracle. Three names that hash to the same
/// slot, written in order: xdftool's `DOS\3` ADF has the *last* one in the
/// root's hash slot and the first at the end of the chain, and so does
/// this crate's.
#[test]
fn a_new_entry_goes_in_at_the_head_of_its_chain_as_the_oracle_does() {
    // AAA, AEU and AFH all hash to slot 54 of a 72-slot table.
    let t = hash_table_size(512);
    for n in [&b"AAA"[..], b"AEU", b"AFH"] {
        assert_eq!(name_hash(n, intl_toupper, t), 54, "{:?}", n);
    }

    let mut vol = populated(Variant::FfsIntl, 512, 1760, |pop| {
        let root = pop.root_lba();
        for name in [&b"AAA"[..], b"AEU", b"AFH"] {
            pop.create_file(root, name, &amiga_ffs::Metadata::new(), b"a")
                .unwrap();
        }
    });

    let root = vol.root_lba();
    let head = vol.root().hash_table[54];
    let mut order = Vec::new();
    let mut next = head;
    while next != 0 {
        let e = vol.entry_at(next as u64).unwrap();
        order.push(e.name.clone());
        next = e.hash_chain;
    }
    assert_eq!(
        order,
        vec![b"AFH".to_vec(), b"AEU".to_vec(), b"AAA".to_vec()],
        "newest first, as xdftool's own chains come out"
    );
    // All three are still findable by name, which is the only thing the
    // chain order has to preserve.
    for name in [&b"AAA"[..], b"AEU", b"AFH"] {
        assert!(vol.lookup(root, name).unwrap().is_some());
    }
    assert!(vol.validate().is_clean());
}

/// The LNFS trap, from the writing side: a name past 30 bytes goes in the
/// merged `NaC` field, and a comment that will not fit beside it goes to
/// its own `T_COMMENT` block with the inline copy left empty.
#[test]
fn an_lnfs_long_name_and_an_overflowing_comment_land_where_the_format_puts_them() {
    let name: Vec<u8> = (0..100).map(|i| b'a' + (i % 26) as u8).collect();
    let comment: Vec<u8> = (0..COMMENT_MAX).map(|i| b'A' + (i % 26) as u8).collect();
    assert!(1 + name.len() + 1 + comment.len() > NAC_LEN);

    let mut vol = populated(Variant::FfsIntlLongname, 512, 1760, |pop| {
        let root = pop.root_lba();
        let meta = amiga_ffs::Metadata::new().comment(&comment);
        pop.create_file(root, &name, &meta, b"contents").unwrap();
        // A short comment beside a short name stays inline, which is the
        // other half of the decision.
        let meta = amiga_ffs::Metadata::new().comment(b"inline");
        pop.create_dir(root, b"Short", &meta).unwrap();
    });

    let root = vol.root_lba();
    let long = vol.lookup(root, &name).unwrap().expect("found by hash");
    assert_eq!(long.name, name, "the whole 100-byte name is stored");
    assert!(
        long.comment.is_empty() && long.comment_block != 0,
        "an overflowed comment leaves the inline field empty and longword -18 set"
    );
    assert_eq!(vol.comment(&long).unwrap(), comment);
    assert_eq!(vol.read_file(long.lba).unwrap(), b"contents");

    let short = vol.lookup(root, b"Short").unwrap().unwrap();
    assert_eq!(short.comment, b"inline");
    assert_eq!(short.comment_block, 0);
    assert!(vol.validate().is_clean());
}

/// Dircache maintenance on `DOS\4`/`DOS\5`: a record per entry, spilling
/// into a chained block, and the validator finding nothing stale.
#[test]
fn a_dircache_volume_gets_its_caches_written_as_it_is_populated() {
    // Enough entries with long names and comments to need more than one
    // cache block: at 512 bytes a block holds 488 bytes of records.
    let count = 40;
    let mut vol = populated(Variant::FfsIntlDircache, 512, 1760, |pop| {
        let root = pop.root_lba();
        let sub = pop
            .create_dir(root, b"Sub", &amiga_ffs::Metadata::new())
            .unwrap();
        for i in 0..count {
            let name = format!("entry-number-{i:04}");
            let meta = amiga_ffs::Metadata::new().comment(b"a comment of some length");
            pop.create_file(root, name.as_bytes(), &meta, b"x").unwrap();
        }
        pop.create_file(sub, b"Nested", &amiga_ffs::Metadata::new(), b"y")
            .unwrap();
    });

    let root = vol.root_lba();
    let cache = vol.read_dircache(root).unwrap();
    assert!(
        cache.blocks.len() > 1,
        "{} records must spill past one cache block",
        cache.records.len()
    );
    assert_eq!(cache.records.len(), count + 1);
    // A directory created here has its own cache from birth, as one
    // xdftool creates does.
    let sub = vol.lookup(root, b"Sub").unwrap().unwrap();
    assert_ne!(vol.dircache_head(sub.lba).unwrap(), 0);
    assert_eq!(vol.read_dircache(sub.lba).unwrap().records.len(), 1);

    // And the whole-volume walk, which compares every record against the
    // chains six ways, reports nothing.
    let report = vol.validate();
    assert!(
        report.is_clean(),
        "{:#?}",
        report
            .findings
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
    );
    // The record's type byte is filled in, where xdftool leaves it zero.
    assert!(cache
        .records
        .iter()
        .all(|r| r.entry_type == ST_FILE as i8 || r.entry_type == ST_USERDIR as i8));
}

#[test]
fn a_duplicate_name_is_refused_under_the_volumes_own_fold_table() {
    for (variant, second, refused) in [
        // Classic folding is ASCII-only, so `caf\xE9` and `CAF\xC9` are
        // different names on DOS\1 and the same name on DOS\3.
        (Variant::Ffs, &b"CAF\xC9"[..], false),
        (Variant::FfsIntl, &b"CAF\xC9"[..], true),
        (Variant::Ffs, &b"CAF\xE9"[..], true),
    ] {
        let disk = MemDisk::blank(512, 1760);
        let opts = FormatOptions::new(variant, 1760, b"Dupes");
        let mut pop = Populator::new(disk, &opts).unwrap();
        let root = pop.root_lba();
        pop.create_dir(root, b"caf\xE9", &amiga_ffs::Metadata::new())
            .unwrap();
        let again = pop.create_file(root, second, &amiga_ffs::Metadata::new(), b"x");
        assert_eq!(
            again.is_err(),
            refused,
            "{variant:?} + {:?}",
            String::from_utf8_lossy(second)
        );
        if refused {
            assert!(matches!(again, Err(PopulateError::DuplicateName { .. })));
        }
        // Refused *before* anything was written, so the volume is still
        // whole either way.
        let disk = pop.finish().unwrap();
        let mut vol = Volume::open_with(disk, None, 1760, 2).unwrap();
        assert!(vol.validate().is_clean());
    }
}

#[test]
fn names_and_comments_are_checked_per_variant() {
    let disk = MemDisk::blank(512, 1760);
    let opts = FormatOptions::new(Variant::FfsIntl, 1760, b"Checked");
    let mut pop = Populator::new(disk, &opts).unwrap();
    let root = pop.root_lba();
    let meta = amiga_ffs::Metadata::new();

    assert!(matches!(
        pop.create_dir(root, b"", &meta),
        Err(PopulateError::NameEmpty)
    ));
    assert!(matches!(
        pop.create_dir(root, &[b'x'; 31], &meta),
        Err(PopulateError::NameTooLong { len: 31, max: 30 })
    ));
    for bad in [b':', b'/', 0x00, 0x0A, 0x7F] {
        assert!(matches!(
            pop.create_dir(root, &[b'A', bad], &meta),
            Err(PopulateError::NameInvalidByte { index: 1, .. })
        ));
    }
    // Latin-1 is a name and stays one.
    assert!(pop.create_dir(root, b"Caf\xE9", &meta).is_ok());
    let long = vec![b'c'; COMMENT_MAX + 1];
    assert!(matches!(
        pop.create_dir(
            root,
            b"Commented",
            &amiga_ffs::Metadata::new().comment(&long)
        ),
        Err(PopulateError::CommentTooLong { max: 79, .. })
    ));

    // 107 bytes is a name on DOS\7 and not on DOS\3 -- the one limit that
    // moves with the variant.
    let disk = MemDisk::blank(512, 1760);
    let opts = FormatOptions::new(Variant::FfsIntlLongname, 1760, b"Long");
    let mut pop = Populator::new(disk, &opts).unwrap();
    let root = pop.root_lba();
    assert!(pop.create_dir(root, &[b'x'; MAX_NAME_LONG], &meta).is_ok());
    assert!(matches!(
        pop.create_dir(root, &[b'y'; MAX_NAME_LONG + 1], &meta),
        Err(PopulateError::NameTooLong { max: 107, .. })
    ));
}

#[test]
fn creating_in_something_that_is_not_a_directory_is_refused() {
    let disk = MemDisk::blank(512, 1760);
    let opts = FormatOptions::new(Variant::FfsIntl, 1760, b"NotDir");
    let mut pop = Populator::new(disk, &opts).unwrap();
    let root = pop.root_lba();
    let file = pop
        .create_file(root, b"File", &amiga_ffs::Metadata::new(), b"x")
        .unwrap();
    assert!(matches!(
        pop.create_dir(file, b"Nope", &amiga_ffs::Metadata::new()),
        Err(PopulateError::NotADirectory { .. })
    ));
    assert!(matches!(
        pop.create_dir(9_999, b"Nope", &amiga_ffs::Metadata::new()),
        Err(PopulateError::LbaOutOfRange { .. })
    ));
}

#[test]
fn a_volume_that_runs_out_of_blocks_says_so_rather_than_writing_past_the_end() {
    // The smallest volume that formats at all: root at 2, one bitmap page
    // at 3, and two blocks left over.
    let disk = MemDisk::blank(512, 6);
    let opts = FormatOptions::new(Variant::Ffs, 6, b"Tiny");
    let mut pop = Populator::new(disk, &opts).unwrap();
    let root = pop.root_lba();
    assert_eq!(pop.blocks_free(), 2);
    pop.create_dir(root, b"A", &amiga_ffs::Metadata::new())
        .unwrap();
    assert!(matches!(
        pop.create_file(root, b"B", &amiga_ffs::Metadata::new(), &[0u8; 600]),
        Err(PopulateError::VolumeFull { block_count: 6 })
    ));
}

#[test]
fn the_allocator_uses_the_half_of_the_volume_below_the_root() {
    // The root sits at the midpoint, so an allocator that started after
    // it would throw away half the disk. This one starts at the first
    // block the bitmap covers and steps over the format's own run.
    let mut vol = populated(Variant::Ffs, 512, 1760, |pop| {
        let root = pop.root_lba();
        pop.create_file(root, b"Big", &amiga_ffs::Metadata::new(), &pattern(600_000))
            .unwrap();
    });
    let bm = vol.read_bitmap().unwrap();
    let allocated: Vec<u64> = bm.allocated().collect();
    assert_eq!(
        allocated[0], 2,
        "allocation starts at the first covered block"
    );
    assert!(
        allocated.iter().any(|&b| b > 881),
        "and runs past the format's own blocks"
    );
    assert!(vol.validate().is_clean());
}

#[test]
fn a_deeply_nested_tree_round_trips() {
    let depth = 40;
    let mut vol = populated(Variant::FfsIntlDircache, 512, 1760, |pop| {
        let mut here = pop.root_lba();
        for i in 0..depth {
            here = pop
                .create_dir(
                    here,
                    format!("d{i}").as_bytes(),
                    &amiga_ffs::Metadata::new(),
                )
                .unwrap();
        }
        pop.create_file(here, b"Bottom", &amiga_ffs::Metadata::new(), b"deep")
            .unwrap();
    });
    let root = vol.root_lba();
    let mut path = String::new();
    for i in 0..depth {
        path.push_str(&format!("d{i}/"));
    }
    path.push_str("Bottom");
    let entry = vol.lookup_path(root, path.as_bytes()).unwrap().unwrap();
    assert_eq!(vol.read_file(entry.lba).unwrap(), b"deep");
    assert!(vol.validate().is_clean());
}

/// The `std` layer: a host directory becomes an image, with mtimes and
/// modes mapped and non-Latin-1 names refused by name.
#[test]
#[cfg(feature = "std")]
fn a_host_tree_becomes_a_volume() {
    let dir = std::env::temp_dir().join(format!("amiga-ffs-tree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("S")).unwrap();
    std::fs::write(dir.join("S/Startup-Sequence"), b"Echo \"hi\"\n").unwrap();
    std::fs::write(dir.join("big.dat"), pattern(50_000)).unwrap();
    std::fs::write(dir.join("empty"), b"").unwrap();

    let nblocks = 1760;
    let disk = MemDisk::blank(512, nblocks);
    let opts = FormatOptions::new(Variant::FfsIntl, nblocks, b"FromTree");
    let disk = amiga_ffs::populate::populate_from_tree(disk, &opts, &dir).expect("populate");

    let mut vol = Volume::open_with(disk, None, nblocks, 2).unwrap();
    let root = vol.root_lba();
    let e = vol
        .lookup_path(root, b"S/Startup-Sequence")
        .unwrap()
        .unwrap();
    assert_eq!(vol.read_file(e.lba).unwrap(), b"Echo \"hi\"\n");
    let e = vol.lookup(root, b"big.dat").unwrap().unwrap();
    assert_eq!(vol.read_file(e.lba).unwrap(), pattern(50_000));
    let e = vol.lookup(root, b"empty").unwrap().unwrap();
    assert_eq!(e.byte_size, 0);
    // A host mtime is not the epoch, so the mapping did something.
    assert!(e.date.days > 17_000, "a host mtime maps to a real date");
    assert!(vol.validate().is_clean());

    // A name that is not Latin-1 is refused, and the error names the file.
    std::fs::write(dir.join("\u{4F60}\u{597D}"), b"x").unwrap();
    let disk = MemDisk::blank(512, nblocks);
    let err = match amiga_ffs::populate::populate_from_tree(disk, &opts, &dir) {
        Ok(_) => panic!("a name that is not Latin-1 must be refused"),
        Err(e) => e,
    };
    match &err {
        PopulateError::HostNameNotLatin1 { path } => {
            assert!(path.to_string_lossy().contains('\u{4F60}'));
        }
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The host-metadata mapping, stated as assertions rather than only as a
/// table in the docs: the executable bit clears the E *denial*, and a
/// missing read bit sets the R denial.
#[test]
#[cfg(all(unix, feature = "std"))]
fn the_host_mode_mapping_is_the_one_documented() {
    use amiga_ffs::meta::*;
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("amiga-ffs-modes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("m");
    std::fs::write(&path, b"x").unwrap();

    let protection = |mode: u32| {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let md = std::fs::metadata(&path).unwrap();
        Protection::from_bits(amiga_ffs::populate::protection_from_metadata(&md))
    };

    // rw-------: the owner may read, write and delete, and not execute.
    let p = protection(0o600);
    assert!(p.readable() && p.writable() && p.deletable() && !p.executable());
    assert!(!p.group_readable() && !p.other_readable());
    // rwx------: the host's x bit *removes* the E denial.
    assert!(protection(0o700).executable());
    // ---------: everything denied but delete, which POSIX has no
    // per-file bit for and which is therefore always allowed.
    let p = protection(0o000);
    assert!(!p.readable() && !p.writable() && !p.executable() && p.deletable());
    // The group and other nibbles read the *normal* way round.
    let p = protection(0o644);
    assert!(p.group_readable() && p.other_readable());
    assert!(!p.group_writable() && !p.other_writable());
    assert!(!p.archived() && !p.script() && !p.pure() && !p.hidden());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[cfg(feature = "std")]
fn a_host_timestamp_maps_onto_the_amigados_epoch() {
    use amiga_ffs::populate::datestamp_from_system_time;
    use std::time::{Duration, UNIX_EPOCH};

    // 1978-01-01 00:00:00 UTC is day 0.
    let epoch =
        UNIX_EPOCH + Duration::from_secs(amiga_ffs::meta::AMIGA_EPOCH_UNIX_DAYS as u64 * 86_400);
    assert_eq!(datestamp_from_system_time(epoch), DateStamp::default());
    // A second later is a second later, in ticks.
    let d = datestamp_from_system_time(epoch + Duration::from_secs(3661));
    assert_eq!((d.days, d.mins, d.ticks), (0, 61, 50));
    // Anything before 1978 has no representation, and becomes the epoch
    // rather than a wrapped number that looks like a date.
    assert_eq!(datestamp_from_system_time(UNIX_EPOCH), DateStamp::default());
    assert_eq!(
        datestamp_from_system_time(UNIX_EPOCH - Duration::from_secs(86_400)),
        DateStamp::default()
    );
}

// ---------------------------------------------------------------------------
// The allocator
// ---------------------------------------------------------------------------

/// A small populated volume to allocate against: three files and a
/// directory, so the bitmap has a plausible mix of used and free.
fn allocatable(variant: Variant, bs: usize, nblocks: u64) -> Volume<MemDisk> {
    populated(variant, bs, nblocks, allocatable_tree)
}

/// The tree itself, so a test that wants one more entry in it does not
/// have to restate the three that everything else asserts on.
fn allocatable_tree(pop: &mut Populator<MemDisk>) {
    let meta = amiga_ffs::Metadata::new();
    let root = pop.root_lba();
    let dir = pop.create_dir(root, b"Devs", &meta).unwrap();
    pop.create_file(dir, b"system-configuration", &meta, &pattern(232))
        .unwrap();
    pop.create_file(root, b"Payload", &meta, &pattern(9000))
        .unwrap();
    pop.create_file(root, b"Small", &meta, b"hello").unwrap();
}

/// Set or clear one bitmap bit behind the volume's back, re-checksumming
/// the page at longword 0. Damage a repair is then asked to undo.
fn poke_bitmap_bit(disk: &mut MemDisk, pages: &[u64], reserved: u64, lba: u64, free: bool) {
    let bs = BlockSource::block_size(disk);
    let per_page = bitmap_bits_per_block(bs);
    let bit = lba - reserved;
    let page = pages[(bit / per_page) as usize];
    let within = bit % per_page;
    let mut buf = disk.block(page).to_vec();
    let off = OFF_BITMAP_BITS + (within / 32) as usize * 4;
    let mut w = be32(&buf, off);
    if free {
        w |= 1 << (within % 32);
    } else {
        w &= !(1 << (within % 32));
    }
    buf[off..off + 4].copy_from_slice(&w.to_be_bytes());
    buf[0..4].copy_from_slice(&0u32.to_be_bytes());
    let ck = checksum_compute(&buf, BITMAP_CHECKSUM_INDEX);
    buf[0..4].copy_from_slice(&ck.to_be_bytes());
    disk.poke_block(page, &buf);
}

/// Every block the bitmap marks allocated, as a set — for the two
/// directional invariants, which are statements about sets.
fn allocated_set(vol: &mut Volume<MemDisk>) -> std::collections::HashSet<u64> {
    vol.read_bitmap().unwrap().allocated().collect()
}

#[test]
fn an_allocator_refuses_a_volume_whose_bitmap_is_mid_update() {
    // The state an interrupted populate leaves: the bits on disk are
    // whatever the interruption left, and acting on them hands out blocks
    // a file is using. Rebuilding them is repair's job, and the refusal
    // says so rather than guessing.
    let nblocks = 1760;
    let disk = MemDisk::blank(512, nblocks);
    let opts = FormatOptions::new(Variant::FfsIntl, nblocks, b"Interrupted");
    let mut pop = Populator::new(disk, &opts).unwrap();
    let root = pop.root_lba();
    pop.create_file(root, b"Half", &amiga_ffs::Metadata::new(), &pattern(5000))
        .unwrap();
    let disk = pop.abandon();

    let mut vol = Volume::open_with(disk, None, nblocks, 2).unwrap();
    assert!(matches!(
        Allocator::load(&mut vol),
        Err(AllocError::BitmapInvalid)
    ));
}

#[test]
fn the_allocator_hands_out_every_free_block_exactly_once_and_then_says_full() {
    let mut vol = allocatable(Variant::FfsIntl, 512, 1760);
    let before = allocated_set(&mut vol);
    let mut alloc = Allocator::load(&mut vol).unwrap();
    let free = alloc.blocks_free();
    assert_eq!(alloc.blocks_used(), before.len() as u64);

    let mut got: Vec<u64> = Vec::new();
    loop {
        match alloc.allocate() {
            Ok(a) => got.push(a.block()),
            Err(AllocError::VolumeFull { .. }) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(got.len() as u64, free, "every free block, and no more");
    assert_eq!(alloc.blocks_free(), 0);

    let seen: std::collections::HashSet<u64> = got.iter().copied().collect();
    assert_eq!(seen.len(), got.len(), "no block handed out twice");
    for lba in &got {
        assert!(
            (2..1760).contains(lba),
            "block {lba} is outside the volume's allocatable range"
        );
        assert!(
            !before.contains(lba),
            "block {lba} was already allocated when it was handed out"
        );
    }

    // ...and back again: freeing everything restores the count exactly,
    // and freeing one of them a second time is refused.
    for lba in &got {
        alloc.free(*lba).unwrap();
    }
    assert_eq!(alloc.blocks_free(), free);
    assert!(matches!(
        alloc.free(got[0]),
        Err(AllocError::DoubleFree { lba }) if lba == got[0]
    ));
    // The boot blocks and anything past the end have no bit at all, which
    // is a different refusal from "already free".
    for lba in [0, 1, 1760, 9999] {
        assert!(matches!(
            alloc.free(lba),
            Err(AllocError::NotCovered { .. })
        ));
    }
}

#[test]
fn allocation_scans_forward_from_the_hint_and_wraps_once() {
    let mut vol = allocatable(Variant::FfsIntl, 512, 1760);
    let before = allocated_set(&mut vol);
    let mut alloc = Allocator::load(&mut vol).unwrap();

    // A hint lands on the first free block at or above it...
    for hint in [2u64, 500, 900, 1500] {
        let want = (hint..1760).find(|b| !before.contains(b)).unwrap();
        let a = alloc.allocate_near(hint).unwrap();
        assert_eq!(a.block(), want, "hint {hint}");
        alloc.free(a.block()).unwrap();
    }

    // ...and a hint past the end of the volume is a preference, not an
    // error: the scan wraps to the first allocatable block.
    let a = alloc.allocate_near(9_999).unwrap();
    assert_eq!(a.block(), (2..1760).find(|b| !before.contains(b)).unwrap());

    // The hintless form rotates: consecutive calls climb rather than
    // rescanning the same region, which is the locality a file's data
    // blocks want.
    let mut last = a.block();
    for _ in 0..20 {
        let n = alloc.allocate().unwrap().block();
        assert!(n > last, "{n} follows {last}");
        last = n;
    }
}

#[test]
fn a_new_block_may_not_be_pointed_at_until_its_bitmap_page_is_on_the_disk() {
    // The mark-then-use rule, as a return type. `block()` is always
    // available -- writing the block's own contents is safe, because
    // nothing reaches it -- and `reference()` is what a hash slot or a
    // data-pointer table has to be filled from.
    let mut vol = allocatable(Variant::FfsIntl, 512, 1760);
    let mut alloc = Allocator::load(&mut vol).unwrap();
    let a = alloc.allocate().unwrap();
    assert!(a.block() >= 2);
    assert!(matches!(
        alloc.reference(&a),
        Err(AllocError::NotDurable { lba }) if lba == a.block()
    ));
    assert_eq!(alloc.dirty_pages(), 1);
    alloc.flush(vol.source_mut()).unwrap();
    assert_eq!(alloc.dirty_pages(), 0);
    assert_eq!(alloc.reference(&a).unwrap(), a.block());
}

#[test]
fn a_flush_writes_only_the_pages_that_changed() {
    // Five bitmap pages at 512 bytes: 4064 blocks each.
    let nblocks = 20_000;
    let mut vol = allocatable(Variant::FfsIntl, 512, nblocks);
    let pages = vol.read_bitmap().unwrap().pages().to_vec();
    assert_eq!(pages.len(), 5, "the volume is big enough for the test");

    let mut alloc = Allocator::load(&mut vol).unwrap();
    // Three allocations inside one page, and one deliberately inside
    // another: two pages dirty, not four.
    for _ in 0..3 {
        alloc.allocate().unwrap();
    }
    let far = alloc.allocate_near(15_000).unwrap();
    assert_eq!(alloc.dirty_pages(), 2);

    vol.source_mut().clear_log();
    assert_eq!(alloc.flush(vol.source_mut()).unwrap(), 2);
    let written = vol.source_mut().write_log().to_vec();
    assert_eq!(written.len(), 2, "one write per dirty page: {written:?}");
    assert!(written.iter().all(|b| pages.contains(b)));
    assert!(written.contains(&pages[(far.block() - 2) as usize / 4064]));

    // And nothing at all the second time: a flush with no edits is not a
    // rewrite of the bitmap.
    vol.source_mut().clear_log();
    assert_eq!(alloc.flush(vol.source_mut()).unwrap(), 0);
    assert!(vol.source_mut().write_log().is_empty());
}

#[test]
fn allocation_never_disagrees_with_a_model_of_the_same_bitmap() {
    // The property the whole module exists for, cross-checked against a
    // set the test keeps itself: interleave allocations, frees and
    // flushes from a seeded generator, and after every step assert the
    // allocator has never handed out a block the model already holds.
    // The disk is compared at the end too, because an allocator that is
    // right in memory and writes the bits inverted is the exact failure
    // the bitmap module's four conventions are about.
    let nblocks = 20_000;
    let mut vol = allocatable(Variant::FfsIntl, 512, nblocks);
    let mut model: std::collections::HashSet<u64> = allocated_set(&mut vol);
    let mut alloc = Allocator::load(&mut vol).unwrap();
    let mut mine: Vec<u64> = Vec::new();
    let mut rng = Rng::new(0xA11C_0DE5);

    for step in 0..4000 {
        match rng.below(10) {
            0..=5 => {
                let a = if rng.below(2) == 0 {
                    alloc.allocate().unwrap()
                } else {
                    alloc
                        .allocate_near(2 + rng.below(nblocks as usize) as u64)
                        .unwrap()
                };
                let lba = a.block();
                assert!(
                    model.insert(lba),
                    "step {step}: block {lba} handed out while already allocated"
                );
                assert!((2..nblocks).contains(&lba), "step {step}: block {lba}");
                mine.push(lba);
            }
            6..=8 => {
                if mine.is_empty() {
                    continue;
                }
                let i = rng.below(mine.len());
                let lba = mine.swap_remove(i);
                alloc.free(lba).unwrap();
                assert!(model.remove(&lba));
            }
            _ => {
                alloc.flush(vol.source_mut()).unwrap();
            }
        }
        assert_eq!(alloc.blocks_used(), model.len() as u64, "step {step}");
    }

    alloc.flush(vol.source_mut()).unwrap();
    let on_disk = allocated_set(&mut vol);
    assert_eq!(on_disk, model, "the bits that were written are the model");
}

/// One allocation session, as a caller with the discipline would write
/// it: mark, flush, then use — the block's contents only, since nothing
/// in wave 1 chains anything into a directory.
fn allocation_session(vol: &mut Volume<MemDisk>) -> Result<Vec<u64>, AllocError<MemError>> {
    let mut alloc = Allocator::load(vol)?;
    let mut got = Vec::new();
    for _ in 0..6 {
        got.push(alloc.allocate()?);
    }
    alloc.flush(vol.source_mut())?;
    let bs = vol.block_size();
    let mut out = Vec::new();
    for a in &got {
        let lba = alloc.reference(a)?;
        let buf = vec![0x5Au8; bs];
        vol.source_mut()
            .write_block(lba, &buf)
            .map_err(AllocError::Io)?;
        out.push(lba);
    }
    Ok(out)
}

#[test]
fn an_interrupted_allocation_session_leaks_and_never_double_allocates() {
    // The crash-shape claim, checked at every point rather than at one:
    // stop the medium after n writes for every n the session takes, and
    // the damage must always be leak-shaped. `OrphanBlock` is allowed --
    // a block marked allocated that nothing reaches is space lost and
    // nothing worse. `ReachableButFree` is not, ever: it is the state in
    // which the next allocation hands a block to a second owner.
    let nblocks = 1760;
    let total = {
        let mut vol = allocatable(Variant::FfsIntl, 512, nblocks);
        vol.source_mut().clear_log();
        allocation_session(&mut vol).unwrap();
        vol.source_mut().write_log().len()
    };
    assert!(
        total >= 7,
        "the session writes a bitmap page and six blocks"
    );

    let mut crashed = 0;
    for n in 0..=total {
        let mut vol = allocatable(Variant::FfsIntl, 512, nblocks);
        vol.source_mut().fail_after(n);
        if allocation_session(&mut vol).is_err() {
            crashed += 1;
        }

        let disk = vol.into_inner();
        let mut vol = Volume::open_with(disk, None, nblocks, 2).unwrap();
        let report = vol.validate();
        for finding in &report.findings {
            assert!(
                matches!(finding, Finding::OrphanBlock { .. }),
                "crash after {n} writes left {finding}"
            );
        }
        assert_eq!(report.summary.reachable_but_free, 0, "crash after {n}");

        // ...and the volume that was there before is untouched: an
        // allocator that scribbles on a file it was supposed to avoid
        // would show up here and nowhere else.
        let root = vol.root_lba();
        let payload = vol.lookup(root, b"Payload").unwrap().unwrap();
        assert_eq!(vol.read_file(payload.lba).unwrap(), pattern(9000));
    }
    assert!(crashed > 0, "at least one prefix must actually fail");
}

// ---------------------------------------------------------------------------
// repair()
// ---------------------------------------------------------------------------

/// The two statements a repair must always be able to make, whatever it
/// was handed: it added allocation and removed none, and nothing is
/// reachable-but-free afterwards.
fn assert_repair_invariants(
    before: &std::collections::HashSet<u64>,
    after: &std::collections::HashSet<u64>,
    report: &Report<MemError>,
) {
    for lba in before {
        assert!(
            after.contains(lba),
            "block {lba} was allocated before the repair and is not after: \
             a repair may only ever add allocation"
        );
    }
    assert_eq!(
        report.summary.reachable_but_free, 0,
        "a repaired volume never leaves a block in use marked free"
    );
    assert!(
        !report
            .findings
            .iter()
            .any(|f| matches!(f, Finding::BitmapInvalid | Finding::BitmapIncomplete { .. })),
        "{:?}",
        report.findings
    );
}

/// Every file of [`allocatable`]'s tree, read back byte for byte.
fn assert_tree_intact(vol: &mut Volume<MemDisk>) {
    let root = vol.root_lba();
    let payload = vol.lookup(root, b"Payload").unwrap().expect("Payload");
    assert_eq!(vol.read_file(payload.lba).unwrap(), pattern(9000));
    let small = vol.lookup(root, b"Small").unwrap().expect("Small");
    assert_eq!(vol.read_file(small.lba).unwrap(), b"hello");
    let devs = vol.lookup(root, b"Devs").unwrap().expect("Devs").lba;
    let cfg = vol
        .lookup(devs, b"system-configuration")
        .unwrap()
        .expect("system-configuration");
    assert_eq!(vol.read_file(cfg.lba).unwrap(), pattern(232));
}

#[test]
fn repair_allocates_a_block_that_was_in_use_and_marked_free() {
    let mut vol = allocatable(Variant::FfsIntl, 512, 1760);
    let pages = vol.read_bitmap().unwrap().pages().to_vec();
    let root = vol.root_lba();
    let victim = vol.lookup(root, b"Payload").unwrap().unwrap().lba;
    let before = allocated_set(&mut vol);

    let mut disk = vol.into_inner();
    poke_bitmap_bit(&mut disk, &pages, 2, victim, true);
    let mut vol = Volume::open_with(disk, None, 1760, 2).unwrap();
    assert_eq!(vol.validate().summary.reachable_but_free, 1);

    let done = vol.repair(&RepairOptions::new()).unwrap();
    assert_eq!(done.allocated, 1);
    assert!(done.actions.contains(&Action::Allocated { lba: victim }));
    assert_eq!(done.leaked, 0);
    assert_eq!(done.pages_written, pages.len() as u64);

    let report = vol.validate();
    assert!(report.is_clean(), "{:?}", report.findings);
    let after = allocated_set(&mut vol);
    assert_repair_invariants(&before, &after, &report);
    assert_eq!(after, before, "the volume is back to exactly what it was");
    assert_tree_intact(&mut vol);
}

#[test]
fn repair_keeps_a_leak_rather_than_freeing_it() {
    // The direction the ROM validator takes and this one does not, and
    // the reason: a block the walk did not reach is only *proved* free if
    // the walk was complete, which on a damaged volume is exactly what it
    // is not.
    let mut vol = allocatable(Variant::FfsIntl, 512, 1760);
    let pages = vol.read_bitmap().unwrap().pages().to_vec();
    let stray = vol.read_bitmap().unwrap().free().next().unwrap();
    let before = allocated_set(&mut vol);

    let mut disk = vol.into_inner();
    poke_bitmap_bit(&mut disk, &pages, 2, stray, false);
    let mut vol = Volume::open_with(disk, None, 1760, 2).unwrap();
    let before_damaged = allocated_set(&mut vol);
    assert_eq!(vol.validate().summary.orphans, 1);

    let done = vol.repair(&RepairOptions::new()).unwrap();
    assert_eq!(done.leaked, 1);
    assert!(done.actions.contains(&Action::LeakKept { lba: stray }));
    assert_eq!(done.allocated, 0);

    let report = vol.validate();
    assert_eq!(report.summary.orphans, 1, "the leak is still a leak");
    let after = allocated_set(&mut vol);
    assert_repair_invariants(&before_damaged, &after, &report);
    assert!(after.contains(&stray));
    assert!(before.is_subset(&after));
    assert_tree_intact(&mut vol);
}

#[test]
fn repair_stamps_a_mid_update_bitmap_valid_again() {
    // The abandoned populate from the allocator tests above: a volume
    // that says "do not allocate from me" and has no other damage. After
    // a repair it validates clean and the allocator will load it.
    let nblocks = 1760;
    let disk = MemDisk::blank(512, nblocks);
    let opts = FormatOptions::new(Variant::FfsIntlDircache, nblocks, b"Interrupted");
    let mut pop = Populator::new(disk, &opts).unwrap();
    let root = pop.root_lba();
    pop.create_file(root, b"Half", &amiga_ffs::Metadata::new(), &pattern(5000))
        .unwrap();
    let disk = pop.abandon();

    let mut vol = Volume::open_with(disk, None, nblocks, 2).unwrap();
    assert!(!vol.read_bitmap().unwrap().valid());
    let done = vol.repair(&RepairOptions::new()).unwrap();
    // Nothing on this volume was ever marked allocated, so every block the
    // walk reaches is a correction.
    assert!(done.allocated > 5, "{done:?}");
    assert_eq!(done.leaked, 0);

    assert!(vol.read_bitmap().unwrap().valid());
    let report = vol.validate();
    assert!(report.is_clean(), "{:?}", report.findings);
    let root = vol.root_lba();
    let half = vol.lookup(root, b"Half").unwrap().unwrap().lba;
    assert_eq!(vol.read_file(half).unwrap(), pattern(5000));
    assert!(Allocator::load(&mut vol).is_ok());
}

#[test]
fn repair_replaces_a_bitmap_page_it_cannot_use() {
    for damage in ["zero", "outside", "in-use"] {
        let mut vol = allocatable(Variant::FfsIntl, 512, 1760);
        let old_page = vol.read_bitmap().unwrap().pages()[0];
        let root_lba = vol.root_lba();
        let payload = {
            let root = vol.root_lba();
            vol.lookup(root, b"Payload").unwrap().unwrap().lba
        };
        let before = allocated_set(&mut vol);

        // Clobber the root's one bitmap page pointer, three ways: gone,
        // out of the volume, and naming a block a file is using.
        let mut disk = vol.into_inner();
        let mut root_block = disk.block(root_lba).to_vec();
        let value: u32 = match damage {
            "zero" => 0,
            "outside" => 9_999,
            _ => payload as u32,
        };
        root_block[tail(512, TL_BITMAP_PAGES)..tail(512, TL_BITMAP_PAGES) + 4]
            .copy_from_slice(&value.to_be_bytes());
        let ck = checksum_compute(&root_block, CHECKSUM_INDEX);
        root_block[OFF_CHECKSUM..OFF_CHECKSUM + 4].copy_from_slice(&ck.to_be_bytes());
        disk.poke_block(root_lba, &root_block);

        let mut vol = Volume::open_with(disk, None, 1760, 2).unwrap();
        let done = vol.repair(&RepairOptions::new()).unwrap();
        assert_eq!(done.blocks_replaced, 1, "{damage}");
        assert!(
            done.actions
                .iter()
                .any(|a| matches!(a, Action::PageReplaced { index: 0, was, .. } if *was == value)),
            "{damage}: {:?}",
            done.actions
        );

        let report = vol.validate();
        let after = allocated_set(&mut vol);
        assert_repair_invariants(&before, &after, &report);
        assert_tree_intact(&mut vol);
        // Wherever the replacement landed, the volume's own bitmap now
        // names it, it is not the pointer that was rejected, and it is
        // marked allocated like any other block in use. (It frequently
        // *is* the old page block: with the pointer gone, nothing said
        // that block was in use, so the scan is free to reuse it -- which
        // is the best outcome available and not a hazard, because a
        // bitmap page is exactly the block whose contents are about to be
        // overwritten anyway.)
        let new_page = vol.read_bitmap().unwrap().pages()[0];
        assert_ne!(new_page as u32, value, "{damage}");
        assert!(after.contains(&new_page), "{damage}");
        let _ = old_page;
    }
}

#[test]
fn repair_rewrites_a_bitmap_page_whose_checksum_is_gone() {
    let mut vol = allocatable(Variant::FfsIntl, 512, 1760);
    let page = vol.read_bitmap().unwrap().pages()[0];
    let before = allocated_set(&mut vol);

    let mut disk = vol.into_inner();
    let mut buf = disk.block(page).to_vec();
    buf[16] ^= 0xFF;
    disk.poke_block(page, &buf);
    let mut vol = Volume::open_with(disk, None, 1760, 2).unwrap();
    assert!(vol.read_bitmap().is_err(), "the page no longer sums");

    let done = vol.repair(&RepairOptions::new()).unwrap();
    assert!(
        done.actions.contains(&Action::PageUnreadable { lba: page }),
        "{:?}",
        done.actions
    );
    let report = vol.validate();
    assert!(report.is_clean(), "{:?}", report.findings);
    let after = allocated_set(&mut vol);
    // Every block that is *reachable* comes back; the page's bits could
    // not be read, so anything allocated and unreachable in its region is
    // gone with it -- which is why the action names the page.
    assert_eq!(after, before);
    assert_tree_intact(&mut vol);
}

#[test]
fn severing_is_opt_in_and_cuts_only_what_will_not_read() {
    // A file header block scribbled over: the entry cannot be parsed, so
    // its whole chain position is unusable. By default the repair leaves
    // it -- a recovery tool wants to see the dangling link -- and under
    // `sever` the chain is truncated at the last good link and the blocks
    // behind it are leaked, not freed.
    let build = || {
        let mut vol = populated(Variant::FfsIntl, 512, 1760, |pop| {
            allocatable_tree(pop);
            pop.create_file(
                pop.root_lba(),
                b"Doomed",
                &amiga_ffs::Metadata::new(),
                b"gone",
            )
            .unwrap();
        });
        // A victim alone in its hash slot, so the cut cannot take an
        // innocent entry with it: nothing chains to it and nothing chains
        // off it.
        let root = vol.root_lba();
        let entries = vol.read_dir(root).unwrap();
        let doomed = entries
            .iter()
            .find(|e| e.name == b"Doomed")
            .expect("Doomed")
            .clone();
        assert_eq!(doomed.hash_chain, 0, "Doomed is the end of its chain");
        assert!(
            !entries.iter().any(|e| e.hash_chain as u64 == doomed.lba),
            "Doomed is the head of its chain"
        );
        let mut disk = vol.into_inner();
        disk.poke_block(doomed.lba, &vec![0u8; 512]);
        (Volume::open_with(disk, None, 1760, 2).unwrap(), doomed.lba)
    };

    let (mut vol, victim) = build();
    let damaged = vol.validate();
    assert!(damaged
        .findings
        .iter()
        .any(|f| matches!(f, Finding::Unreadable { lba, .. } if *lba == victim)));

    let done = vol.repair(&RepairOptions::new()).unwrap();
    assert_eq!(done.severed, 0, "severing is opt-in");
    assert!(vol
        .validate()
        .findings
        .iter()
        .any(|f| matches!(f, Finding::Unreadable { lba, .. } if *lba == victim)));

    let (mut vol, victim) = build();
    let before = allocated_set(&mut vol);
    let done = vol.repair(&RepairOptions::new().sever(true)).unwrap();
    assert_eq!(done.severed, 1);
    assert!(
        done.actions
            .iter()
            .any(|a| matches!(a, Action::ChainTruncated { dropped, .. } if *dropped == victim)),
        "{:?}",
        done.actions
    );

    let report = vol.validate();
    let after = allocated_set(&mut vol);
    assert_repair_invariants(&before, &after, &report);
    // What remains is leaks and nothing else: the severed header and the
    // blocks it owned are still marked allocated, which is the direction
    // that can be recovered.
    for finding in &report.findings {
        assert!(
            matches!(finding, Finding::OrphanBlock { .. }),
            "after severing: {finding}"
        );
    }
    assert!(
        after.contains(&victim),
        "the severed block is leaked, not freed"
    );
    let root = vol.root_lba();
    assert!(vol.lookup(root, b"Doomed").unwrap().is_none());
    assert_tree_intact(&mut vol);
}

#[test]
fn repair_is_clean_on_every_variant_and_block_size() {
    // The bitmap arithmetic changes with the block size and the dircache
    // adds a block per directory, so the matrix is the test.
    for variant in ALL_VARIANTS {
        for bs in [512usize, 1024, 4096] {
            let nblocks = 1_800_000 / bs as u64;
            let mut vol = allocatable(variant, bs, nblocks);
            let pages = vol.read_bitmap().unwrap().pages().to_vec();
            let before = allocated_set(&mut vol);
            let root = vol.root_lba();
            let victim = vol.lookup(root, b"Payload").unwrap().unwrap().lba;

            let mut disk = vol.into_inner();
            poke_bitmap_bit(&mut disk, &pages, 2, victim, true);
            let mut vol = Volume::open_with(disk, None, nblocks, 2).unwrap();
            vol.repair(&RepairOptions::new()).unwrap();

            let report = vol.validate();
            assert!(
                report.is_clean(),
                "{variant:?} @{bs}: {:?}",
                report.findings
            );
            let after = allocated_set(&mut vol);
            assert_repair_invariants(&before, &after, &report);
            assert_eq!(after, before, "{variant:?} @{bs}");
            assert_tree_intact(&mut vol);
        }
    }
}

#[test]
fn an_interrupted_repair_leaves_a_volume_that_refuses_to_be_allocated_from() {
    // The same crash sweep as the allocator's, over the repair itself:
    // whatever prefix of its writes lands, the result must never claim a
    // valid bitmap it has not finished writing.
    let nblocks = 1760;
    let total = {
        let mut vol = allocatable(Variant::FfsIntl, 512, nblocks);
        vol.source_mut().clear_log();
        vol.repair(&RepairOptions::new()).unwrap();
        vol.source_mut().write_log().len()
    };

    for n in 0..total {
        let mut vol = allocatable(Variant::FfsIntl, 512, nblocks);
        let pages = vol.read_bitmap().unwrap().pages().to_vec();
        let root = vol.root_lba();
        let victim = vol.lookup(root, b"Payload").unwrap().unwrap().lba;
        let mut disk = vol.into_inner();
        poke_bitmap_bit(&mut disk, &pages, 2, victim, true);
        let mut vol = Volume::open_with(disk, None, nblocks, 2).unwrap();

        vol.source_mut().fail_after(n);
        let _ = vol.repair(&RepairOptions::new());

        let disk = vol.into_inner();
        let mut vol = Volume::open_with(disk, None, nblocks, 2).unwrap();
        let bitmap_claims_valid = vol.read_bitmap().map(|b| b.valid()).unwrap_or(false);
        if bitmap_claims_valid {
            // Either the repair never started or it finished: both are
            // states in which the bitmap may be trusted, so it must not
            // be lying about a block in use.
            assert_eq!(
                vol.validate().summary.reachable_but_free,
                if n == 0 { 1 } else { 0 },
                "crash after {n} writes"
            );
        }
        assert_tree_intact(&mut vol);
    }
}

// ---------------------------------------------------------------------------
// Mutation: create, delete, rename, set_metadata in existing volumes
// ---------------------------------------------------------------------------

use amiga_ffs::{MetaUpdate, MutateError, Mutator};

/// Open a populated volume for mutation, do something to it, and hand the
/// volume back. Every operation is complete when it returns, so there is
/// no `finish()` here and none in the API.
fn mutating(vol: Volume<MemDisk>, f: impl FnOnce(&mut Mutator<MemDisk>)) -> Volume<MemDisk> {
    let mut m = Mutator::open(vol).expect("open mutator");
    f(&mut m);
    m.into_volume()
}

/// Fail loudly with the findings spelled out: a bare `assert!(clean)` on a
/// validator whose whole point is typed findings tells you nothing.
fn assert_clean(vol: &mut Volume<MemDisk>, what: &str) {
    let report = vol.validate();
    assert!(
        report.is_clean(),
        "{what}: {:#?}",
        report
            .findings
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
    );
}

/// A volume with a little tree already on it, opened for mutation — the
/// case this whole wave is about: blocks somebody else allocated, a
/// bitmap that has to be believed and updated rather than rewritten.
fn mutable(variant: Variant, bs: usize, nblocks: u64) -> Volume<MemDisk> {
    populated(variant, bs, nblocks, allocatable_tree)
}

/// The property this wave is for: entries created in a volume that
/// already existed read back identical, the volume validates clean, and
/// the bitmap accounts for exactly the blocks the walk reaches.
#[test]
fn creating_in_an_existing_volume_round_trips_on_every_variant_and_block_size() {
    for variant in ALL_VARIANTS {
        for bs in [512usize, 4096] {
            let nblocks = 8 * 1024 * 1024 / bs as u64;
            let vol = mutable(variant, bs, nblocks);
            let root = vol.root_lba();
            let big = pattern(
                hash_table_size(bs) as usize * data_payload_size(bs, variant.is_ffs()) + 3000,
            );

            let mut made = Vec::new();
            let mut vol = mutating(vol, |m| {
                let meta = Metadata::new().comment(b"a comment").protection(0x55);
                let devs = m.volume().lookup(root, b"Devs").unwrap().unwrap().lba;
                made.push(m.create_dir(devs, b"Nested", &meta).unwrap());
                let nested = *made.last().unwrap();
                made.push(
                    m.create_file(nested, b"deep", &meta, b"deep bytes")
                        .unwrap(),
                );
                made.push(m.create_file(root, b"Crosser", &meta, &big).unwrap());
                made.push(m.create_file(root, b"Empty", &meta, b"").unwrap());
            });

            assert_clean(&mut vol, &format!("{variant:?} @{bs}"));
            let devs = vol.lookup(root, b"Devs").unwrap().unwrap().lba;
            let nested = vol.lookup(devs, b"Nested").unwrap().unwrap();
            assert_eq!(nested.kind, EntryKind::Directory);
            assert_eq!(nested.protection, 0x55);
            assert_eq!(vol.comment(&nested).unwrap(), b"a comment");
            let deep = vol.lookup(nested.lba, b"deep").unwrap().unwrap();
            assert_eq!(vol.read_file(deep.lba).unwrap(), b"deep bytes");
            let crosser = vol.lookup(root, b"Crosser").unwrap().unwrap();
            assert_eq!(vol.read_file(crosser.lba).unwrap(), big);
            let empty = vol.lookup(root, b"Empty").unwrap().unwrap();
            assert_eq!(empty.byte_size, 0);
            assert_eq!(vol.read_file(empty.lba).unwrap(), b"");

            // The tree that was already there is untouched, byte for byte.
            assert_tree_intact(&mut vol);

            // Reachable and allocated are two independent tallies of the
            // same fact, and a mutation that leaked would separate them.
            let report = vol.validate();
            assert_eq!(report.summary.reachable, report.summary.allocated);
            assert_eq!(report.summary.orphans, 0);
            assert_eq!(report.summary.reachable_but_free, 0);
            // A file that needed one is proof the extension-block path ran.
            assert!(report.summary.extension_blocks >= 1);
        }
    }
}

/// Head insertion again, this time into a chain that already exists on a
/// volume this crate did not write in one pass: the oracle-verified order
/// is newest-first, and a mutator that appended instead would still pass
/// every lookup.
#[test]
fn a_created_entry_takes_the_head_of_an_existing_chain() {
    // AAA, AEU and AFH all hash to slot 54 of a 72-slot table.
    let t = hash_table_size(512);
    for n in [&b"AAA"[..], b"AEU", b"AFH"] {
        assert_eq!(name_hash(n, intl_toupper, t), 54);
    }
    let vol = populated(Variant::FfsIntl, 512, 1760, |pop| {
        let root = pop.root_lba();
        pop.create_file(root, b"AAA", &Metadata::new(), b"one")
            .unwrap();
    });
    let root = vol.root_lba();

    let mut vol = mutating(vol, |m| {
        m.create_file(root, b"AEU", &Metadata::new(), b"two")
            .unwrap();
        m.create_file(root, b"AFH", &Metadata::new(), b"three")
            .unwrap();
    });

    let head = vol.root().hash_table[54];
    let first = vol.entry_at(head as u64).unwrap();
    assert_eq!(first.name, b"AFH");
    let second = vol.entry_at(first.hash_chain as u64).unwrap();
    assert_eq!(second.name, b"AEU");
    let third = vol.entry_at(second.hash_chain as u64).unwrap();
    assert_eq!(third.name, b"AAA");
    assert_eq!(third.hash_chain, 0);
    assert_clean(&mut vol, "three colliding names");
}

/// No leaks in the happy path, stated as an equation: a create followed
/// by the matching delete returns the volume to the *exact* set of blocks
/// it had before, on every variant and both block sizes. Anything the
/// create allocated and the delete forgot — an extension block, an
/// overflow comment, a dircache block the chain grew into — shows up here
/// and nowhere else.
#[test]
fn a_create_and_its_delete_return_the_volume_to_the_same_blocks() {
    for variant in ALL_VARIANTS {
        for bs in [512usize, 4096] {
            let nblocks = 8 * 1024 * 1024 / bs as u64;
            let mut vol = mutable(variant, bs, nblocks);
            let root = vol.root_lba();
            let before = allocated_set(&mut vol);
            let comment = &b"a comment long enough to be worth carrying about with us"[..];
            // On LNFS this name plus that comment does not fit the merged
            // field, so the create allocates a T_COMMENT block too.
            let long: Vec<u8> = if variant.has_long_names() {
                vec![b'L'; 100]
            } else {
                b"Ordinary".to_vec()
            };
            let big = pattern(
                hash_table_size(bs) as usize * data_payload_size(bs, variant.is_ffs()) + 100,
            );

            let mut vol = mutating(vol, |m| {
                let meta = Metadata::new().comment(comment);
                m.create_dir(root, b"Doomed", &meta).unwrap();
                m.create_file(root, &long, &meta, &big).unwrap();
            });
            assert_clean(&mut vol, &format!("{variant:?} @{bs} after create"));
            assert!(allocated_set(&mut vol).len() > before.len());

            let mut vol = mutating(vol, |m| {
                m.delete(root, b"Doomed").unwrap();
                m.delete(root, &long).unwrap();
            });
            assert_clean(&mut vol, &format!("{variant:?} @{bs} after delete"));
            assert_eq!(
                allocated_set(&mut vol),
                before,
                "{variant:?} @{bs}: create+delete did not round-trip the bitmap"
            );
            assert_tree_intact(&mut vol);
        }
    }
}

/// Splicing a chain has three cases and they are all different code: the
/// head lives in the directory's hash table, everything else lives in the
/// previous entry's longword −4, and the tail is the one whose successor
/// is 0.
#[test]
fn delete_splices_the_head_the_middle_and_the_tail_of_a_chain() {
    // Written AAA, AEU, AFH, the chain reads AFH -> AEU -> AAA.
    for (victim, survivors) in [
        (&b"AFH"[..], [&b"AEU"[..], b"AAA"]),
        (b"AEU", [b"AFH", b"AAA"]),
        (b"AAA", [b"AFH", b"AEU"]),
    ] {
        let vol = populated(Variant::FfsIntl, 512, 1760, |pop| {
            let root = pop.root_lba();
            for n in [&b"AAA"[..], b"AEU", b"AFH"] {
                pop.create_file(root, n, &Metadata::new(), n).unwrap();
            }
        });
        let root = vol.root_lba();
        let mut vol = mutating(vol, |m| {
            m.delete(root, victim).unwrap();
        });

        assert_clean(&mut vol, "after splicing a chain");
        assert!(vol.lookup(root, victim).unwrap().is_none());
        for name in survivors {
            let e = vol
                .lookup(root, name)
                .unwrap()
                .unwrap_or_else(|| panic!("{:?} gone after deleting {:?}", name, victim));
            assert_eq!(vol.read_file(e.lba).unwrap(), name);
        }
        assert_eq!(vol.read_dir(root).unwrap().len(), 2);
    }
}

#[test]
fn delete_refuses_a_directory_with_anything_in_it() {
    let vol = mutable(Variant::FfsIntl, 512, 1760);
    let root = vol.root_lba();
    let mut m = Mutator::open(vol).unwrap();
    assert!(matches!(
        m.delete(root, b"Devs"),
        Err(MutateError::DirectoryNotEmpty { .. })
    ));
    // And the refusal changed nothing: the volume is still exactly there.
    let devs = m.volume().lookup(root, b"Devs").unwrap().unwrap().lba;
    m.delete(devs, b"system-configuration").unwrap();
    m.delete(root, b"Devs").unwrap();
    let mut vol = m.into_volume();
    assert_clean(&mut vol, "after emptying and deleting");
    assert!(vol.lookup(root, b"Devs").unwrap().is_none());
}

#[test]
fn delete_refuses_a_hard_links_target_and_frees_the_links_themselves() {
    // A volume with a hard link, built by hand: the crate has no
    // link-creating API yet, and what is under test is the refusal.
    let nblocks = 1760;
    let mut b = Builder::new(Variant::FfsIntl, 512, nblocks, b"Linked");
    let root_lba = b.root_lba();
    let f = b.add_file(root_lba, b"Real", b"", b"contents");
    b.add_hard_link(root_lba, b"Alias", EntryKind::LinkFile, f.header);
    b.add_soft_link(root_lba, b"Elsewhere", b"Work:Tools/Ed");
    b.add_bitmap(true);
    let vol = Volume::open_with(b.finish(), None, nblocks, 2).unwrap();
    let root = vol.root_lba();
    let mut m = Mutator::open(vol).unwrap();
    assert!(matches!(
        m.delete(root, b"Real"),
        Err(MutateError::LinkedTo { .. })
    ));

    // Deleting the link first works, and takes the link out of the
    // target's chain -- which is what makes the target deletable.
    m.delete(root, b"Alias").unwrap();
    assert_eq!(
        m.volume().lookup(root, b"Real").unwrap().unwrap().next_link,
        0
    );
    m.delete(root, b"Real").unwrap();

    // A soft link is a header block and the path inside it: nothing to
    // splice, one block to give back.
    let soft = m.volume().lookup(root, b"Elsewhere").unwrap().unwrap().lba;
    assert_eq!(m.volume().read_softlink(soft).unwrap(), b"Work:Tools/Ed");
    m.delete(root, b"Elsewhere").unwrap();

    let mut vol = m.into_volume();
    assert_clean(
        &mut vol,
        "after deleting a link, its target and a soft link",
    );
    assert!(vol.read_dir(root).unwrap().is_empty());
}

#[test]
fn rename_moves_an_entry_within_and_between_directories() {
    for variant in ALL_VARIANTS {
        let vol = mutable(variant, 512, 1760);
        let root = vol.root_lba();
        let mut vol = mutating(vol, |m| {
            let devs = m.volume().lookup(root, b"Devs").unwrap().unwrap().lba;
            // Within one directory.
            m.rename(root, b"Small", root, b"Tiny").unwrap();
            // And across two, keeping the name.
            m.rename(root, b"Payload", devs, b"Payload").unwrap();
        });

        assert_clean(&mut vol, &format!("{variant:?} after renames"));
        assert!(vol.lookup(root, b"Small").unwrap().is_none());
        let tiny = vol.lookup(root, b"Tiny").unwrap().unwrap();
        assert_eq!(vol.read_file(tiny.lba).unwrap(), b"hello");
        assert!(vol.lookup(root, b"Payload").unwrap().is_none());
        let devs = vol.lookup(root, b"Devs").unwrap().unwrap().lba;
        let moved = vol.lookup(devs, b"Payload").unwrap().unwrap();
        assert_eq!(moved.parent as u64, devs);
        assert_eq!(vol.read_file(moved.lba).unwrap(), pattern(9000));
    }
}

/// The rename FFS is unusual for supporting: same name, different case.
/// The name hashes to the same slot, so the entry leaves and re-enters
/// one chain, and the stored bytes change while every lookup keeps
/// matching.
#[test]
fn a_case_only_rename_changes_the_stored_bytes_and_nothing_else() {
    for variant in ALL_VARIANTS {
        let vol = mutable(variant, 512, 1760);
        let root = vol.root_lba();
        let before = {
            let mut v = vol;
            let s = allocated_set(&mut v);
            v = mutating(v, |m| {
                m.rename(root, b"Small", root, b"SMALL").unwrap();
            });
            let after = allocated_set(&mut v);
            assert_eq!(s, after, "{variant:?}: a rename allocated something");
            v
        };
        let mut vol = before;
        assert_clean(&mut vol, &format!("{variant:?} after a case-only rename"));
        let e = vol.lookup(root, b"small").unwrap().unwrap();
        assert_eq!(e.name, b"SMALL", "{variant:?}: case was not preserved");
        assert_eq!(vol.read_file(e.lba).unwrap(), b"hello");
    }
}

/// Latin-1 under both fold tables: `Café` and `CAFÉ` are one name on the
/// international variants and two on `DOS\0`/`DOS\1`, and a rename has to
/// agree with whichever table the volume uses — in the duplicate check
/// and in the hash slot alike.
#[test]
fn renaming_latin1_respects_the_volumes_own_fold_table() {
    for variant in [Variant::Ffs, Variant::FfsIntl] {
        let vol = populated(variant, 512, 1760, |pop| {
            let root = pop.root_lba();
            let m = Metadata::new();
            pop.create_file(root, b"caf\xE9", &m, b"lower").unwrap();
            pop.create_file(root, b"Other", &m, b"other").unwrap();
        });
        let root = vol.root_lba();
        let mut m = Mutator::open(vol).unwrap();

        let clash = m.rename(root, b"Other", root, b"CAF\xC9");
        if variant.is_intl() {
            // Folds together: the name is taken.
            assert!(matches!(clash, Err(MutateError::DuplicateName { .. })));
        } else {
            // Does not fold: two different names, both legal.
            clash.unwrap();
        }
        // And the entry itself renames to its own upper case either way.
        m.rename(root, b"caf\xE9", root, b"CAF\xC9").ok();
        let mut vol = m.into_volume();
        assert_clean(&mut vol, &format!("{variant:?} latin-1 rename"));
    }
}

#[test]
fn rename_refuses_a_duplicate_and_a_directory_moved_into_itself() {
    let vol = mutable(Variant::FfsIntl, 512, 1760);
    let root = vol.root_lba();
    let mut m = Mutator::open(vol).unwrap();
    let devs = m.volume().lookup(root, b"Devs").unwrap().unwrap().lba;
    let inner = m.create_dir(devs, b"Inner", &Metadata::new()).unwrap();

    assert!(matches!(
        m.rename(root, b"Small", root, b"Payload"),
        Err(MutateError::DuplicateName { .. })
    ));
    // A directory into itself...
    assert!(matches!(
        m.rename(root, b"Devs", devs, b"Devs"),
        Err(MutateError::IntoOwnSubtree { .. })
    ));
    // ...and into its own subtree, which needs the ancestry walk.
    assert!(matches!(
        m.rename(root, b"Devs", inner, b"Devs"),
        Err(MutateError::IntoOwnSubtree { .. })
    ));
    // The other direction is fine: a child moved out to the root.
    m.rename(devs, b"Inner", root, b"Inner").unwrap();

    let mut vol = m.into_volume();
    assert_clean(&mut vol, "after refused renames");
    assert!(vol.lookup(root, b"Inner").unwrap().is_some());
    assert!(vol.lookup(devs, b"Inner").unwrap().is_none());
}

/// The LNFS boundary, both ways. The name and comment share one 112-byte
/// field, so growing the name past what is left pushes the comment into a
/// `T_COMMENT` block, and shrinking it pulls the comment back inline and
/// frees the block. The comment survives both crossings unchanged, and
/// the block accounting says exactly one block moved.
#[test]
fn an_lnfs_rename_moves_the_comment_across_the_boundary_in_both_directions() {
    for variant in [Variant::OfsIntlLongname, Variant::FfsIntlLongname] {
        let comment = &b"seventy-nine characters is the most any Amiga comment has ever been"[..];
        let short = &b"Short"[..];
        let long: Vec<u8> = vec![b'L'; 100];

        let vol = populated(variant, 512, 1760, |pop| {
            let root = pop.root_lba();
            pop.create_file(root, short, &Metadata::new().comment(comment), b"body")
                .unwrap();
        });
        let root = vol.root_lba();
        let mut vol = vol;
        let inline = allocated_set(&mut vol).len();
        {
            let e = vol.lookup(root, short).unwrap().unwrap();
            assert_eq!(e.comment_block, 0, "the comment should start inline");
            assert_eq!(vol.comment(&e).unwrap(), comment);
        }

        // Inline -> overflow.
        let mut vol = mutating(vol, |m| {
            m.rename(root, short, root, &long).unwrap();
        });
        assert_clean(&mut vol, &format!("{variant:?} after growing the name"));
        let e = vol.lookup(root, &long).unwrap().unwrap();
        assert_ne!(e.comment_block, 0, "the comment should have moved out");
        assert!(e.comment.is_empty(), "the inline comment must be empty");
        assert_eq!(vol.comment(&e).unwrap(), comment);
        assert_eq!(allocated_set(&mut vol).len(), inline + 1);

        // Overflow -> inline, and the block comes back.
        let mut vol = mutating(vol, |m| {
            m.rename(root, &long, root, short).unwrap();
        });
        assert_clean(&mut vol, &format!("{variant:?} after shrinking the name"));
        let e = vol.lookup(root, short).unwrap().unwrap();
        assert_eq!(e.comment_block, 0);
        assert_eq!(vol.comment(&e).unwrap(), comment);
        assert_eq!(vol.read_file(e.lba).unwrap(), b"body");
        assert_eq!(allocated_set(&mut vol).len(), inline);
    }
}

#[test]
fn set_metadata_changes_four_fields_in_place_on_every_variant() {
    let comment = &b"a replacement comment"[..];
    let date = DateStamp {
        days: 9000,
        mins: 61,
        ticks: 25,
    };
    for variant in ALL_VARIANTS {
        let vol = mutable(variant, 512, 1760);
        let root = vol.root_lba();
        let mut vol = mutating(vol, |m| {
            let small = m.volume().lookup(root, b"Small").unwrap().unwrap().lba;
            let devs = m.volume().lookup(root, b"Devs").unwrap().unwrap().lba;
            m.set_metadata(
                small,
                &MetaUpdate::new()
                    .protection(0x1234_5678)
                    .comment(comment)
                    .date(date)
                    .owner(0x0007_0009),
            )
            .unwrap();
            // Directories too, and one field at a time is a different
            // path from all four at once.
            m.set_metadata(devs, &MetaUpdate::new().protection(0xF))
                .unwrap();
        });

        assert_clean(&mut vol, &format!("{variant:?} after set_metadata"));
        let small = vol.lookup(root, b"Small").unwrap().unwrap();
        assert_eq!(small.protection, 0x1234_5678);
        assert_eq!(small.owner, 0x0007_0009);
        assert_eq!(small.date, date);
        assert_eq!(small.uid(), 7);
        assert_eq!(small.gid(), 9);
        assert_eq!(vol.comment(&small).unwrap(), comment);
        assert_eq!(vol.read_file(small.lba).unwrap(), b"hello");
        let devs = vol.lookup(root, b"Devs").unwrap().unwrap();
        assert_eq!(devs.protection, 0xF);
        assert!(vol.comment(&devs).unwrap().is_empty());
    }
}

/// On LNFS, setting a long comment on a long-named entry has to allocate
/// the overflow block, and clearing it has to give the block back.
#[test]
fn set_metadata_allocates_and_frees_an_lnfs_comment_block() {
    let long: Vec<u8> = vec![b'N'; 100];
    let comment = &b"long enough not to fit beside a hundred-byte name"[..];
    let vol = populated(Variant::FfsIntlLongname, 512, 1760, |pop| {
        let root = pop.root_lba();
        pop.create_file(root, &long, &Metadata::new(), b"body")
            .unwrap();
    });
    let root = vol.root_lba();
    let mut vol = vol;
    let bare = allocated_set(&mut vol).len();

    let mut vol = mutating(vol, |m| {
        let e = m.volume().lookup(root, &long).unwrap().unwrap().lba;
        m.set_metadata(e, &MetaUpdate::new().comment(comment))
            .unwrap();
    });
    assert_clean(&mut vol, "after setting an overflowing comment");
    let e = vol.lookup(root, &long).unwrap().unwrap();
    assert_ne!(e.comment_block, 0);
    assert_eq!(vol.comment(&e).unwrap(), comment);
    assert_eq!(allocated_set(&mut vol).len(), bare + 1);

    let mut vol = mutating(vol, |m| {
        let e = m.volume().lookup(root, &long).unwrap().unwrap().lba;
        m.set_metadata(e, &MetaUpdate::new().comment(b"")).unwrap();
    });
    assert_clean(&mut vol, "after clearing it again");
    let e = vol.lookup(root, &long).unwrap().unwrap();
    assert_eq!(e.comment_block, 0);
    assert!(vol.comment(&e).unwrap().is_empty());
    assert_eq!(allocated_set(&mut vol).len(), bare);
}

/// PLAN's rule for dircaches in this milestone is "update or clear, never
/// leave stale", and `validate()` is what decides whether it was kept:
/// every one of the six ways a cache can disagree with its chains is a
/// `DircacheStale` finding, and after every operation here there are none.
#[test]
fn every_mutation_leaves_the_dircache_agreeing_with_the_chains() {
    for variant in [Variant::OfsIntlDircache, Variant::FfsIntlDircache] {
        for bs in [512usize, 4096] {
            let nblocks = 8 * 1024 * 1024 / bs as u64;
            let vol = mutable(variant, bs, nblocks);
            let root = vol.root_lba();

            // Enough entries in one directory to spill the cache chain
            // into a second block and back out of it again: a 512-byte
            // block holds about nineteen records.
            let mut vol = mutating(vol, |m| {
                for i in 0..30u32 {
                    let name = format!("entry{i:04}");
                    m.create_file(root, name.as_bytes(), &Metadata::new(), b"x")
                        .unwrap();
                }
            });
            assert_clean(&mut vol, &format!("{variant:?} @{bs} after 30 creates"));
            let cache = vol.read_dircache(root).unwrap();
            assert!(
                cache.blocks.len() > 1 || bs > 512,
                "the cache chain should have spilled at 512 bytes"
            );
            assert_eq!(cache.records.len(), vol.read_dir(root).unwrap().len());

            let mut vol = mutating(vol, |m| {
                for i in 0..25u32 {
                    let name = format!("entry{i:04}");
                    m.delete(root, name.as_bytes()).unwrap();
                }
                m.rename(root, b"entry0029", root, b"renamed").unwrap();
                let e = m.volume().lookup(root, b"Small").unwrap().unwrap().lba;
                m.set_metadata(e, &MetaUpdate::new().comment(b"cached too"))
                    .unwrap();
            });
            assert_clean(&mut vol, &format!("{variant:?} @{bs} after deletes"));

            // The cache is not merely *consistent*, it is the listing: a
            // record per entry, with the metadata a `List` would print.
            let entries = vol.read_dir(root).unwrap();
            let cache = vol.read_dircache(root).unwrap();
            assert_eq!(cache.records.len(), entries.len());
            let small = entries.iter().find(|e| e.name == b"Small").unwrap();
            let record = cache
                .records
                .iter()
                .find(|r| r.entry as u64 == small.lba)
                .expect("Small is cached");
            assert_eq!(record.comment, b"cached too");
            assert_eq!(record.name, b"Small");
            assert_eq!(record.size, small.byte_size);
        }
    }
}

/// With a clock set, the parent's DateStamp and the root's `disk_altered`
/// move on every change -- the rule amitools' xdftool follows. Without
/// one, nothing is written, because a `no_std` crate has no "now" and an
/// invented date is worse than an old true one.
#[test]
fn dates_move_only_when_the_session_has_been_given_a_clock() {
    let now = DateStamp {
        days: 12345,
        mins: 678,
        ticks: 9,
    };
    for variant in ALL_VARIANTS {
        let vol = mutable(variant, 512, 1760);
        let root = vol.root_lba();
        let before = (vol.root().dir_altered, vol.root().disk_altered);

        // No clock: the dates are exactly as they were.
        let mut vol = mutating(vol, |m| {
            m.create_file(root, b"Undated", &Metadata::new(), b"x")
                .unwrap();
        });
        assert_eq!((vol.root().dir_altered, vol.root().disk_altered), before);
        let devs_before = vol.lookup(root, b"Devs").unwrap().unwrap().date;

        let mut vol = {
            let mut m = Mutator::open(vol).unwrap().clock(now);
            let devs = m.volume().lookup(root, b"Devs").unwrap().unwrap().lba;
            m.create_file(devs, b"Dated", &Metadata::new(), b"x")
                .unwrap();
            m.into_volume()
        };
        // The parent moved, and so did the volume's own "altered".
        let devs = vol.lookup(root, b"Devs").unwrap().unwrap();
        assert_ne!(devs.date, devs_before, "{variant:?}: parent not stamped");
        assert_eq!(devs.date, now);
        assert_eq!(vol.root().disk_altered, now);
        // The root's "directory altered" only moves when the root is the
        // directory that changed.
        assert_eq!(vol.root().dir_altered, before.0);
        assert_clean(&mut vol, &format!("{variant:?} with a clock"));

        let mut vol = {
            let mut m = Mutator::open(vol).unwrap().clock(now);
            m.delete(root, b"Undated").unwrap();
            m.into_volume()
        };
        assert_eq!(vol.root().dir_altered, now);
        assert_eq!(vol.root().disk_made, {
            let mut fresh = mutable(variant, 512, 1760);
            let made = fresh.root().disk_made;
            let _ = fresh.validate();
            made
        });
        assert_clean(&mut vol, &format!("{variant:?} after a dated delete"));
    }
}

/// Names and comments are checked against the variant's own limits before
/// a block is allocated, so a refusal costs nothing and leaves nothing.
#[test]
fn a_mutator_checks_names_and_comments_before_it_allocates() {
    for variant in [Variant::FfsIntl, Variant::FfsIntlLongname] {
        let vol = mutable(variant, 512, 1760);
        let root = vol.root_lba();
        let mut vol = vol;
        let before = allocated_set(&mut vol);
        let mut m = Mutator::open(vol).unwrap();
        let meta = Metadata::new();

        assert!(matches!(
            m.create_file(root, b"", &meta, b""),
            Err(MutateError::NameEmpty)
        ));
        assert!(matches!(
            m.create_file(root, b"has/slash", &meta, b""),
            Err(MutateError::NameInvalidByte { byte: b'/', .. })
        ));
        assert!(matches!(
            m.create_file(root, b"colon:here", &meta, b""),
            Err(MutateError::NameInvalidByte { byte: b':', .. })
        ));
        let over = vec![b'x'; m.max_name_len() + 1];
        assert!(matches!(
            m.create_file(root, &over, &meta, b""),
            Err(MutateError::NameTooLong { .. })
        ));
        let comment = vec![b'c'; COMMENT_MAX + 1];
        assert!(matches!(
            m.create_file(root, b"Fine", &Metadata::new().comment(&comment), b""),
            Err(MutateError::CommentTooLong { .. })
        ));
        assert!(matches!(
            m.create_file(root, b"Small", &meta, b""),
            Err(MutateError::DuplicateName { .. })
        ));
        assert!(matches!(
            m.delete(root, b"NoSuchThing"),
            Err(MutateError::NotFound { .. })
        ));
        assert!(matches!(
            m.set_metadata(root, &MetaUpdate::new().protection(0)),
            Err(MutateError::IsRoot { .. })
        ));
        // Creating inside a file, rather than a directory.
        let small = m.volume().lookup(root, b"Small").unwrap().unwrap().lba;
        assert!(matches!(
            m.create_dir(small, b"Nope", &meta),
            Err(MutateError::NotADirectory { .. })
        ));

        let mut vol = m.into_volume();
        assert_eq!(
            allocated_set(&mut vol),
            before,
            "{variant:?}: a refusal allocated a block"
        );
        assert_clean(&mut vol, &format!("{variant:?} after refusals"));
    }
}

#[test]
fn a_mutator_refuses_a_volume_whose_bitmap_is_mid_update() {
    // Repair's territory, not a mutator's guess -- and the refusal comes
    // from the allocator, which is the one place it is decided.
    let nblocks = 1760;
    let disk = MemDisk::blank(512, nblocks);
    let opts = FormatOptions::new(Variant::FfsIntl, nblocks, b"Interrupted");
    let mut pop = Populator::new(disk, &opts).unwrap();
    let root = pop.root_lba();
    pop.create_file(root, b"Half", &Metadata::new(), &pattern(5000))
        .unwrap();
    let disk = pop.abandon();

    let vol = Volume::open_with(disk, None, nblocks, 2).unwrap();
    assert!(matches!(
        Mutator::open(vol),
        Err(MutateError::Alloc(AllocError::BitmapInvalid))
    ));
}

/// The crash-shape claim for this wave, checked at every point rather
/// than at one convenient one: stop the medium after *n* writes, for
/// every n a mutation session takes, and the damage must always be
/// leak-shaped.
///
/// `OrphanBlock` is allowed -- a block marked allocated that nothing
/// reaches is space lost and nothing worse. `ReachableButFree` is never
/// allowed: it is the state in which the next allocation hands a block to
/// a second owner. `DircacheStale` is allowed on the two variants that
/// have caches, and only there: a cache is advisory, a half-written one
/// is what every dircache-unaware tool leaves behind, and `validate()`
/// reporting it is the mechanism by which it gets rebuilt. Everything
/// else -- a bad checksum on a reachable block, a half-linked entry, a
/// chain that will not walk -- is a failure.
#[test]
fn an_interrupted_mutation_leaks_and_never_double_allocates() {
    for variant in [
        Variant::Ffs,
        Variant::FfsIntlDircache,
        Variant::FfsIntlLongname,
    ] {
        let nblocks = 1760;
        let session = |vol: Volume<MemDisk>| -> (Volume<MemDisk>, bool) {
            let mut m = match Mutator::open(vol) {
                Ok(m) => m,
                Err(_) => unreachable!("the bitmap is valid at the start of every prefix"),
            };
            let root = m.volume().root_lba();
            let comment = &b"a comment that will not fit beside a long name at all, honestly"[..];
            let meta = Metadata::new().comment(comment);
            let long: Vec<u8> = if variant.has_long_names() {
                vec![b'L'; 100]
            } else {
                b"Created".to_vec()
            };
            let ok = m.create_dir(root, b"Fresh", &Metadata::new()).is_ok()
                & m.create_file(root, &long, &meta, &pattern(4000)).is_ok()
                & m.rename(root, b"Small", root, b"Renamed").is_ok()
                & m.delete(root, b"Payload").is_ok();
            (m.into_volume(), ok)
        };

        let total = {
            let mut vol = mutable(variant, 512, nblocks);
            vol.source_mut().clear_log();
            let (vol, ok) = session(vol);
            assert!(ok, "{variant:?}: the uninterrupted session must succeed");
            vol.into_inner().write_log().len()
        };
        assert!(total > 10, "{variant:?}: the session writes {total} blocks");

        let mut crashed = 0;
        for n in 0..=total {
            let mut vol = mutable(variant, 512, nblocks);
            vol.source_mut().clear_log();
            vol.source_mut().fail_after(n);
            let (vol, ok) = session(vol);
            if !ok {
                crashed += 1;
            }
            let disk = vol.into_inner();

            let mut vol = Volume::open_with(disk, None, nblocks, 2).unwrap();
            let report = vol.validate();
            for finding in &report.findings {
                let excusable = matches!(finding, Finding::OrphanBlock { .. })
                    || (variant.has_dircache() && matches!(finding, Finding::DircacheStale { .. }));
                assert!(
                    excusable,
                    "{variant:?}: crash after {n} writes left {finding}"
                );
            }
            assert_eq!(
                report.summary.reachable_but_free, 0,
                "{variant:?}: crash after {n} writes"
            );

            // Every entry still reachable is *whole*: the walk above
            // already refused a bad checksum or an unfollowable chain, and
            // this reads the bytes of whatever files are still there.
            let root = vol.root_lba();
            for entry in vol.read_dir(root).unwrap() {
                if entry.kind == EntryKind::File {
                    vol.read_file(entry.lba).unwrap();
                }
            }
        }
        assert!(crashed > 0, "{variant:?}: no prefix actually failed");
    }
}

// ---------------------------------------------------------------------------
// Mutation against a model
// ---------------------------------------------------------------------------

/// What the model remembers about one entry. Everything here is
/// independently checkable on the volume, which is the point: the model
/// is a second implementation of "what should be on the disk", written in
/// the least similar way available.
#[derive(Clone, Debug)]
struct MEntry {
    name: Vec<u8>,
    lba: u64,
    kind: EntryKind,
    data: Vec<u8>,
    comment: Vec<u8>,
    protection: u32,
}

/// The tree as a `BTreeMap` of `BTreeMap`s — deterministic iteration, so
/// a failing seed reproduces exactly.
struct Model {
    /// Directory block -> folded name -> entry.
    dirs: std::collections::BTreeMap<u64, std::collections::BTreeMap<Vec<u8>, MEntry>>,
    /// Directory block -> its parent, for the ancestry check.
    parent: std::collections::BTreeMap<u64, u64>,
    root: u64,
}

impl Model {
    fn new(root: u64) -> Self {
        let mut dirs = std::collections::BTreeMap::new();
        dirs.insert(root, std::collections::BTreeMap::new());
        Self {
            dirs,
            parent: std::collections::BTreeMap::new(),
            root,
        }
    }

    fn dir_list(&self) -> Vec<u64> {
        self.dirs.keys().copied().collect()
    }

    /// Every entry, as (parent, folded key), for picking a victim.
    fn entry_list(&self) -> Vec<(u64, Vec<u8>)> {
        let mut v = Vec::new();
        for (&dir, entries) in &self.dirs {
            for key in entries.keys() {
                v.push((dir, key.clone()));
            }
        }
        v
    }

    /// Is `maybe_below` inside `dir` (or `dir` itself)? The check
    /// `rename` makes on the volume, made again here so the test only
    /// ever asks for renames that must succeed.
    fn inside(&self, dir: u64, maybe_below: u64) -> bool {
        let mut here = maybe_below;
        loop {
            if here == dir {
                return true;
            }
            if here == self.root {
                return false;
            }
            match self.parent.get(&here) {
                Some(&p) => here = p,
                None => return false,
            }
        }
    }
}

fn folded(name: &[u8], fold: fn(u8) -> u8) -> Vec<u8> {
    name.iter().map(|&c| fold(c)).collect()
}

/// A name from an alphabet chosen to exercise both fold tables: ASCII
/// letters in both cases, digits, and the two Latin-1 letters that fold
/// only on the international variants.
fn mutation_name(rng: &mut Rng, max: usize) -> Vec<u8> {
    const ALPHABET: &[u8] = b"abcdefgHIJKLM0123\xE9\xC9-";
    // Lengths that cross the 30-byte classic limit where the variant
    // allows it, and cluster short where collisions are likely -- short
    // names out of a small alphabet is how hash collisions happen.
    let span = if rng.below(4) == 0 { max } else { max.min(12) };
    let len = 1 + rng.below(span);
    (0..len)
        .map(|_| ALPHABET[rng.below(ALPHABET.len())])
        .collect()
}

/// Read the volume back and compare it, entry by entry and byte by byte,
/// to the model.
fn verify_model(vol: &mut Volume<MemDisk>, model: &Model, what: &str) {
    for (&dir, entries) in &model.dirs {
        let listed = vol.read_dir(dir).expect("read_dir");
        assert_eq!(
            listed.len(),
            entries.len(),
            "{what}: directory {dir} holds {:?}, model says {:?}",
            listed.iter().map(|e| Latin1(&e.name)).collect::<Vec<_>>(),
            entries
                .values()
                .map(|e| Latin1(&e.name))
                .collect::<Vec<_>>()
        );
        for e in entries.values() {
            let found = vol
                .lookup(dir, &e.name)
                .expect("lookup")
                .unwrap_or_else(|| panic!("{what}: {:?} not found in {dir}", Latin1(&e.name)));
            assert_eq!(found.lba, e.lba, "{what}: {:?} moved", Latin1(&e.name));
            // Byte for byte, not under the fold table: case is preserved
            // on disk, and a writer that upper-cased what it stored would
            // still pass every lookup.
            assert_eq!(found.name, e.name, "{what}: case not preserved");
            assert_eq!(found.kind, e.kind, "{what}: {:?}", Latin1(&e.name));
            assert_eq!(found.parent as u64, dir, "{what}: {:?}", Latin1(&e.name));
            assert_eq!(
                found.protection,
                e.protection,
                "{what}: {:?} protection",
                Latin1(&e.name)
            );
            assert_eq!(
                vol.comment(&found).expect("comment"),
                e.comment,
                "{what}: {:?} comment",
                Latin1(&e.name)
            );
            if e.kind == EntryKind::File {
                assert_eq!(found.byte_size as usize, e.data.len());
                assert_eq!(
                    vol.read_file(found.lba).expect("read_file"),
                    e.data,
                    "{what}: {:?} contents",
                    Latin1(&e.name)
                );
            }
        }
    }
}

/// The property test for the whole wave: a random interleave of creates,
/// deletes, renames and metadata changes against a model of the same
/// tree, on every variant at two block sizes. After each batch the whole
/// tree is compared field by field and byte by byte, and the volume must
/// validate with zero findings — no stale dircache, no leaked block, no
/// entry in the wrong chain.
///
/// Only *legal* operations are issued: the model knows which names are
/// taken, which directories are empty and which moves would make a cycle,
/// so every call is expected to succeed and a refusal is a failure. The
/// refusals have their own tests; this one is about the volume ending up
/// exactly as asked.
#[test]
fn a_random_interleave_of_mutations_agrees_with_a_model() {
    for (v, variant) in ALL_VARIANTS.into_iter().enumerate() {
        for (b, bs) in [512usize, 4096].into_iter().enumerate() {
            let nblocks = 8 * 1024 * 1024 / bs as u64;
            let mut rng = Rng::new(0x03DE_1000 + (v * 16 + b) as u64);
            let fold = variant.fold();
            let max_name = if variant.has_long_names() {
                MAX_NAME_LONG
            } else {
                MAX_NAME_CLASSIC
            };

            let vol = populated(variant, bs, nblocks, |_| {});
            let root = vol.root_lba();
            let empty = {
                let mut v = vol;
                let set = allocated_set(&mut v);
                (v, set)
            };
            let (vol, before) = empty;
            let mut model = Model::new(root);
            let mut m = Mutator::open(vol).expect("mutator");
            let mut counts = [0usize; 5];

            for batch in 0..5 {
                for _ in 0..20 {
                    let dirs = model.dir_list();
                    let dir = dirs[rng.below(dirs.len())];
                    match rng.below(10) {
                        // Create a file.
                        0..=3 => {
                            let name = mutation_name(&mut rng, max_name);
                            let key = folded(&name, fold);
                            if model.dirs[&dir].contains_key(&key) {
                                continue;
                            }
                            let data = pattern(rng.below(3000));
                            let comment = if rng.below(3) == 0 {
                                b"a comment, which on LNFS may not fit beside the name".to_vec()
                            } else {
                                Vec::new()
                            };
                            let protection = rng.next() as u32;
                            let meta = Metadata::new().comment(&comment).protection(protection);
                            let lba = m
                                .create_file(dir, &name, &meta, &data)
                                .expect("create_file");
                            model.dirs.get_mut(&dir).unwrap().insert(
                                key,
                                MEntry {
                                    name,
                                    lba,
                                    kind: EntryKind::File,
                                    data,
                                    comment,
                                    protection,
                                },
                            );
                            counts[0] += 1;
                        }
                        // Create a directory.
                        4..=5 => {
                            let name = mutation_name(&mut rng, max_name);
                            let key = folded(&name, fold);
                            if model.dirs[&dir].contains_key(&key) {
                                continue;
                            }
                            let protection = rng.next() as u32;
                            let meta = Metadata::new().protection(protection);
                            let lba = m.create_dir(dir, &name, &meta).expect("create_dir");
                            model.dirs.get_mut(&dir).unwrap().insert(
                                key,
                                MEntry {
                                    name,
                                    lba,
                                    kind: EntryKind::Directory,
                                    data: Vec::new(),
                                    comment: Vec::new(),
                                    protection,
                                },
                            );
                            model.dirs.insert(lba, Default::default());
                            model.parent.insert(lba, dir);
                            counts[1] += 1;
                        }
                        // Delete something deletable.
                        6..=7 => {
                            let all = model.entry_list();
                            if all.is_empty() {
                                continue;
                            }
                            let (dir, key) = all[rng.below(all.len())].clone();
                            let e = model.dirs[&dir][&key].clone();
                            if e.kind == EntryKind::Directory && !model.dirs[&e.lba].is_empty() {
                                continue;
                            }
                            m.delete(dir, &e.name).expect("delete");
                            model.dirs.get_mut(&dir).unwrap().remove(&key);
                            if e.kind == EntryKind::Directory {
                                model.dirs.remove(&e.lba);
                                model.parent.remove(&e.lba);
                            }
                            counts[2] += 1;
                        }
                        // Rename, sometimes to another directory.
                        8 => {
                            let all = model.entry_list();
                            if all.is_empty() {
                                continue;
                            }
                            let (from, key) = all[rng.below(all.len())].clone();
                            let e = model.dirs[&from][&key].clone();
                            let dirs = model.dir_list();
                            let to = dirs[rng.below(dirs.len())];
                            if e.kind == EntryKind::Directory && model.inside(e.lba, to) {
                                continue;
                            }
                            let name = mutation_name(&mut rng, max_name);
                            let new_key = folded(&name, fold);
                            if model.dirs[&to].contains_key(&new_key)
                                && !(to == from && new_key == key)
                            {
                                continue;
                            }
                            m.rename(from, &e.name, to, &name).expect("rename");
                            model.dirs.get_mut(&from).unwrap().remove(&key);
                            let mut moved = e.clone();
                            moved.name = name;
                            if moved.kind == EntryKind::Directory {
                                model.parent.insert(moved.lba, to);
                            }
                            model.dirs.get_mut(&to).unwrap().insert(new_key, moved);
                            counts[3] += 1;
                        }
                        // Change metadata.
                        _ => {
                            let all = model.entry_list();
                            if all.is_empty() {
                                continue;
                            }
                            let (dir, key) = all[rng.below(all.len())].clone();
                            let protection = rng.next() as u32;
                            let comment: Vec<u8> = match rng.below(3) {
                                0 => Vec::new(),
                                1 => b"short".to_vec(),
                                _ => vec![b'c'; COMMENT_MAX],
                            };
                            let e = model.dirs.get_mut(&dir).unwrap().get_mut(&key).unwrap();
                            m.set_metadata(
                                e.lba,
                                &MetaUpdate::new().protection(protection).comment(&comment),
                            )
                            .expect("set_metadata");
                            e.protection = protection;
                            e.comment = comment;
                            counts[4] += 1;
                        }
                    }
                }

                let what = format!("{variant:?} @{bs} batch {batch}");
                verify_model(m.volume(), &model, &what);
                assert_clean(m.volume(), &what);
            }

            // Every operation was exercised, on every variant: a seed
            // that quietly stopped deleting would otherwise still pass.
            for (i, n) in counts.iter().enumerate() {
                assert!(*n > 0, "{variant:?} @{bs}: operation {i} never ran");
            }

            // And the whole tree back off again returns the volume to the
            // blocks a freshly formatted one had -- the strongest form of
            // "no leaks in the happy path" available.
            let mut vol = m.into_volume();
            let mut m = Mutator::open(vol).expect("mutator");
            loop {
                let all = model.entry_list();
                let mut removed = false;
                for (dir, key) in all {
                    let e = model.dirs[&dir][&key].clone();
                    if e.kind == EntryKind::Directory && !model.dirs[&e.lba].is_empty() {
                        continue;
                    }
                    m.delete(dir, &e.name).expect("delete");
                    model.dirs.get_mut(&dir).unwrap().remove(&key);
                    if e.kind == EntryKind::Directory {
                        model.dirs.remove(&e.lba);
                        model.parent.remove(&e.lba);
                    }
                    removed = true;
                }
                if !removed {
                    break;
                }
            }
            vol = m.into_volume();
            assert_clean(&mut vol, &format!("{variant:?} @{bs} emptied"));
            assert_eq!(
                allocated_set(&mut vol),
                before,
                "{variant:?} @{bs}: emptying the volume did not return every block"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Ranged reads
// ---------------------------------------------------------------------------

/// A volume with one file big enough to have an extension block, and the
/// bytes that went into it.
fn ranged_volume(
    variant: Variant,
    bs: usize,
    size: usize,
) -> (Volume<MemDisk>, Vec<u8>, FileChain) {
    let nblocks = 8 * 1024 * 1024 / bs as u64;
    let data = pattern(size);
    let payload = data.clone();
    let mut vol = populated(variant, bs, nblocks, |pop| {
        let root = pop.root_lba();
        pop.create_file(root, b"Big", &amiga_ffs::Metadata::new(), &payload)
            .unwrap();
    });
    let root = vol.root_lba();
    let lba = vol.lookup(root, b"Big").unwrap().unwrap().lba;
    let chain = vol.file_chain(lba).unwrap();
    (vol, data, chain)
}

/// Every shape of range there is, against the bytes a whole-file read
/// gives: within one block, exactly one block, spanning a boundary,
/// crossing into extension-block territory, and clamped at the end.
///
/// The interesting half is the clamp. `byte_size` is the authority for
/// where a file stops -- the last data block has capacity past it, and a
/// ranged read that returned the block's tail would invent data that no
/// whole-file read of the same volume ever produces.
#[test]
fn a_ranged_read_returns_exactly_what_a_whole_file_read_would() {
    for variant in [Variant::Ofs, Variant::Ffs] {
        for bs in [512usize, 4096] {
            let payload = data_payload_size(bs, variant.is_ffs());
            let slots = hash_table_size(bs) as usize;
            // Past the header's own table by four blocks, so a range can
            // cross into the extension block and sit wholly inside it.
            let size = (slots + 3) * payload + 17;
            let (mut vol, data, chain) = ranged_volume(variant, bs, size);
            assert!(!chain.extensions.is_empty(), "{variant:?} @{bs}");

            let p = payload as u64;
            let ext = slots as u64 * p;
            let cases: [(u64, usize); 12] = [
                (0, size),              // the whole file, the long way round
                (0, payload),           // exactly the first block
                (p, payload),           // exactly a block, not the first
                (3, 10),                // wholly within one block
                (p - 5, 10),            // spanning a block boundary
                (p / 2, payload * 3),   // partial, whole, partial
                (p * 2, payload * 2),   // two whole blocks
                (ext - 7, 20),          // crossing into the extension block
                (ext + 1, payload * 2), // wholly inside it
                (size as u64 - 3, 100), // clamped at the end
                (size as u64 - 1, 1),   // the last byte
                (0, size + 4096),       // a buffer longer than the file
            ];
            for (off, len) in cases {
                let what = format!("{variant:?} @{bs}: {len} bytes at {off}");
                let mut buf = vec![0xAAu8; len];
                let got = vol.read_range(&chain, off, &mut buf).unwrap();
                let want = &data[off as usize..(off as usize + len).min(size)];
                assert_eq!(got, want.len(), "{what}: count");
                assert_eq!(&buf[..got], want, "{what}: bytes");
                // Nothing past the returned count is written, so a caller
                // that trusts the count is never reading its own padding.
                assert!(buf[got..].iter().all(|&b| b == 0xAA), "{what}: overrun");
            }
        }
    }
}

/// End of file is a length, not a failure: read(2) returns 0 there and so
/// does this. The distinction matters to a caller looping until it gets
/// zero, which is every caller.
#[test]
fn a_ranged_read_at_or_past_the_end_of_file_returns_zero() {
    for variant in [Variant::Ofs, Variant::Ffs] {
        let size = 5000;
        let (mut vol, _, chain) = ranged_volume(variant, 512, size);
        let mut buf = [0u8; 64];
        for off in [size as u64, size as u64 + 1, u64::MAX / 2] {
            assert_eq!(vol.read_range(&chain, off, &mut buf).unwrap(), 0);
        }
        // A zero-length buffer inside the file falls out of the same
        // clamp rather than being a case of its own.
        assert_eq!(vol.read_range(&chain, 0, &mut []).unwrap(), 0);

        // And an empty file is entirely past its own end.
        let nblocks = 1760;
        let mut vol = populated(variant, 512, nblocks, |pop| {
            let root = pop.root_lba();
            pop.create_file(root, b"Empty", &amiga_ffs::Metadata::new(), b"")
                .unwrap();
        });
        let root = vol.root_lba();
        let lba = vol.lookup(root, b"Empty").unwrap().unwrap().lba;
        let chain = vol.file_chain(lba).unwrap();
        assert_eq!(vol.read_range(&chain, 0, &mut buf).unwrap(), 0);
    }
}

/// The whole claim of a ranged read: it walks the blocks the range covers
/// and no others. The bytes come back right either way, so the only way
/// to assert it is to count the reads.
#[test]
fn a_ranged_read_touches_only_the_blocks_the_range_covers() {
    for variant in [Variant::Ofs, Variant::Ffs] {
        let bs = 512;
        let payload = data_payload_size(bs, variant.is_ffs()) as u64;
        let slots = hash_table_size(bs) as u64;
        let size = ((slots + 3) * payload + 17) as usize;
        let (mut vol, _, chain) = ranged_volume(variant, bs, size);
        let mut buf = vec![0u8; 4 * payload as usize];

        for (off, len, blocks) in [
            (0u64, 1usize, 1usize),
            (payload - 1, 2, 2),
            (payload * 40 + 3, 10, 1),
            // Deep inside the extension block's territory: still one
            // read, because the sequence number is arithmetic and nothing
            // has to be walked from block 1 to know it.
            (slots * payload + payload + 5, 20, 1),
            (payload * 3, 3 * payload as usize, 3),
        ] {
            vol.source_mut().clear_read_log();
            let got = vol.read_range(&chain, off, &mut buf[..len]).unwrap();
            assert_eq!(got, len);
            assert_eq!(
                vol.source_mut().read_log().len(),
                blocks,
                "{variant:?}: {len} bytes at {off}"
            );
        }
    }
}

/// A ranged read verifies the OFS headers of the blocks it touches --
/// and, because it touches only those, a range that avoids a corrupt
/// block still succeeds. Both halves matter: the first is the check not
/// being skipped for speed, the second is the range genuinely being a
/// range.
#[test]
fn a_ranged_read_verifies_the_ofs_headers_of_the_blocks_it_touches() {
    let bs = 512;
    let payload = data_payload_size(bs, false);
    let size = 10 * payload;
    let (mut vol, _, chain) = ranged_volume(Variant::Ofs, bs, size);

    // Data block 5 claims to be somebody else's block 1.
    let victim = chain.blocks[4] as u64;
    let mut buf = vol.source_mut().block(victim).to_vec();
    buf[OFF_DATA_SEQ..OFF_DATA_SEQ + 4].copy_from_slice(&1u32.to_be_bytes());
    buf[OFF_CHECKSUM..OFF_CHECKSUM + 4].copy_from_slice(&0u32.to_be_bytes());
    let ck = checksum_compute(&buf, CHECKSUM_INDEX);
    buf[OFF_CHECKSUM..OFF_CHECKSUM + 4].copy_from_slice(&ck.to_be_bytes());
    vol.source_mut().poke_block(victim, &buf);

    let mut out = vec![0u8; 2 * payload];
    // A range that stops before it is fine...
    assert_eq!(
        vol.read_range(&chain, 0, &mut out[..payload]).unwrap(),
        payload
    );
    // ...one that touches it is refused, by name.
    let err = vol
        .read_range(&chain, 4 * payload as u64, &mut out[..8])
        .unwrap_err();
    assert!(
        matches!(
            err,
            amiga_ffs::read::Error::DataBlockSequence {
                found: 1,
                expected: 5,
                ..
            }
        ),
        "{err}"
    );
    // ...and so is one that merely runs through it on its way somewhere
    // else, because a block in a range is a block that gets checked.
    assert!(
        vol.read_range(&chain, 3 * payload as u64, &mut out)
            .is_err(),
        "a range spanning the bad block must refuse"
    );
}

// ---------------------------------------------------------------------------
// File write, append and truncate
// ---------------------------------------------------------------------------

/// Bytes that say which operation wrote them, so an overwrite that landed
/// at the wrong offset shows up as the wrong *tag* rather than as a
/// coincidence of two identical patterns.
fn stamped(tag: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(tag))
        .collect()
}

/// The model of a file: a `Vec<u8>` with `resize`'s zero-fill, which is
/// exactly the semantics the crate implements and the reason it is worth
/// stating that way.
fn model_write(model: &mut Vec<u8>, off: u64, data: &[u8]) {
    let end = off as usize + data.len();
    if model.len() < end {
        model.resize(end, 0);
    }
    model[off as usize..end].copy_from_slice(data);
}

/// A volume with the usual tree plus one file to rewrite.
fn writable(variant: Variant, bs: usize, nblocks: u64, initial: &[u8]) -> Volume<MemDisk> {
    let initial = initial.to_vec();
    populated(variant, bs, nblocks, |pop| {
        allocatable_tree(pop);
        let root = pop.root_lba();
        pop.create_file(root, b"Doc", &amiga_ffs::Metadata::new(), &initial)
            .unwrap();
    })
}

/// Read the file back whole and compare it to the model, then check the
/// volume is still one: clean, with reachable and allocated agreeing, and
/// the tree that was already there untouched.
fn assert_file_model(vol: &mut Volume<MemDisk>, want: &[u8], what: &str) {
    let root = vol.root_lba();
    let e = vol.lookup(root, b"Doc").unwrap().expect("Doc");
    assert_eq!(e.byte_size as usize, want.len(), "{what}: byte_size");
    assert_eq!(vol.read_file(e.lba).unwrap(), want, "{what}: contents");
    assert_clean(vol, what);
    let report = vol.validate();
    assert_eq!(report.summary.reachable, report.summary.allocated, "{what}");
    assert_eq!(report.summary.orphans, 0, "{what}");
    assert_eq!(report.summary.reachable_but_free, 0, "{what}");
    assert_tree_intact(vol);
}

/// The matrix: every shape of write, append and truncate, on OFS and FFS,
/// at two block sizes, with a dircache variant and a long-name variant in
/// for the metadata that rides along -- each operation followed by a
/// byte-identical readback against a model `Vec<u8>` and a clean
/// `validate()`.
///
/// The sizes are all expressed in payload blocks and in the header's own
/// table size, because that is where the arithmetic can go wrong: a file
/// that never crosses the boundary between the header's table and its
/// first extension block proves nothing about either.
#[test]
fn write_append_and_truncate_track_a_model_file_on_every_shape_of_volume() {
    for variant in [
        Variant::Ofs,
        Variant::Ffs,
        Variant::OfsIntlDircache,
        Variant::FfsIntlLongname,
    ] {
        for bs in [512usize, 4096] {
            let nblocks = 8 * 1024 * 1024 / bs as u64;
            let p = data_payload_size(bs, variant.is_ffs()) as u64;
            let slots = hash_table_size(bs) as u64;
            // The first byte the header's own table cannot reach.
            let ext = slots * p;

            let initial = pattern(300);
            let mut model = initial.clone();
            let mut vol = writable(variant, bs, nblocks, &initial);

            let mut m = Mutator::open(vol).expect("mutator");
            let root = m.volume().root_lba();
            let mut step = 0u8;
            let mut run = |m: &mut Mutator<MemDisk>, model: &mut Vec<u8>, op: Op| {
                step = step.wrapping_add(1);
                let what = format!("{variant:?} @{bs} step {step} {op:?}");
                match op {
                    Op::Write(off, len) => {
                        let data = stamped(step, len);
                        let got = m.write_file(root, b"Doc", off, &data).expect(&what);
                        model_write(model, off, &data);
                        assert_eq!(got, model.len() as u64, "{what}: returned length");
                    }
                    Op::Append(len) => {
                        let data = stamped(step, len);
                        let at = model.len() as u64;
                        let got = m.append(root, b"Doc", &data).expect(&what);
                        model_write(model, at, &data);
                        assert_eq!(got, model.len() as u64, "{what}: returned length");
                    }
                    Op::Truncate(n) => {
                        m.truncate(root, b"Doc", n).expect(&what);
                        model.resize(n as usize, 0);
                    }
                }
                what
            };

            for op in [
                // Grow across the header table's last slot and three
                // blocks into the first extension block.
                Op::Append((ext + 3 * p) as usize - 300),
                // Partial first block, whole middle blocks, partial last.
                Op::Write(p / 2, 3 * p as usize),
                // A single byte, at the very start.
                Op::Write(0, 1),
                // Back to exactly the extension boundary: the file now
                // ends where the header's own table ends, so the whole
                // extension chain goes.
                Op::Truncate(ext),
                // ...and out again into it, to a mid-block length.
                Op::Truncate(ext + p / 3),
                // A write wholly inside the extension block's territory.
                Op::Write(ext + p, 200),
                // Shrink to the middle of a block: the block that is now
                // last has to have its OFS length corrected.
                Op::Truncate(p / 2),
                // Empty, then a write past the end -- the gap zero-fills,
                // because the format has no hole to leave.
                Op::Truncate(0),
                Op::Write(3 * p + 7, 100),
                // A no-op append, and a pure grow.
                Op::Append(0),
                Op::Truncate(5 * p),
            ] {
                let what = run(&mut m, &mut model, op);
                assert_file_model(m.volume(), &model, &what);
            }

            vol = m.into_volume();
            // The gap really is zeroes, not whatever the deleted blocks
            // held -- the volume was filled with 0xA5 before it was
            // formatted, so anything else would show.
            let doc = vol.lookup(root, b"Doc").unwrap().unwrap();
            let back = vol.read_file(doc.lba).unwrap();
            assert_eq!(back, model);
            assert!(back[..3 * p as usize + 7].iter().all(|&b| b == 0));
        }
    }
}

/// The operations a step of the matrix above can be.
#[derive(Debug, Clone, Copy)]
enum Op {
    /// Write `len` bytes at this offset.
    Write(u64, usize),
    /// Append `len` bytes.
    Append(usize),
    /// Set the length.
    Truncate(u64),
}

/// The refusals, and the one place the format's own field size shows
/// through.
#[test]
fn write_refuses_what_has_no_contents_of_its_own() {
    let vol = writable(Variant::FfsIntl, 512, 1760, b"hello");
    let root = vol.root_lba();
    let mut m = Mutator::open(vol).unwrap();

    // A directory has no data chain...
    let err = m.append(root, b"Devs", b"x").unwrap_err();
    assert!(
        matches!(err, MutateError::NotAFile { found: 2, .. }),
        "{err}"
    );
    // ...and a name that is not there is not there.
    let err = m.write_file(root, b"Nope", 0, b"x").unwrap_err();
    assert!(matches!(err, MutateError::NotFound { .. }), "{err}");
    // The format records a length in 32 bits, and says so rather than
    // truncating it.
    let err = m.truncate(root, b"Doc", 1 << 32).unwrap_err();
    assert!(matches!(err, MutateError::FileTooLarge { .. }), "{err}");
    assert!(err.to_string().contains("byte_size"), "{err}");
}

/// The caveat, asserted rather than merely written down: an interrupted
/// **overwrite** leaves old bytes or new bytes, per block, and nothing
/// structural at all.
///
/// This is the one operation where a content block and the file's
/// metadata are both in play, and the reason it comes out this clean is
/// that an overwrite that changes no length changes no metadata either:
/// the block count, `byte_size`, the pointer table and the bitmap are all
/// untouched, so every prefix of the write is a volume that validates with
/// *zero* findings -- not even a leak -- and holds a file of the right
/// length made of some old blocks and some new ones.
#[test]
fn an_interrupted_overwrite_leaves_old_bytes_or_new_bytes_and_nothing_else() {
    for variant in [Variant::Ofs, Variant::Ffs] {
        let bs = 512;
        let nblocks = 1760;
        let payload = data_payload_size(bs, variant.is_ffs());
        let old = stamped(1, payload * 6);
        let new = stamped(2, payload * 6);

        let total = {
            let vol = writable(variant, bs, nblocks, &old);
            let root = vol.root_lba();
            let mut m = Mutator::open(vol).unwrap();
            m.write_file(root, b"Doc", 0, &new).unwrap();
            let mut vol = m.into_volume();
            vol.source_mut().clear_log();
            let mut m = Mutator::open(vol).unwrap();
            m.write_file(root, b"Doc", 0, &old).unwrap();
            m.into_volume().into_inner().write_log().len()
        };
        assert!(total >= 7, "{variant:?}: {total} writes");

        let mut crashed = 0;
        for n in 0..=total {
            let vol = writable(variant, bs, nblocks, &old);
            let root = vol.root_lba();
            let mut m = Mutator::open(vol).unwrap();
            m.volume().source_mut().clear_log();
            m.volume().source_mut().fail_after(n);
            if m.write_file(root, b"Doc", 0, &new).is_err() {
                crashed += 1;
            }
            let mut vol =
                Volume::open_with(m.into_volume().into_inner(), None, nblocks, 2).unwrap();

            let report = vol.validate();
            assert!(
                report.is_clean(),
                "{variant:?}: crash after {n} writes left {:#?}",
                report
                    .findings
                    .iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
            );
            let doc = vol.lookup(root, b"Doc").unwrap().unwrap();
            let back = vol.read_file(doc.lba).unwrap();
            assert_eq!(back.len(), old.len(), "{variant:?}: length moved");
            // Old bytes or new bytes, independently per block, and
            // nothing in between.
            for i in 0..back.len() / payload {
                let (from, to) = (i * payload, (i + 1) * payload);
                assert!(
                    back[from..to] == old[from..to] || back[from..to] == new[from..to],
                    "{variant:?}: crash after {n} writes tore block {i}"
                );
            }
            assert_tree_intact(&mut vol);
        }
        assert!(crashed > 0, "{variant:?}: no prefix actually failed");
    }
}

/// The sweep the crash-shape box was held open for: every prefix of a
/// session that appends across an extension-block boundary, overwrites,
/// grows and shrinks.
///
/// The damage allowed is exactly what the module documents. `OrphanBlock`
/// -- blocks allocated and not yet reachable, or reachable and not yet
/// freed -- because that is the direction the ordering fails in;
/// `DircacheStale` on the two variants that have caches, because a cache
/// is advisory. `ReachableButFree` never, a corrupt block never, and every
/// file still reachable still reads every one of its bytes -- which for
/// the file being written means its length and its extent still agree,
/// the thing the header-block-as-single-commit rule exists to guarantee.
#[test]
fn an_interrupted_file_write_leaks_and_never_double_allocates() {
    for variant in [
        Variant::Ofs,
        Variant::Ffs,
        Variant::FfsIntlDircache,
        Variant::FfsIntlLongname,
    ] {
        let bs = 512;
        let nblocks = 1760;
        let p = data_payload_size(bs, variant.is_ffs()) as u64;
        let slots = hash_table_size(bs) as u64;
        let ext = slots * p;

        let session = |vol: Volume<MemDisk>| -> (Volume<MemDisk>, bool) {
            let root = vol.root_lba();
            let mut m = Mutator::open(vol).expect("the bitmap is valid at every prefix");
            let ok = m
                .append(root, b"Doc", &stamped(1, (ext + 2 * p) as usize))
                .is_ok()
                & m.write_file(root, b"Doc", p / 2, &stamped(2, 2 * p as usize))
                    .is_ok()
                & m.truncate(root, b"Doc", p + 11).is_ok()
                & m.truncate(root, b"Doc", 3 * p + 5).is_ok()
                & m.append(root, b"Doc", &stamped(3, 40)).is_ok();
            (m.into_volume(), ok)
        };

        let total = {
            let mut vol = writable(variant, bs, nblocks, &pattern(700));
            vol.source_mut().clear_log();
            let (vol, ok) = session(vol);
            assert!(ok, "{variant:?}: the uninterrupted session must succeed");
            vol.into_inner().write_log().len()
        };
        assert!(total > 20, "{variant:?}: the session writes {total} blocks");

        let mut crashed = 0;
        for n in 0..=total {
            let mut vol = writable(variant, bs, nblocks, &pattern(700));
            vol.source_mut().clear_log();
            vol.source_mut().fail_after(n);
            let (vol, ok) = session(vol);
            if !ok {
                crashed += 1;
            }
            let mut vol = Volume::open_with(vol.into_inner(), None, nblocks, 2).unwrap();

            let report = vol.validate();
            for finding in &report.findings {
                let excusable = matches!(finding, Finding::OrphanBlock { .. })
                    || (variant.has_dircache() && matches!(finding, Finding::DircacheStale { .. }));
                assert!(
                    excusable,
                    "{variant:?}: crash after {n} writes left {finding}"
                );
            }
            assert_eq!(
                report.summary.reachable_but_free, 0,
                "{variant:?}: crash after {n} writes"
            );

            let root = vol.root_lba();
            for entry in vol.read_dir(root).unwrap() {
                if entry.kind == EntryKind::File {
                    let read = vol.read_file(entry.lba).unwrap();
                    assert_eq!(read.len(), entry.byte_size as usize);
                }
            }
            assert_tree_intact(&mut vol);
        }
        assert!(crashed > 0, "{variant:?}: no prefix actually failed");
    }
}

/// A seeded random interleave of ranged writes, appends and truncates
/// against a `Vec<u8>`, on OFS and FFS.
///
/// The offsets and lengths are drawn around the two boundaries where the
/// arithmetic differs -- the payload block and the header table's last
/// slot -- rather than uniformly over the file, because a uniform draw
/// almost never lands on either. Every step is followed by a whole-file
/// readback, a ranged readback of a random window (so the two paths are
/// checked against the same model, not against each other) and a clean
/// `validate()`.
#[test]
fn a_random_interleave_of_writes_agrees_with_a_model_file() {
    for (v, variant) in [Variant::Ofs, Variant::Ffs].into_iter().enumerate() {
        for (b, bs) in [512usize, 4096].into_iter().enumerate() {
            let nblocks = 8 * 1024 * 1024 / bs as u64;
            let p = data_payload_size(bs, variant.is_ffs()) as u64;
            let slots = hash_table_size(bs) as u64;
            let mut rng = Rng::new(0x03DF_2000 + (v * 16 + b) as u64);

            let mut model = pattern(1234);
            let vol = writable(variant, bs, nblocks, &model);
            let root = vol.root_lba();
            let mut m = Mutator::open(vol).expect("mutator");
            let mut counts = [0usize; 3];

            // Offsets and lengths that cluster on the boundaries: a
            // block edge, the header table's last slot, and small
            // amounts either side of both.
            let near = |rng: &mut Rng| -> u64 {
                let base = match rng.below(4) {
                    0 => 0,
                    1 => p,
                    2 => slots * p,
                    _ => (1 + rng.below(6) as u64) * p,
                };
                let jitter = rng.below(2 * p as usize + 1) as u64;
                (base + jitter).saturating_sub(p / 2)
            };

            for step in 0..40u8 {
                let tag = step.wrapping_add(1);
                match rng.below(6) {
                    0..=2 => {
                        let off = near(&mut rng);
                        let len = rng.below(2 * p as usize + 200);
                        let data = stamped(tag, len);
                        m.write_file(root, b"Doc", off, &data).expect("write_file");
                        model_write(&mut model, off, &data);
                        counts[0] += 1;
                    }
                    3..=4 => {
                        let len = rng.below(2 * p as usize + 200);
                        let data = stamped(tag, len);
                        let at = model.len() as u64;
                        m.append(root, b"Doc", &data).expect("append");
                        model_write(&mut model, at, &data);
                        counts[1] += 1;
                    }
                    _ => {
                        let n = near(&mut rng);
                        m.truncate(root, b"Doc", n).expect("truncate");
                        model.resize(n as usize, 0);
                        counts[2] += 1;
                    }
                }

                let what = format!("{variant:?} @{bs} step {step}");
                let vol = m.volume();
                let doc = vol.lookup(root, b"Doc").unwrap().unwrap();
                assert_eq!(doc.byte_size as usize, model.len(), "{what}: byte_size");
                assert_eq!(vol.read_file(doc.lba).unwrap(), model, "{what}");

                // The same file through the ranged path, over a window
                // the model also knows the answer for.
                if !model.is_empty() {
                    let chain = vol.file_chain(doc.lba).unwrap();
                    let off = rng.below(model.len()) as u64;
                    let len = rng.below(3 * p as usize + 1);
                    let mut buf = vec![0u8; len];
                    let got = vol.read_range(&chain, off, &mut buf).unwrap();
                    let want = &model[off as usize..(off as usize + len).min(model.len())];
                    assert_eq!(&buf[..got], want, "{what}: ranged read at {off}");
                }
                assert_clean(vol, &what);
            }

            for (i, n) in counts.iter().enumerate() {
                assert!(*n > 0, "{variant:?} @{bs}: operation {i} never ran");
            }

            // And the file put back to what it started as leaves the
            // volume using exactly as many blocks as it did then. Not the
            // *same* blocks: a copy-on-written boundary block moves, so
            // which blocks a file occupies is allocation order and a
            // documented don't-care, while how many it occupies is
            // arithmetic and is not.
            let before = {
                let mut fresh = writable(variant, bs, nblocks, &pattern(1234));
                allocated_set(&mut fresh).len()
            };
            m.truncate(root, b"Doc", 1234).expect("truncate back");
            m.write_file(root, b"Doc", 0, &pattern(1234))
                .expect("rewrite");
            let mut vol = m.into_volume();
            assert_clean(&mut vol, &format!("{variant:?} @{bs} restored"));
            assert_eq!(
                allocated_set(&mut vol).len(),
                before,
                "{variant:?} @{bs}: the file did not give its blocks back"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// resize()
// ---------------------------------------------------------------------------

/// A populated volume with a random tree, and the tree itself so a resize
/// test can check it survived.
fn resizable(
    variant: Variant,
    bs: usize,
    nblocks: u64,
    seed: u64,
    mut budget: usize,
) -> (Volume<MemDisk>, Vec<Node>) {
    let mut rng = Rng::new(seed);
    let tree = random_tree(&mut rng, variant, 0, &mut budget);
    let disk = MemDisk::filled(bs, nblocks, 0xA5);
    let opts = FormatOptions::new(variant, nblocks, b"Resizable");
    let mut pop = Populator::new(disk, &opts).expect("populate");
    let root = pop.root_lba();
    write_tree(&mut pop, root, &tree);
    let disk = pop.finish().expect("finish");
    let vol = Volume::open_with(disk, None, nblocks, 2).expect("open");
    (vol, tree)
}

/// The bitmap covers exactly `reserved..block_count`, is valid, and every
/// LNFS root field agrees with it.
fn assert_bitmap_and_lnfs_fields_agree(vol: &mut Volume<MemDisk>, what: &str) {
    let bm = vol.read_bitmap().unwrap();
    assert!(bm.valid(), "{what}: bitmap not valid");
    assert!(
        bm.covers_whole_volume(),
        "{what}: bitmap short of the new end"
    );
    assert_eq!(bm.end_block(), vol.block_count(), "{what}");
    let variant = vol.variant();
    if variant.has_long_names() {
        assert_eq!(
            vol.root().blocks_used,
            Some(bm.allocated_count() as u32),
            "{what}: NumBlocksUsed"
        );
        assert_eq!(
            vol.root().fs_type,
            Some(variant.dostype()),
            "{what}: FileSystemType"
        );
    }
}

/// Grow and shrink back, every variant, every block size: the round trip
/// the whole feature is for. Every file byte-identical, every name still
/// resolvable (the re-parenting proof: a wrong parent pointer still
/// enumerates and looks up fine, since lookup never follows `parent`),
/// the bitmap covering exactly the new extent, and the LNFS fields
/// agreeing with it in both directions.
#[test]
fn resize_grows_and_shrinks_every_variant_and_block_size() {
    for variant in ALL_VARIANTS {
        for bs in [512usize, 1024, 4096] {
            let nblocks = 4 * 1024 * 1024 / bs as u64;
            let grown = nblocks * 3;
            let (mut vol, tree) = resizable(variant, bs, nblocks, 0xAB00_0000 + bs as u64, 300_000);
            let root = vol.root_lba();
            check_tree(&mut vol, root, &tree, "before grow");

            vol.source_mut().grow(grown);
            let report = vol
                .resize(grown)
                .unwrap_or_else(|e| panic!("{variant:?} @{bs} grow to {grown}: {e}"));
            assert_eq!(report.old_block_count, nblocks);
            assert_eq!(report.new_block_count, grown);
            assert_eq!(
                vol.root_lba(),
                canonical_root_lba(grown, 2).unwrap(),
                "{variant:?} @{bs}: root did not move to the new midpoint"
            );
            let root = vol.root_lba();
            check_tree(&mut vol, root, &tree, "after grow");
            assert_clean(&mut vol, &format!("{variant:?} @{bs} after grow"));
            assert_bitmap_and_lnfs_fields_agree(&mut vol, "after grow");

            let report = vol
                .resize(nblocks)
                .unwrap_or_else(|e| panic!("{variant:?} @{bs} shrink to {nblocks}: {e}"));
            assert_eq!(report.new_block_count, nblocks);
            assert_eq!(
                vol.root_lba(),
                canonical_root_lba(nblocks, 2).unwrap(),
                "{variant:?} @{bs}: root did not move back"
            );
            let root = vol.root_lba();
            check_tree(&mut vol, root, &tree, "after shrink");
            assert_clean(&mut vol, &format!("{variant:?} @{bs} after shrink"));
            assert_bitmap_and_lnfs_fields_agree(&mut vol, "after shrink");
        }
    }
}

/// Resizing to the size a volume already is writes nothing at all.
#[test]
fn resize_to_the_same_size_is_a_true_no_op() {
    let (mut vol, _tree) = resizable(Variant::Ffs, 512, 1760, 1, 5000);
    vol.source_mut().clear_log();
    let report = vol.resize(1760).unwrap();
    assert_eq!(report.old_root_lba, report.new_root_lba);
    assert_eq!(report.children_reparented, 0);
    assert!(vol.source_mut().write_log().is_empty());
}

/// Growing across the 25-bitmap-pointer boundary chains an extension
/// block, and shrinking back drops it again -- the same "chain extension
/// blocks when the root's 25 pointers are exhausted" arithmetic
/// [`format`] uses at creation, exercised here by a resize instead.
#[test]
fn resize_crosses_the_bitmap_extension_boundary_in_both_directions() {
    let bs = 512usize;
    let small = 4096u64;
    let big = 150_000u64; // (big - 2) / 4064 > 25: needs an extension block.

    let (mut vol, tree) = resizable(Variant::FfsIntl, bs, small, 7, 20_000);
    vol.source_mut().grow(big);

    vol.resize(big)
        .unwrap_or_else(|e| panic!("grow to {big}: {e}"));
    let bm = vol.read_bitmap().unwrap();
    assert!(
        !bm.ext_blocks().is_empty(),
        "a {big}-block volume at {bs} bytes needs a bitmap extension block"
    );
    assert!(bm.pages().len() > BITMAP_PAGES);
    let root = vol.root_lba();
    check_tree(
        &mut vol,
        root,
        &tree,
        "after growing past the extension boundary",
    );
    assert_clean(&mut vol, "after growing past the extension boundary");
    assert_bitmap_and_lnfs_fields_agree(&mut vol, "after growing past the extension boundary");

    vol.resize(small)
        .unwrap_or_else(|e| panic!("shrink back to {small}: {e}"));
    let bm = vol.read_bitmap().unwrap();
    assert!(
        bm.ext_blocks().is_empty(),
        "shrinking back below the boundary must drop the extension block"
    );
    let root = vol.root_lba();
    check_tree(
        &mut vol,
        root,
        &tree,
        "after shrinking back below the extension boundary",
    );
    assert_clean(
        &mut vol,
        "after shrinking back below the extension boundary",
    );
    assert_bitmap_and_lnfs_fields_agree(
        &mut vol,
        "after shrinking back below the extension boundary",
    );
}

/// A shrink refuses when real, non-movable data would end up past the new
/// end -- naming the offending block -- and [`Volume::minimum_size`]
/// agrees exactly: the largest refused size plus one is the smallest size
/// that succeeds.
#[test]
fn resize_shrink_refuses_user_data_past_the_cut_and_minimum_size_agrees() {
    let bs = 512usize;
    let nblocks = 20_000u64;

    // A big filler (low LBAs, since this crate's allocator fills upward
    // from `reserved`) deleted afterwards, and a small straggler that
    // survives at the low end of what is now free space above it: the
    // shape that makes `minimum_size` land well clear of the *new* root's
    // own midpoint, so this test is only ever about the refusal, never
    // about the new root's target also happening to collide with data.
    let mut vol = populated(Variant::Ffs, bs, nblocks, |pop| {
        let root = pop.root_lba();
        pop.create_file(
            root,
            b"Filler",
            &amiga_ffs::Metadata::new(),
            &pattern(400_000),
        )
        .unwrap();
        pop.create_file(
            root,
            b"Straggler",
            &amiga_ffs::Metadata::new(),
            &pattern(2000),
        )
        .unwrap();
    });
    let mut m = Mutator::open(vol).unwrap();
    let root = m.volume().root_lba();
    m.delete(root, b"Filler").unwrap();
    vol = m.into_volume();
    assert_clean(&mut vol, "after deleting the filler");

    let min = vol.minimum_size().unwrap();
    assert!(min > 2 && min < nblocks, "sanity: min={min}");

    let err = vol.resize(min - 1).unwrap_err();
    let (lba, refused_at) = match err {
        ResizeError::UserDataPastCut {
            lba,
            new_block_count,
        } => (lba, new_block_count),
        other => panic!("expected UserDataPastCut, got {other}"),
    };
    assert_eq!(refused_at, min - 1);
    assert!(lba >= min - 1, "the finding must actually be past the cut");

    // The refusal must not have written anything.
    assert_clean(&mut vol, "after a refused shrink");
    assert_eq!(vol.block_count(), nblocks);

    // And exactly the minimum succeeds.
    vol.resize(min)
        .unwrap_or_else(|e| panic!("resize to minimum_size {min}: {e}"));
    assert_clean(&mut vol, "at minimum_size");
    let root = vol.root_lba();
    let straggler = vol.lookup(root, b"Straggler").unwrap().expect("Straggler");
    assert_eq!(vol.read_file(straggler.lba).unwrap(), pattern(2000));
}

/// A root dircache chain long enough to span several blocks, shrunk down
/// to `minimum_size()` -- which by definition may cut straight through
/// where those blocks used to be, since the root's own dircache chain is
/// movable metadata. Every block ends up back under the new end, every
/// name is still findable, and the cache agrees with the chains
/// afterwards (zero `DircacheStale`).
#[test]
fn resize_relocates_the_roots_dircache_chain_past_a_shrinking_cut() {
    let bs = 512usize;
    let nblocks = 6000u64;
    let names: Vec<String> = (0..80u32).map(|i| format!("File{i:03}")).collect();
    let mut vol = populated(Variant::FfsIntlDircache, bs, nblocks, |pop| {
        let root = pop.root_lba();
        for name in &names {
            pop.create_file(root, name.as_bytes(), &amiga_ffs::Metadata::new(), b"x")
                .unwrap();
        }
    });
    let before = vol.read_dircache(vol.root_lba()).unwrap();
    assert!(
        before.blocks.len() > 1,
        "need a multi-block root dircache to exercise relocation"
    );

    let min = vol.minimum_size().unwrap();
    let report = vol
        .resize(min)
        .unwrap_or_else(|e| panic!("resize to minimum_size {min}: {e}"));

    assert_clean(&mut vol, "after shrinking past the old dircache chain");
    let after = vol.read_dircache(vol.root_lba()).unwrap();
    assert_eq!(after.blocks.len(), before.blocks.len());
    for &b in &after.blocks {
        assert!(
            b < vol.block_count(),
            "dircache block {b} left past the new end"
        );
    }
    let root = vol.root_lba();
    for name in &names {
        let entry = vol
            .lookup(root, name.as_bytes())
            .unwrap()
            .unwrap_or_else(|| panic!("{name} missing after resize"));
        assert_eq!(vol.read_file(entry.lba).unwrap(), b"x");
    }
    let _ = report;
}

/// A seeded sequence of grow and shrink calls, alternating more or less
/// at random, with the tree checked byte for byte after every step.
#[test]
fn resize_random_grow_shrink_sequence_keeps_the_tree_intact() {
    let bs = 512usize;
    let variant = Variant::FfsIntlLongname;
    let start = 4000u64;
    let ceiling = 60_000u64;
    let (mut vol, tree) = resizable(variant, bs, start, 0xF00D_5EED, 60_000);
    vol.source_mut().grow(ceiling);

    let mut rng = Rng::new(0x1234_5678);
    let mut cur = start;
    for step in 0..8 {
        let min = vol.minimum_size().unwrap();
        let grow = cur >= ceiling.saturating_sub(2000) || rng.below(2) == 0 && cur > min + 200;
        let target = if grow && cur < ceiling {
            (cur + 500 + rng.below(3000) as u64).min(ceiling)
        } else {
            let span = (cur - min + 1) as usize;
            min + rng.below(span.max(1)) as u64
        };
        vol.resize(target)
            .unwrap_or_else(|e| panic!("step {step}: resize {cur} -> {target}: {e}"));
        cur = target;
        let root = vol.root_lba();
        check_tree(
            &mut vol,
            root,
            &tree,
            &format!("step {step} (now {cur} blocks)"),
        );
        assert_clean(&mut vol, &format!("step {step}"));
        assert_bitmap_and_lnfs_fields_agree(&mut vol, &format!("step {step}"));
    }
}

/// The crash-shape claim for this feature, over the whole write sequence
/// of a grow: whatever prefix lands, the volume at the new geometry is
/// either not mountable yet (the very first write -- the new root -- has
/// not landed) or is mountable with `reachable_but_free == 0` always, and
/// [`Volume::repair`] can always bring the bitmap back to valid with the
/// tree's file contents intact. This scopes the claim to the new geometry
/// deliberately: see the module's "Whose job is what" documentation for
/// why a crash is only meaningfully recoverable once the caller's own
/// geometry authority (the RDB) agrees a resize was already under way.
#[test]
fn resize_grow_crash_sweep_never_leaves_a_block_double_allocated() {
    let bs = 512usize;
    let old_n = 1760u64;
    let new_n = 4000u64;

    let total = {
        let mut vol = allocatable(Variant::FfsIntl, bs, old_n);
        vol.source_mut().grow(new_n);
        vol.source_mut().clear_log();
        vol.resize(new_n).unwrap();
        vol.source_mut().write_log().len()
    };
    assert!(total > 0);

    let mut crashed = 0;
    for n in 0..total {
        let mut vol = allocatable(Variant::FfsIntl, bs, old_n);
        vol.source_mut().grow(new_n);
        vol.source_mut().fail_after(n);
        if vol.resize(new_n).is_err() {
            crashed += 1;
        }

        let mut disk = vol.into_inner();
        disk.stop_failing();
        let mut vol = match Volume::open_with(disk, None, new_n, 2) {
            // The new root has not landed yet: a safe refusal to mount,
            // not a state this crash sweep has anything further to check.
            Err(_) => continue,
            Ok(v) => v,
        };
        if let Ok(bm) = vol.read_bitmap() {
            if bm.valid() {
                assert_eq!(
                    vol.validate().summary.reachable_but_free,
                    0,
                    "crash after {n} writes: a valid bitmap must never lie about a block in use"
                );
            }
        }
        let _ = vol.repair(&RepairOptions::new());
        let bm = vol.read_bitmap().unwrap();
        assert!(
            bm.valid(),
            "crash after {n} writes: repair must leave a valid bitmap"
        );
        assert_eq!(
            vol.validate().summary.reachable_but_free,
            0,
            "crash after {n} writes, post-repair"
        );
        assert_tree_intact(&mut vol);
    }
    assert!(crashed > 0, "at least one prefix must actually fail");
}

/// The more dangerous direction: a shrink also relocates metadata (excess
/// bitmap pages, and here a root dircache chain that would otherwise be
/// stranded) and issues explicit frees -- exactly the ingredients a
/// double allocation would come from if the ordering were wrong, and
/// paths the grow sweep above never exercises. From the same crashed
/// state, two independent recoveries are checked: [`Volume::repair`]
/// alone, which this module documents as fixing only the bitmap, and a
/// same-target retry of [`Volume::resize`], which this module documents
/// as able to finish reparenting a crash caught mid-move (see
/// `src/resize.rs`'s "Retrying"). Both are pinned rather than assumed.
#[test]
fn resize_shrink_crash_sweep_never_leaves_a_block_double_allocated() {
    let bs = 512usize;
    let old_n = 1760u64;
    let new_n = 200u64; // Below the old root (~880): it must move, and
                        // the old dircache/bitmap-page blocks (~881+)
                        // end up past the new end and must relocate.

    let total = {
        let mut vol = allocatable(Variant::FfsIntlDircache, bs, old_n);
        vol.source_mut().clear_log();
        vol.resize(new_n).unwrap();
        vol.source_mut().write_log().len()
    };
    assert!(total > 0);

    let mut crashed = 0;
    for n in 0..total {
        let mut vol = allocatable(Variant::FfsIntlDircache, bs, old_n);
        vol.source_mut().fail_after(n);
        if vol.resize(new_n).is_err() {
            crashed += 1;
        }

        let mut crashed_disk = vol.into_inner();
        crashed_disk.stop_failing();
        let mut vol = match Volume::open_with(crashed_disk.clone(), None, new_n, 2) {
            // The new root has not landed yet: a safe refusal to mount,
            // not a state this sweep has anything further to check.
            Err(_) => continue,
            Ok(v) => v,
        };
        if let Ok(bm) = vol.read_bitmap() {
            if bm.valid() {
                assert_eq!(
                    vol.validate().summary.reachable_but_free,
                    0,
                    "crash after {n} writes: a valid bitmap must never lie about a block in use"
                );
            }
        }

        // Path A: `repair()` alone. Documented as fixing only the bitmap
        // -- pin that rather than assume it: whatever findings are left
        // must be exactly the shapes a stray, not-yet-updated pointer
        // produces -- a child or a dircache block still naming the old
        // root (`ParentMismatch`/`DircacheStale`), the root's own
        // dircache pointer still naming a block a shrink already sliced
        // off before the pointer was updated to point at its relocated
        // replacement (`Unreadable`/`LbaOutOfRange`), or a block a
        // relocation or an explicit free left nothing pointing at
        // (`OrphanBlock`, the recoverable leak direction) -- never
        // anything that says a block is double-used.
        {
            let mut vol_a = Volume::open_with(crashed_disk.clone(), None, new_n, 2).unwrap();
            let _ = vol_a.repair(&RepairOptions::new());
            let bm = vol_a.read_bitmap().unwrap();
            assert!(
                bm.valid(),
                "crash after {n} writes: repair must leave a valid bitmap"
            );
            let report = vol_a.validate();
            assert_eq!(
                report.summary.reachable_but_free, 0,
                "crash after {n} writes, post-repair"
            );
            for finding in &report.findings {
                let expected = matches!(
                    finding,
                    Finding::ParentMismatch { .. }
                        | Finding::DircacheStale { .. }
                        | Finding::OrphanBlock { .. }
                        | Finding::Unreadable {
                            error: Error::LbaOutOfRange { .. },
                            ..
                        }
                );
                assert!(
                    expected,
                    "crash after {n} writes: repair alone left an unexpected finding: {finding}"
                );
            }
            assert_tree_intact(&mut vol_a);
        }

        // Path B: a same-target `resize` retry, from the same crashed
        // state. Documented as able to finish the reparenting a plain
        // repair cannot -- checked here down to every direct child's own
        // parent longword, the re-parenting proof in full -- at the cost
        // of two things a retry has no way to recover, both pinned
        // rather than assumed away: at most one harmless leak (the very
        // first root position, once superseded), and -- only when the
        // root's own dircache pointer was itself caught mid-relocation
        // and this retry could no longer read what it pointed at -- a
        // dropped cache, rebuilt empty rather than recovered, which
        // `DircacheStale` reports and any later `Mutator` operation on
        // the root regenerates. Never anything else, and never more than
        // one leak.
        vol.resize(new_n)
            .unwrap_or_else(|e| panic!("crash after {n} writes: retry resize failed: {e}"));
        let report = vol.validate();
        assert_eq!(
            report.summary.reachable_but_free, 0,
            "crash after {n} writes, post-retry"
        );
        let mut orphans = 0;
        for finding in &report.findings {
            match finding {
                Finding::OrphanBlock { .. } => orphans += 1,
                Finding::DircacheStale { .. } => {}
                other => panic!("crash after {n} writes: retry left a non-leak finding: {other}"),
            }
        }
        assert!(
            orphans <= 1,
            "crash after {n} writes: {orphans} leaks after retry, expected at most one"
        );
        assert_tree_intact(&mut vol);
        let root = vol.root_lba();
        for e in vol.read_dir(root).unwrap() {
            assert_eq!(
                e.parent as u64, root,
                "crash after {n} writes, post-retry: block {} not fully re-parented",
                e.lba
            );
        }
    }
    assert!(crashed > 0, "at least one prefix must actually fail");
}

/// [`ResizeError::RootTargetOccupied`], engineered directly rather than
/// hit incidentally: a volume packed with data from `reserved` upward
/// (this crate's own allocator's normal shape) grown to a size whose new
/// midpoint lands squarely inside that data.
#[test]
fn resize_refuses_a_root_target_occupied_by_real_data() {
    let bs = 512usize;
    let old_n = 2000u64;

    // A file spanning well past the halfway point, so even the smallest
    // possible grow -- whose new midpoint starts just past the *old*
    // one, since the midpoint moves by half a block per block of growth
    // -- still lands inside it.
    let mut vol = populated(Variant::Ffs, bs, old_n, |pop| {
        let root = pop.root_lba();
        pop.create_file(root, b"Big", &amiga_ffs::Metadata::new(), &pattern(600_000))
            .unwrap();
    });
    let before = allocated_set(&mut vol);

    let grown = old_n + 50;
    let new_root = canonical_root_lba(grown, 2).unwrap();
    assert_eq!(
        vol.read_bitmap().unwrap().is_allocated(new_root),
        Some(true),
        "test setup: the new root's target must actually collide with data"
    );

    vol.source_mut().grow(grown);
    let err = vol.resize(grown).unwrap_err();
    match err {
        ResizeError::RootTargetOccupied { lba } => assert_eq!(lba, new_root),
        other => panic!("expected RootTargetOccupied, got {other}"),
    }

    // A refusal must not have written anything the volume did not
    // already have -- same size, same root, same allocated set, still
    // clean.
    assert_eq!(vol.block_count(), old_n);
    assert_eq!(vol.root_lba(), canonical_root_lba(old_n, 2).unwrap());
    assert_eq!(allocated_set(&mut vol), before);
    assert_clean(&mut vol, "after a refused grow");
}
