//! Filling a freshly formatted volume: directories, files, metadata.
//!
//! [`format`](crate::format()) leaves a mountable empty volume;
//! [`Populator`] is what puts something in it. Between them they are the
//! whole of "make me a disk image out of this directory" — the operation
//! Copperline wants for dynamic drives and amibake wants for `dir` → HDF
//! — and they are deliberately *not* the operation milestone 3 is about.
//!
//! # What this is not: the mutation boundary
//!
//! A [`Populator`] may only be pointed at a volume **this crate has just
//! formatted**, and it may only *add*. There is no delete, no rename, no
//! truncate, no second session over a volume somebody else wrote — those
//! are milestone 3, and they need the read-modify-write allocator
//! discipline (crash ordering, bitmap-before-metadata, freeing chains
//! without leaking them) that this module exists to avoid needing.
//!
//! The avoidance is the design. A populator is constructed from the
//! [`FormatLayout`] the format returned, so the set of blocks already in
//! use is *known*, not discovered; allocation is a cursor that walks
//! upward from the first free block and never revisits one; and the
//! bitmap is written **once**, at [`Populator::finish`], from that
//! cursor. Nothing here ever has to decide whether a block on disk is
//! free, so nothing here can get that decision wrong.
//!
//! # The crash shape
//!
//! Because the bitmap is not written until the end, a populator that is
//! interrupted leaves a volume whose bitmap says less is in use than
//! really is — the *dangerous* direction, the one
//! [`Finding::ReachableButFree`](crate::validate::Finding::ReachableButFree)
//! is about. So construction clears the root's `bitmap_flag` first: an
//! interrupted populate leaves a volume that says out loud "my bitmap is
//! mid-update, do not allocate from me" (which AmigaDOS's own validator
//! and this crate's [`Bitmap::valid`](crate::Bitmap::valid) both honour),
//! and [`Populator::finish`] sets it back to −1 as the last thing it
//! does, after the pages it describes are on the disk.
//!
//! Within one entry the ordering is the same one
//! [`format`](crate::format()) uses: content before metadata, and the
//! pointer that makes a block reachable written last. A file's data
//! blocks go down first, then its extension blocks, then its header, and
//! only then is the header chained into its parent's hash table. An
//! interruption at any point leaves blocks that nothing reaches — a leak,
//! and a leak is what a validator can fix.
//!
//! # Every variant, honestly
//!
//! - **Names** are checked per variant: 30 bytes on `DOS\0`–`DOS\5`, 107
//!   on `DOS\6`/`DOS\7`, Latin-1 throughout, no `:` or `/` and no control
//!   codes — the same rule [`format`](crate::format()) applies to a volume
//!   name, from the same function.
//! - **Comments** go beside the name in the classic layout, and into the
//!   LNFS `NaC` field when a long name leaves room for them. When it does
//!   not, a [`T_COMMENT`] block is allocated and longword −18 points at
//!   it — which is the state [`Volume::comment`](crate::Volume::comment)
//!   exists to resolve, and the state a writer that skipped it would
//!   silently truncate instead.
//! - **File data** is FFS raw blocks or OFS headered ones, with the
//!   header's backward-filled pointer table spilling into `T_LIST`
//!   extension blocks exactly when it fills.
//! - **Hash chains**: a new entry goes in at the **head** of its slot's
//!   chain, which is what the oracle does — three colliding names written
//!   in order to a `DOS\3` ADF by xdftool come back with the *last* one in
//!   the root's hash slot and the first at the end of the chain. Head
//!   insertion is also the only order that is O(1) and cannot walk a chain
//!   it is about to change.
//! - **Dircaches** (`DOS\4`/`DOS\5`) are maintained, not skipped. Every
//!   directory this module creates gets its own `T_DIRCACHE` block at
//!   birth (xdftool's do, and so does the root
//!   [`format`](crate::format()) writes), and every entry created gets a
//!   record appended to its parent's cache, spilling into a chained block
//!   when one fills. The one place this module does *more* than the
//!   oracle is the record's secondary-type byte, which xdftool leaves
//!   zero: filling it in is strictly more information and
//!   [`validate`](crate::Volume::validate) reads a zero there as "not
//!   recorded" either way.
//!
//! # Duplicates
//!
//! A name that already exists in the parent — under the *volume's* fold
//! table, so `Foo` and `FOO` are the same name on every variant and `café`
//! and `CAFÉ` are the same name on the intl ones — is refused. AmigaDOS
//! would happily create the second one and then be unable to open either
//! reliably; this refuses instead.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::bitmap::pack_ranges;
use crate::format::{
    check_name_bytes, format, wr32, wr_bcpl, wr_date, FormatError, FormatLayout, FormatOptions,
    NameProblem,
};
use crate::layout::*;
use crate::read::{DateStamp, EntryKind};
use crate::{be32, checksum_compute, hash_table_size, name_hash, names_equal};
use crate::{BlockSink, BlockSource, Variant, MAX_NAME_CLASSIC, MAX_NAME_LONG};

/// The transport error of a type that both reads and writes.
///
/// [`BlockSource`] and [`BlockSink`] each name their own error type, and
/// this module needs both traits — so it requires them to be the *same*
/// type. Every real backend that does both (a file, an image in memory, a
/// partition on a device) reports its failures one way; two error types
/// would put a `Read`/`Write` split through every signature here for a
/// distinction no backend actually makes.
pub type Transport<S> = <S as BlockSource>::Error;

/// A type that can be populated: it reads and writes the same medium and
/// fails the same way doing either.
pub trait BlockMedium: BlockSource + BlockSink<Error = <Self as BlockSource>::Error> {}

impl<T> BlockMedium for T where T: BlockSource + BlockSink<Error = <T as BlockSource>::Error> {}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything a [`Populator`] refuses, and why.
///
/// Generic over the transport's error for the same reason
/// [`crate::read::Error`] and [`FormatError`] are: "why did the medium
/// fail" is a question only the medium can answer.
///
/// Deliberately not `Clone`/`PartialEq`, unlike this crate's other error
/// types: the `std`-only variants carry a `std::io::Error`, which is
/// neither, and losing the operating system's own message to keep a
/// derive would be trading the useful half away.
#[derive(Debug)]
pub enum PopulateError<E> {
    /// The underlying medium failed.
    Io(E),
    /// Formatting the volume failed, on the way to populating it.
    Format(FormatError<E>),
    /// A block size with no defined hash-table size.
    BadBlockSize(usize),
    /// A block pointer outside the volume — from a caller that named a
    /// parent LBA this populator never handed out.
    LbaOutOfRange {
        /// The offending block number.
        lba: u64,
        /// Blocks in the volume.
        block_count: u64,
    },
    /// A block read back from the volume whose checksum does not balance.
    /// On a volume only this module has written, that is the medium
    /// lying, not the format being wrong.
    Checksum {
        /// The block that failed.
        lba: u64,
    },
    /// The LBA offered as a parent is not a directory.
    NotADirectory {
        /// The block.
        lba: u64,
        /// Its secondary type.
        found: i32,
    },
    /// The volume has no free block left.
    VolumeFull {
        /// Blocks in the volume.
        block_count: u64,
    },
    /// An empty name.
    NameEmpty,
    /// A name longer than the variant can store: 30 bytes on
    /// `DOS\0`–`DOS\5`, 107 on `DOS\6`/`DOS\7`.
    NameTooLong {
        /// The length offered.
        len: usize,
        /// The variant's maximum.
        max: usize,
    },
    /// A byte AmigaDOS cannot have in a name.
    NameInvalidByte {
        /// The offending byte.
        byte: u8,
        /// Where it was.
        index: usize,
    },
    /// A comment longer than [`COMMENT_MAX`]. Not a layout limit that
    /// moves with the variant — 79 characters is all any of them store,
    /// inline or in a `T_COMMENT` block.
    CommentTooLong {
        /// The length offered.
        len: usize,
        /// The maximum, [`COMMENT_MAX`].
        max: usize,
    },
    /// The parent directory already holds this name, under the volume's
    /// own fold table.
    DuplicateName {
        /// The directory.
        parent: u64,
        /// The name, raw Latin-1.
        name: Vec<u8>,
    },
    /// A file longer than the 32-bit `byte_size` field can record.
    FileTooLarge {
        /// The length offered.
        len: u64,
    },
    /// A host path could not be read.
    #[cfg(feature = "std")]
    Host {
        /// The path that failed.
        path: std::path::PathBuf,
        /// What the operating system said.
        error: std::io::Error,
    },
    /// A host file name that is not representable as Latin-1 — either not
    /// UTF-8 at all, or carrying a character above U+00FF.
    ///
    /// Refused rather than transliterated: an Amiga name is Latin-1 bytes,
    /// and guessing a replacement would put a file on the image under a
    /// name the caller never asked for and cannot predict.
    #[cfg(feature = "std")]
    HostNameNotLatin1 {
        /// The path whose final component cannot be converted.
        path: std::path::PathBuf,
    },
    /// A host directory entry that is neither a regular file nor a
    /// directory: a symlink, a socket, a device node, a FIFO.
    ///
    /// Symlinks in particular are refused rather than followed *or*
    /// translated: following one can walk a cycle or leave the tree, and
    /// an AmigaDOS soft link stores an *Amiga* path that this crate has no
    /// way to derive from a host one.
    #[cfg(feature = "std")]
    HostUnsupported {
        /// The path.
        path: std::path::PathBuf,
    },
}

impl<E: fmt::Display> fmt::Display for PopulateError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "block access failed: {e}"),
            Self::Format(e) => write!(f, "formatting the volume failed: {e}"),
            Self::BadBlockSize(n) => {
                write!(f, "block size {n} is not a power of two in 512..=32768")
            }
            Self::LbaOutOfRange { lba, block_count } => write!(
                f,
                "block {lba} is outside the volume ({block_count} blocks)"
            ),
            Self::Checksum { lba } => write!(f, "bad checksum on block {lba}"),
            Self::NotADirectory { lba, found } => write!(
                f,
                "block {lba} (secondary type {found}) is not a directory to create in"
            ),
            Self::VolumeFull { block_count } => {
                write!(f, "no free block left in {block_count}")
            }
            Self::NameEmpty => f.write_str("an entry name is required"),
            Self::NameTooLong { len, max } => {
                write!(f, "name of {len} bytes exceeds this variant's {max}")
            }
            Self::NameInvalidByte { byte, index } => {
                write!(
                    f,
                    "byte {byte:#04x} at index {index} cannot appear in a name"
                )
            }
            Self::CommentTooLong { len, max } => {
                write!(f, "comment of {len} bytes exceeds {max}")
            }
            Self::DuplicateName { parent, name } => write!(
                f,
                "directory {parent} already holds a name folding to {:?}",
                Latin1(name)
            ),
            Self::FileTooLarge { len } => write!(
                f,
                "a file of {len} bytes exceeds the 32-bit length the format records"
            ),
            #[cfg(feature = "std")]
            Self::Host { path, error } => write!(f, "{}: {error}", path.display()),
            #[cfg(feature = "std")]
            Self::HostNameNotLatin1 { path } => write!(
                f,
                "{}: the file name is not representable in Latin-1",
                path.display()
            ),
            #[cfg(feature = "std")]
            Self::HostUnsupported { path } => {
                write!(f, "{}: not a regular file or directory", path.display())
            }
        }
    }
}

/// Latin-1 bytes rendered for a message, with anything unprintable
/// escaped. Names are not UTF-8 and pretending otherwise in an error
/// message is how a diagnostic becomes a second bug.
struct Latin1<'a>(&'a [u8]);

impl fmt::Debug for Latin1<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"")?;
        for &b in self.0 {
            match b {
                0x20..=0x7E => write!(f, "{}", b as char)?,
                _ => write!(f, "\\x{b:02x}")?,
            }
        }
        f.write_str("\"")
    }
}

#[cfg(feature = "std")]
impl<E: std::error::Error + 'static> std::error::Error for PopulateError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Format(e) => Some(e),
            Self::Host { error, .. } => Some(error),
            _ => None,
        }
    }
}

impl<E> From<FormatError<E>> for PopulateError<E> {
    fn from(e: FormatError<E>) -> Self {
        match e {
            FormatError::Io(e) => Self::Io(e),
            other => Self::Format(other),
        }
    }
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// Everything about an entry that is not its name or its contents.
///
/// All four fields are the caller's, and all four default to what a
/// freshly created AmigaDOS object has: protection `0` (which is the
/// *owner may do everything* state — see [`Protection`](crate::Protection)
/// for why that reads backwards), no comment, the epoch, and no owner.
/// There is no clock in a `no_std` crate, so a date is supplied rather
/// than taken; the `std` layer's `datestamp_from_system_time` is what
/// turns a host mtime into one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Metadata<'a> {
    /// The full 32-bit protection longword, group and other bits
    /// included. Zero means the owner may do everything and nobody else
    /// may do anything.
    pub protection: u32,
    /// The comment, raw Latin-1, at most [`COMMENT_MAX`] bytes.
    pub comment: &'a [u8],
    /// The entry's DateStamp.
    pub date: DateStamp,
    /// The raw owner longword: UID in the high word, GID in the low.
    /// Zero on plain FFS; meaningful under muFS.
    pub owner: u32,
}

impl<'a> Metadata<'a> {
    /// The defaults: full owner access, no comment, the epoch, no owner.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the protection longword.
    pub fn protection(mut self, protection: u32) -> Self {
        self.protection = protection;
        self
    }

    /// Set the comment.
    pub fn comment(mut self, comment: &'a [u8]) -> Self {
        self.comment = comment;
        self
    }

    /// Set the DateStamp.
    pub fn date(mut self, date: DateStamp) -> Self {
        self.date = date;
        self
    }

    /// Set the owner longword (UID in the high word, GID in the low).
    pub fn owner(mut self, owner: u32) -> Self {
        self.owner = owner;
        self
    }
}

// ---------------------------------------------------------------------------
// The populator
// ---------------------------------------------------------------------------

/// Adds directories and files to a volume this crate just formatted.
///
/// See the module documentation for the boundary this type draws and why:
/// it is an *append-only* writer over a *known* free extent, which is
/// what lets it allocate with a cursor and write the bitmap once.
///
/// The volume is not valid again until [`Populator::finish`] has been
/// called — the bitmap is deliberately marked mid-update for the whole
/// session — so `finish` is not optional, and dropping a populator
/// without it leaves a volume that says so rather than one that lies.
pub struct Populator<S: BlockMedium> {
    src: S,
    variant: Variant,
    block_size: usize,
    block_count: u64,
    reserved: u64,
    root_lba: u64,
    /// The contiguous run of blocks `format()` claimed: root, bitmap
    /// extension blocks, bitmap pages and (on `DOS\4`/`DOS\5`) the root's
    /// dircache. The allocator jumps over it rather than tracking it bit
    /// by bit, which is only sound because the format lays it down
    /// contiguously — and it does, by construction and by test.
    format_span: (u64, u64),
    bitmap_pages: Vec<u64>,
    /// The next block to hand out. Never inside `format_span`.
    next_free: u64,
    buf: Vec<u8>,
}

impl<S: BlockMedium> Populator<S> {
    /// Format a volume and open it for populating, in one step.
    ///
    /// The usual entry point: a caller that wants an image with something
    /// on it has no use for the intermediate empty one.
    pub fn new(src: S, opts: &FormatOptions<'_>) -> Result<Self, PopulateError<Transport<S>>> {
        let mut src = src;
        let layout = format(&mut src, opts)?;
        Self::adopt(src, opts, &layout)
    }

    /// Open a volume that [`format`](crate::format()) has *already*
    /// written, described by the [`FormatLayout`] it returned and the
    /// [`FormatOptions`] it was given.
    ///
    /// Both are required, and that is the point: the layout is how this
    /// type knows which blocks are already in use without reading a
    /// bitmap, and demanding it makes "only a volume we just made" a
    /// compile-time-visible precondition rather than a comment. Handing in
    /// a layout that does not describe the volume on the medium is the one
    /// way to misuse this type, and it produces a volume whose bitmap is
    /// wrong — which [`Volume::validate`](crate::Volume::validate) will
    /// say, loudly.
    pub fn adopt(
        src: S,
        opts: &FormatOptions<'_>,
        layout: &FormatLayout,
    ) -> Result<Self, PopulateError<Transport<S>>> {
        let block_size = BlockSource::block_size(&src);
        if !block_size_ok(block_size) {
            return Err(PopulateError::BadBlockSize(block_size));
        }
        let allocated = layout.allocated();
        let span = (
            allocated.first().copied().unwrap_or(layout.root_lba),
            allocated.last().copied().unwrap_or(layout.root_lba) + 1,
        );
        let mut me = Self {
            src,
            variant: opts.variant,
            block_size,
            block_count: opts.block_count,
            reserved: opts.reserved,
            root_lba: layout.root_lba,
            format_span: span,
            bitmap_pages: layout.bitmap_pages.clone(),
            next_free: opts.reserved,
            buf: vec![0u8; block_size],
        };
        if me.next_free >= span.0 {
            me.next_free = span.1;
        }
        // Say out loud that the bitmap is mid-update, before anything is
        // allocated against it. An interrupted populate then leaves a
        // volume that refuses to allocate rather than one that hands out
        // blocks a file is already using.
        me.set_bitmap_flag(0)?;
        Ok(me)
    }

    /// The volume's root directory — the parent to create the top level
    /// in.
    pub fn root_lba(&self) -> u64 {
        self.root_lba
    }

    /// The variant being written.
    pub fn variant(&self) -> Variant {
        self.variant
    }

    /// The filesystem block size.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Blocks allocated so far: the format's own, plus everything this
    /// populator has handed out.
    pub fn blocks_used(&self) -> u64 {
        let mine = self.next_free.saturating_sub(self.reserved);
        let formatted = if self.format_span.0 >= self.next_free {
            self.format_span.1 - self.format_span.0
        } else {
            0
        };
        mine + formatted
    }

    /// Blocks still free.
    pub fn blocks_free(&self) -> u64 {
        (self.block_count - self.reserved).saturating_sub(self.blocks_used())
    }

    /// The longest name this volume's variant can store.
    pub fn max_name_len(&self) -> usize {
        if self.variant.has_long_names() {
            MAX_NAME_LONG
        } else {
            MAX_NAME_CLASSIC
        }
    }

    /// Write the bitmap, restore the root's `bitmap_flag`, and hand the
    /// medium back.
    ///
    /// This is what makes the volume valid again. Every bitmap page is
    /// rewritten from the allocation cursor — one pass, no read-modify-
    /// write, no chance of a page that disagrees with another — then the
    /// root is updated: `bitmap_flag` back to −1, and on `DOS\6`/`DOS\7`
    /// the `NumBlocksUsed` longword the LNFS root carries.
    ///
    /// The root goes last on purpose. Until its flag is −1 the pages are
    /// not being trusted by anybody, so a failure part-way through the
    /// pages leaves the same honest "do not allocate from me" state the
    /// session started in.
    pub fn finish(mut self) -> Result<S, PopulateError<Transport<S>>> {
        let bs = self.block_size;
        let ranges = [
            self.reserved..self.next_free,
            self.format_span.0..self.format_span.1,
        ];
        let words = pack_ranges(self.reserved, self.block_count, &ranges, bs);
        let words_per_page = (bs - OFF_BITMAP_BITS) / 4;
        for i in 0..self.bitmap_pages.len() {
            let page = self.bitmap_pages[i];
            self.buf.iter_mut().for_each(|b| *b = 0);
            for w in 0..words_per_page {
                let v = words
                    .get(i * words_per_page + w)
                    .copied()
                    .unwrap_or(u32::MAX);
                wr32(&mut self.buf, OFF_BITMAP_BITS + w * 4, v);
            }
            // Longword 0: the format's one checksum exception, and the
            // one a writer gets wrong in the direction that still
            // verifies against itself.
            let ck = checksum_compute(&self.buf, BITMAP_CHECKSUM_INDEX);
            wr32(&mut self.buf, BITMAP_CHECKSUM_INDEX * 4, ck);
            self.put(page)?;
        }

        let used = self.blocks_used();
        let root = self.root_lba;
        self.read_checked(root)?;
        wr32(&mut self.buf, tail(bs, TL_BITMAP_FLAG), (-1i32) as u32);
        if self.variant.has_long_names() {
            wr32(
                &mut self.buf,
                tail(bs, TL_ROOT_NUM_BLOCKS_USED),
                used as u32,
            );
        }
        self.put_checked(root)?;
        Ok(self.src)
    }

    /// Give the medium back **without** writing the bitmap.
    ///
    /// The deliberate way to abandon a session: the volume keeps the
    /// `bitmap_flag == 0` that construction set, so it is a volume that
    /// says "my bitmap is mid-update" rather than one that quietly claims
    /// blocks are free while files are using them. That is the same state
    /// an interrupted populate leaves, and the same state AmigaDOS's own
    /// validator (or [`Volume::validate`](crate::Volume::validate), which
    /// reports [`Finding::BitmapInvalid`](crate::Finding::BitmapInvalid))
    /// knows how to deal with.
    ///
    /// [`Populator::finish`] is what makes the volume usable; this is
    /// what makes giving up honest.
    pub fn abandon(self) -> S {
        self.src
    }

    // -- creating entries --------------------------------------------------

    /// Create a directory in `parent`, and return its block.
    ///
    /// On `DOS\4`/`DOS\5` the new directory gets its own empty
    /// `T_DIRCACHE` block at the same time, because that is what a
    /// directory on those variants has from birth — an empty cache is not
    /// a null pointer.
    pub fn create_dir(
        &mut self,
        parent: u64,
        name: &[u8],
        meta: &Metadata<'_>,
    ) -> Result<u64, PopulateError<Transport<S>>> {
        let prep = self.prepare(parent, name, meta)?;
        let bs = self.block_size;
        let mut hdr = vec![0u8; bs];

        // The directory's own cache, before its header names it: a header
        // pointing at a block that was never written is the one shape the
        // write order exists to rule out.
        if self.variant.has_dircache() {
            let dc = self.alloc()?;
            self.write_dircache_block(dc, prep.lba, &[])?;
            wr32(&mut hdr, tail(bs, TL_EXTENSION), dc as u32);
        }

        self.emit(&prep, EntryKind::Directory, 0, &mut hdr)
    }

    /// Create a file in `parent` from bytes already in memory.
    pub fn create_file(
        &mut self,
        parent: u64,
        name: &[u8],
        meta: &Metadata<'_>,
        data: &[u8],
    ) -> Result<u64, PopulateError<Transport<S>>> {
        let mut off = 0usize;
        self.create_file_with(parent, name, meta, |buf| {
            let n = buf.len().min(data.len() - off);
            buf[..n].copy_from_slice(&data[off..off + n]);
            off += n;
            n
        })
    }

    /// Create a file in `parent`, pulling its contents a chunk at a time.
    ///
    /// `chunk` fills as much of `buf` as it has and returns how many bytes
    /// it wrote; **0 means end of file**. It may be called more than once
    /// per data block — a short return is topped up, not treated as the
    /// end — because a short data block anywhere but the last one is a
    /// volume no reader will accept.
    ///
    /// The callback cannot fail, for the same reason
    /// [`Volume::read_file_with`](crate::Volume::read_file_with)'s cannot:
    /// a source that can fail (a host file, a socket) captures its own
    /// error, returns 0, and the caller checks it after this returns.
    /// Threading a second error type through here would put the caller's
    /// failure inside this crate's error, which is a shape the transport
    /// error already occupies for a different reason. The file will be
    /// short in that case, which is why the caller must check.
    ///
    /// Memory is bounded at four blocks whatever the file's size: the
    /// header, the extension block being filled, and two data buffers —
    /// two because an OFS data block records its *successor*, so one block
    /// has to be held back until the next one has an LBA.
    pub fn create_file_with<F>(
        &mut self,
        parent: u64,
        name: &[u8],
        meta: &Metadata<'_>,
        mut chunk: F,
    ) -> Result<u64, PopulateError<Transport<S>>>
    where
        F: FnMut(&mut [u8]) -> usize,
    {
        let prep = self.prepare(parent, name, meta)?;
        let hdr_lba = prep.lba;
        let bs = self.block_size;
        let ffs = self.variant.is_ffs();
        let payload = data_payload_size(bs, ffs);
        let slots = hash_table_size(bs) as usize;

        let mut hdr = vec![0u8; bs];
        // The extension block currently being filled, held until its
        // successor's LBA is known (or the file ends).
        let mut ext: Option<(u64, Vec<u8>)> = None;
        let mut in_table = 0usize;
        let mut total: u64 = 0;
        let mut seq: u32 = 0;

        let mut fresh = vec![0u8; bs];
        let mut held = vec![0u8; bs];
        let mut pending: Option<(u64, usize, u32)> = None;

        loop {
            let n = fill(&mut chunk, &mut fresh[..payload]);
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > u64::from(u32::MAX) {
                return Err(PopulateError::FileTooLarge { len: total });
            }

            // A full table means a new extension block, allocated before
            // the data block it will point at so the file's blocks stay
            // in ascending order on the medium.
            if in_table == slots {
                let ext_lba = self.alloc()?;
                match ext.take() {
                    None => wr32(&mut hdr, tail(bs, TL_EXTENSION), ext_lba as u32),
                    Some((prev, mut prev_buf)) => {
                        wr32(&mut prev_buf, tail(bs, TL_EXTENSION), ext_lba as u32);
                        self.put_buf_checked(prev, &mut prev_buf)?;
                    }
                }
                let mut nb = vec![0u8; bs];
                wr32(&mut nb, OFF_TYPE, T_LIST);
                wr32(&mut nb, OFF_OWN_KEY, ext_lba as u32);
                wr32(&mut nb, tail(bs, TL_PARENT), hdr_lba as u32);
                wr32(&mut nb, tail(bs, TL_SECONDARY_TYPE), ST_FILE as u32);
                ext = Some((ext_lba, nb));
                in_table = 0;
            }

            let lba = self.alloc()?;
            seq += 1;
            if seq == 1 {
                wr32(&mut hdr, OFF_FIRST_DATA, lba as u32);
            }
            if let Some((prev, len, pseq)) = pending.take() {
                self.write_data_block(prev, hdr_lba, pseq, &held[..len], lba as u32, ffs)?;
            }
            core::mem::swap(&mut fresh, &mut held);
            pending = Some((lba, n, seq));

            in_table += 1;
            let off = data_pointer_offset(bs, in_table as u32);
            let table = match ext {
                Some((_, ref mut buf)) => buf,
                None => &mut hdr,
            };
            wr32(table, off, lba as u32);
            wr32(table, OFF_HIGH_SEQ, in_table as u32);
        }

        if let Some((prev, len, pseq)) = pending.take() {
            self.write_data_block(prev, hdr_lba, pseq, &held[..len], 0, ffs)?;
        }
        if let Some((lba, mut buf)) = ext.take() {
            self.put_buf_checked(lba, &mut buf)?;
        }

        self.emit(&prep, EntryKind::File, total as u32, &mut hdr)
    }

    // -- the machinery -----------------------------------------------------

    /// Everything that must be settled before a block is allocated: the
    /// name is legal, the parent is a directory, the name is not already
    /// there, and where in the parent's hash table the entry will go.
    fn prepare<'a>(
        &mut self,
        parent: u64,
        name: &'a [u8],
        meta: &'a Metadata<'a>,
    ) -> Result<Prepared<'a>, PopulateError<Transport<S>>> {
        check_name_bytes(name, self.max_name_len()).map_err(|p| match p {
            NameProblem::Empty => PopulateError::NameEmpty,
            NameProblem::TooLong { len, max } => PopulateError::NameTooLong { len, max },
            NameProblem::InvalidByte { byte, index } => {
                PopulateError::NameInvalidByte { byte, index }
            }
        })?;
        if meta.comment.len() > COMMENT_MAX {
            return Err(PopulateError::CommentTooLong {
                len: meta.comment.len(),
                max: COMMENT_MAX,
            });
        }

        let bs = self.block_size;
        self.read_checked(parent)?;
        let st = be32(&self.buf, tail(bs, TL_SECONDARY_TYPE)) as i32;
        if be32(&self.buf, OFF_TYPE) != T_HEADER || (st != ST_USERDIR && st != ST_ROOT) {
            return Err(PopulateError::NotADirectory {
                lba: parent,
                found: st,
            });
        }
        let fold = self.variant.fold();
        let slot = name_hash(name, fold, hash_table_size(bs)) as usize;
        let head = be32(&self.buf, OFF_HASH_TABLE + slot * 4);

        // Walk the one chain the name would be found in. Bounded by the
        // volume's block count, because a chain this module wrote cannot
        // loop and a medium that reports one that does must not hang us.
        let mut next = head;
        let mut steps = 0u64;
        while next != 0 {
            let lba = next as u64;
            steps += 1;
            if steps > self.block_count {
                return Err(PopulateError::LbaOutOfRange {
                    lba,
                    block_count: self.block_count,
                });
            }
            let (found, chain) = self.entry_name_at(lba)?;
            if names_equal(&found, name, fold) {
                return Err(PopulateError::DuplicateName {
                    parent,
                    name: name.to_vec(),
                });
            }
            next = chain;
        }

        let lba = self.alloc()?;
        // The comment goes in its own block only when the name left no
        // room beside it — which can only happen on LNFS, where the two
        // share one 112-byte field.
        let comment_block =
            if self.variant.has_long_names() && 1 + name.len() + 1 + meta.comment.len() > NAC_LEN {
                let cb = self.alloc()?;
                self.buf.iter_mut().for_each(|b| *b = 0);
                wr32(&mut self.buf, OFF_TYPE, T_COMMENT);
                wr32(&mut self.buf, OFF_OWN_KEY, cb as u32);
                wr32(&mut self.buf, OFF_COMMENT_HEADER_KEY, lba as u32);
                wr_bcpl(&mut self.buf, OFF_COMMENT_TEXT, meta.comment);
                self.put_checked(cb)?;
                cb as u32
            } else {
                0
            };

        Ok(Prepared {
            lba,
            slot,
            head,
            comment_block,
            parent,
            name,
            meta,
        })
    }

    /// Write the header block, chain it into its parent, and cache it.
    ///
    /// The order is the whole point: the header exists in full before
    /// anything points at it, and the dircache record — which is advisory
    /// — is written after the chain that is authoritative.
    fn emit(
        &mut self,
        prep: &Prepared<'_>,
        kind: EntryKind,
        byte_size: u32,
        hdr: &mut [u8],
    ) -> Result<u64, PopulateError<Transport<S>>> {
        let bs = self.block_size;
        let (lba, parent, name, meta) = (prep.lba, prep.parent, prep.name, prep.meta);
        wr32(hdr, OFF_TYPE, T_HEADER);
        wr32(hdr, OFF_OWN_KEY, lba as u32);
        wr32(hdr, tail(bs, TL_OWNER), meta.owner);
        wr32(hdr, tail(bs, TL_PROTECTION), meta.protection);
        if matches!(kind, EntryKind::File) {
            wr32(hdr, tail(bs, TL_BYTE_SIZE), byte_size);
        }
        if self.variant.has_long_names() {
            let off = tail(bs, TL_NAC);
            wr_bcpl(hdr, off, name);
            let after = off + 1 + name.len();
            if prep.comment_block == 0 {
                wr_bcpl(hdr, after, meta.comment);
            } else {
                // The inline comment is left empty and longword −18 names
                // the block that has it — the exact state an LNFS volume
                // is in when a long name crowded the comment out, and the
                // one a reader must not mistake for "no comment".
                hdr[after] = 0;
                wr32(hdr, tail(bs, TL_COMMENT_BLOCK), prep.comment_block);
            }
            wr_date(hdr, tail(bs, TL_DATE_LONG), meta.date);
        } else {
            wr_bcpl(hdr, tail(bs, TL_NAME), name);
            wr_bcpl(hdr, tail(bs, TL_COMMENT), meta.comment);
            wr_date(hdr, tail(bs, TL_DATE), meta.date);
        }
        wr32(hdr, tail(bs, TL_HASH_CHAIN), prep.head);
        wr32(hdr, tail(bs, TL_PARENT), parent as u32);
        wr32(
            hdr,
            tail(bs, TL_SECONDARY_TYPE),
            kind.secondary_type() as u32,
        );
        let ck = checksum_compute(hdr, CHECKSUM_INDEX);
        wr32(hdr, OFF_CHECKSUM, ck);
        self.write(lba, hdr)?;

        // Head insertion, as the oracle does it: the new entry takes the
        // slot and the old head becomes its chain.
        self.read_checked(parent)?;
        wr32(&mut self.buf, OFF_HASH_TABLE + prep.slot * 4, lba as u32);
        self.put_checked(parent)?;

        if self.variant.has_dircache() {
            self.dircache_append(parent, lba, name, meta, kind, byte_size)?;
        }
        Ok(lba)
    }

    /// Append one record to a directory's dircache, spilling into a new
    /// chained block when the last one is full.
    ///
    /// Walks to the end of the chain rather than remembering where it got
    /// to: the chains are short (a 512-byte block holds around nineteen
    /// records) and a per-directory cursor would be state that can go
    /// stale against the disk, which is the whole failure mode dircaches
    /// have.
    fn dircache_append(
        &mut self,
        dir: u64,
        entry: u64,
        name: &[u8],
        meta: &Metadata<'_>,
        kind: EntryKind,
        byte_size: u32,
    ) -> Result<(), PopulateError<Transport<S>>> {
        let bs = self.block_size;
        let record = dircache_record(entry, byte_size, meta, kind, name);

        self.read_checked(dir)?;
        let mut block = be32(&self.buf, tail(bs, TL_EXTENSION)) as u64;
        let mut steps = 0u64;
        loop {
            self.read_checked(block)?;
            let next = be32(&self.buf, OFF_DIRCACHE_NEXT) as u64;
            if next == 0 {
                break;
            }
            steps += 1;
            if steps > self.block_count {
                return Err(PopulateError::LbaOutOfRange {
                    lba: next,
                    block_count: self.block_count,
                });
            }
            block = next;
        }

        // `self.buf` still holds the last block of the chain.
        let count = be32(&self.buf, OFF_DIRCACHE_RECORDS);
        let used = dircache_used(&self.buf, count);
        if used + record.len() <= bs {
            wr32(&mut self.buf, OFF_DIRCACHE_RECORDS, count + 1);
            self.buf[used..used + record.len()].copy_from_slice(&record);
            return self.put_checked(block);
        }

        let fresh = self.alloc()?;
        self.write_dircache_block(fresh, dir, &record)?;
        self.read_checked(block)?;
        wr32(&mut self.buf, OFF_DIRCACHE_NEXT, fresh as u32);
        self.put_checked(block)
    }

    fn write_dircache_block(
        &mut self,
        lba: u64,
        dir: u64,
        record: &[u8],
    ) -> Result<(), PopulateError<Transport<S>>> {
        self.buf.iter_mut().for_each(|b| *b = 0);
        wr32(&mut self.buf, OFF_TYPE, T_DIRCACHE);
        wr32(&mut self.buf, OFF_OWN_KEY, lba as u32);
        wr32(&mut self.buf, OFF_DIRCACHE_PARENT, dir as u32);
        wr32(
            &mut self.buf,
            OFF_DIRCACHE_RECORDS,
            u32::from(!record.is_empty()),
        );
        wr32(&mut self.buf, OFF_DIRCACHE_NEXT, 0);
        let start = OFF_DIRCACHE_RECORDS_START;
        self.buf[start..start + record.len()].copy_from_slice(record);
        self.put_checked(lba)
    }

    /// One data block: raw payload on FFS, six longwords of header and a
    /// checksum on OFS.
    fn write_data_block(
        &mut self,
        lba: u64,
        header: u64,
        seq: u32,
        data: &[u8],
        next: u32,
        ffs: bool,
    ) -> Result<(), PopulateError<Transport<S>>> {
        self.buf.iter_mut().for_each(|b| *b = 0);
        if ffs {
            self.buf[..data.len()].copy_from_slice(data);
            self.put(lba)
        } else {
            wr32(&mut self.buf, OFF_TYPE, T_DATA);
            wr32(&mut self.buf, OFF_DATA_HEADER_KEY, header as u32);
            wr32(&mut self.buf, OFF_DATA_SEQ, seq);
            wr32(&mut self.buf, OFF_DATA_SIZE, data.len() as u32);
            wr32(&mut self.buf, OFF_DATA_NEXT, next);
            self.buf[OFF_DATA_PAYLOAD..OFF_DATA_PAYLOAD + data.len()].copy_from_slice(data);
            self.put_checked(lba)
        }
    }

    /// The name and hash chain of an entry, for the duplicate check.
    fn entry_name_at(&mut self, lba: u64) -> Result<(Vec<u8>, u32), PopulateError<Transport<S>>> {
        let bs = self.block_size;
        self.read_checked(lba)?;
        let name = if self.variant.has_long_names() {
            crate::bcpl_str(&self.buf, tail(bs, TL_NAC), MAX_NAME_LONG).to_vec()
        } else {
            crate::bcpl_str(&self.buf, tail(bs, TL_NAME), MAX_NAME_CLASSIC).to_vec()
        };
        Ok((name, be32(&self.buf, tail(bs, TL_HASH_CHAIN))))
    }

    // -- allocation and block I/O ------------------------------------------

    /// The next free block, upward from the first one the volume has.
    ///
    /// Starting at `reserved` rather than after the root uses the whole
    /// volume — half of it lives *below* the root, which sits at the
    /// midpoint — and the only run to step over is the format's own,
    /// which is contiguous.
    fn alloc(&mut self) -> Result<u64, PopulateError<Transport<S>>> {
        if self.next_free >= self.format_span.0 && self.next_free < self.format_span.1 {
            self.next_free = self.format_span.1;
        }
        if self.next_free >= self.block_count {
            return Err(PopulateError::VolumeFull {
                block_count: self.block_count,
            });
        }
        let lba = self.next_free;
        self.next_free += 1;
        if self.next_free >= self.format_span.0 && self.next_free < self.format_span.1 {
            self.next_free = self.format_span.1;
        }
        Ok(lba)
    }

    fn set_bitmap_flag(&mut self, flag: i32) -> Result<(), PopulateError<Transport<S>>> {
        let bs = self.block_size;
        let root = self.root_lba;
        self.read_checked(root)?;
        wr32(&mut self.buf, tail(bs, TL_BITMAP_FLAG), flag as u32);
        self.put_checked(root)
    }

    fn read_checked(&mut self, lba: u64) -> Result<(), PopulateError<Transport<S>>> {
        if lba >= self.block_count {
            return Err(PopulateError::LbaOutOfRange {
                lba,
                block_count: self.block_count,
            });
        }
        self.src
            .read_block(lba, &mut self.buf)
            .map_err(PopulateError::Io)?;
        if !crate::checksum_ok(&self.buf) {
            return Err(PopulateError::Checksum { lba });
        }
        Ok(())
    }

    /// Write the scratch buffer as it stands.
    fn put(&mut self, lba: u64) -> Result<(), PopulateError<Transport<S>>> {
        if lba >= self.block_count {
            return Err(PopulateError::LbaOutOfRange {
                lba,
                block_count: self.block_count,
            });
        }
        self.src
            .write_block(lba, &self.buf)
            .map_err(PopulateError::Io)
    }

    /// Fix the scratch buffer's checksum at longword 5 and write it.
    fn put_checked(&mut self, lba: u64) -> Result<(), PopulateError<Transport<S>>> {
        let ck = checksum_compute(&self.buf, CHECKSUM_INDEX);
        wr32(&mut self.buf, OFF_CHECKSUM, ck);
        self.put(lba)
    }

    fn write(&mut self, lba: u64, block: &[u8]) -> Result<(), PopulateError<Transport<S>>> {
        if lba >= self.block_count {
            return Err(PopulateError::LbaOutOfRange {
                lba,
                block_count: self.block_count,
            });
        }
        self.src.write_block(lba, block).map_err(PopulateError::Io)
    }

    fn put_buf_checked(
        &mut self,
        lba: u64,
        block: &mut [u8],
    ) -> Result<(), PopulateError<Transport<S>>> {
        let ck = checksum_compute(block, CHECKSUM_INDEX);
        wr32(block, OFF_CHECKSUM, ck);
        self.write(lba, block)
    }
}

/// What [`Populator::prepare`] settled before anything was written.
///
/// It carries the caller's `parent`, `name` and metadata onward rather
/// than making [`Populator::emit`] take them again: they were already
/// validated here, and a second copy of the argument list is a second
/// chance to pass them in a different order.
struct Prepared<'a> {
    lba: u64,
    slot: usize,
    head: u32,
    comment_block: u32,
    parent: u64,
    name: &'a [u8],
    meta: &'a Metadata<'a>,
}

/// Top up `buf` from `chunk` until it is full or the source is done.
///
/// A caller that hands back fewer bytes than asked for has not
/// necessarily finished — a `Read` is allowed to be short — and a data
/// block that is short in the middle of a file is a volume no reader will
/// accept, so the shortfall is topped up here rather than written out.
fn fill<F: FnMut(&mut [u8]) -> usize>(chunk: &mut F, buf: &mut [u8]) -> usize {
    let mut got = 0;
    while got < buf.len() {
        let n = chunk(&mut buf[got..]);
        if n == 0 {
            break;
        }
        got += n.min(buf.len() - got);
    }
    got
}

/// Where the records in a dircache block end: walk `count` of them,
/// because their lengths depend on the two counted strings inside them.
fn dircache_used(block: &[u8], count: u32) -> usize {
    let mut off = OFF_DIRCACHE_RECORDS_START;
    for _ in 0..count {
        if off + DIRCACHE_RECORD_MIN > block.len() {
            return block.len();
        }
        let name_len = block[off + DC_NAME_LEN] as usize;
        if off + DIRCACHE_RECORD_FIXED + name_len >= block.len() {
            return block.len();
        }
        let comment_len = block[off + DIRCACHE_RECORD_FIXED + name_len] as usize;
        off += dircache_record_len(name_len, comment_len);
    }
    off
}

/// Pack one dircache record.
///
/// The `DateStamp` is narrowed to three *words* — the format's own
/// narrowing, not this crate's — so a cached date runs out in 2157 where
/// the header block's runs out in 11.7 million. Truncating rather than
/// clamping is what AmigaDOS does, and the cache is advisory anyway:
/// [`Volume::validate`](crate::Volume::validate) never compares dates,
/// and nothing in this crate resolves anything through a cache.
fn dircache_record(
    entry: u64,
    byte_size: u32,
    meta: &Metadata<'_>,
    kind: EntryKind,
    name: &[u8],
) -> Vec<u8> {
    let mut r = vec![0u8; dircache_record_len(name.len(), meta.comment.len())];
    wr32(&mut r, DC_ENTRY, entry as u32);
    wr32(&mut r, DC_SIZE, byte_size);
    wr32(&mut r, DC_PROTECTION, meta.protection);
    wr16(&mut r, DC_UID, (meta.owner >> 16) as u16);
    wr16(&mut r, DC_GID, meta.owner as u16);
    wr16(&mut r, DC_DAYS, meta.date.days as u16);
    wr16(&mut r, DC_MINS, meta.date.mins as u16);
    wr16(&mut r, DC_TICKS, meta.date.ticks as u16);
    // xdftool leaves this byte zero on every record it writes; filling it
    // in is strictly more information, and a validator reads a zero as
    // "not recorded" rather than as a disagreement either way.
    r[DC_TYPE] = kind.secondary_type() as i8 as u8;
    r[DC_NAME_LEN] = name.len() as u8;
    let n = DIRCACHE_RECORD_FIXED;
    r[n..n + name.len()].copy_from_slice(name);
    r[n + name.len()] = meta.comment.len() as u8;
    let c = n + name.len() + 1;
    r[c..c + meta.comment.len()].copy_from_slice(meta.comment);
    r
}

fn wr16(block: &mut [u8], off: usize, v: u16) {
    block[off..off + 2].copy_from_slice(&v.to_be_bytes());
}

// ---------------------------------------------------------------------------
// The std layer: a host directory tree
// ---------------------------------------------------------------------------

/// How a host file's mode becomes an Amiga protection longword.
///
/// | host | Amiga | why |
/// |---|---|---|
/// | `u+r` | `FIBF_READ` **clear** | the owner nibble denies; a clear bit allows |
/// | `u+w` | `FIBF_WRITE` clear | as above |
/// | `u+x` | `FIBF_EXECUTE` clear | the host's executable bit *removes* the E denial |
/// | — | `FIBF_DELETE` always clear | POSIX governs deletion by the *directory's* write bit, not the file's; there is nothing per-file to map, and denying deletion on every copied file would make the image tiresome to use |
/// | `g+r/w/x` | `FIBF_GRP_READ`/`WRITE`/`EXECUTE` **set** | the group nibble grants, the normal sense |
/// | `o+r/w/x` | `FIBF_OTR_READ`/`WRITE`/`EXECUTE` set | as above |
/// | — | `FIBF_GRP_DELETE`, `FIBF_OTR_DELETE` clear | POSIX has no per-file delete bit to map |
/// | — | `FIBF_ARCHIVE` clear | "backed up since last change" is a fact about a backup this crate did not take |
/// | — | `FIBF_SCRIPT`, `FIBF_PURE`, `FIBF_HIDDEN` clear | Amiga-only facts with no host equivalent; sniffing a `#!` line to guess `s` would be inventing metadata |
///
/// Off Unix there is no mode to read, so the one bit the standard library
/// does expose is used: a read-only file gets `FIBF_WRITE` set and nothing
/// else changes.
///
/// The owner longword is **not** derived from the host's uid/gid. A muFS
/// UID is a 16-bit number in a namespace the host knows nothing about, and
/// truncating a host uid into it would invent an owner; ownership stays
/// the caller's to set through [`Metadata::owner`].
#[cfg(feature = "std")]
pub fn protection_from_metadata(md: &std::fs::Metadata) -> u32 {
    use crate::meta::*;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = md.permissions().mode();
        let mut p = 0u32;
        if m & 0o400 == 0 {
            p |= FIBF_READ;
        }
        if m & 0o200 == 0 {
            p |= FIBF_WRITE;
        }
        if m & 0o100 == 0 {
            p |= FIBF_EXECUTE;
        }
        if m & 0o040 != 0 {
            p |= FIBF_GRP_READ;
        }
        if m & 0o020 != 0 {
            p |= FIBF_GRP_WRITE;
        }
        if m & 0o010 != 0 {
            p |= FIBF_GRP_EXECUTE;
        }
        if m & 0o004 != 0 {
            p |= FIBF_OTR_READ;
        }
        if m & 0o002 != 0 {
            p |= FIBF_OTR_WRITE;
        }
        if m & 0o001 != 0 {
            p |= FIBF_OTR_EXECUTE;
        }
        p
    }
    #[cfg(not(unix))]
    {
        if md.permissions().readonly() {
            FIBF_WRITE
        } else {
            0
        }
    }
}

/// A host timestamp as an AmigaDOS `DateStamp`.
///
/// Anything before 1978-01-01 — the epoch the format counts from — has no
/// representation at all, so it becomes the epoch rather than a wrapped
/// number that looks like a real date. Sub-second precision survives as
/// far as the format allows: ticks are fiftieths of a second, so a host
/// nanosecond field lands in a 20 ms bucket.
#[cfg(feature = "std")]
pub fn datestamp_from_system_time(t: std::time::SystemTime) -> DateStamp {
    use crate::meta::{AMIGA_EPOCH_UNIX_DAYS, MINUTES_PER_DAY, TICKS_PER_SECOND};
    let d = match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d,
        Err(_) => return DateStamp::default(),
    };
    let amiga = d.as_secs() as i64 - AMIGA_EPOCH_UNIX_DAYS * 86_400;
    if amiga < 0 {
        return DateStamp::default();
    }
    let days = amiga / 86_400;
    if days > i64::from(u32::MAX) {
        return DateStamp {
            days: u32::MAX,
            mins: MINUTES_PER_DAY - 1,
            ticks: 0,
        };
    }
    let rest = (amiga % 86_400) as u32;
    DateStamp {
        days: days as u32,
        mins: rest / 60,
        ticks: (rest % 60) * TICKS_PER_SECOND + d.subsec_nanos() / 20_000_000,
    }
}

/// A host file name as Latin-1 bytes, or `None` if it is not one.
///
/// Two ways to fail, both refused rather than papered over: a name that is
/// not UTF-8 (so there is nothing to convert *from*), and a name with a
/// character above U+00FF (so there is nothing to convert *to*).
#[cfg(feature = "std")]
pub fn latin1_name(name: &std::ffi::OsStr) -> Option<Vec<u8>> {
    let s = name.to_str()?;
    let mut out = Vec::with_capacity(s.len());
    for ch in s.chars() {
        let c = ch as u32;
        if c > 0xFF {
            return None;
        }
        out.push(c as u8);
    }
    Some(out)
}

#[cfg(feature = "std")]
impl<S: BlockMedium> Populator<S> {
    /// Copy a host directory's contents into `parent`, recursively.
    ///
    /// Files and directories only: anything else — a symlink, a socket, a
    /// device node — is [`PopulateError::HostUnsupported`] naming the
    /// path, because a soft link needs an *Amiga* path this crate cannot
    /// derive and the rest have no filesystem representation at all.
    ///
    /// Entries are copied in **sorted order**, so the same tree produces
    /// the same image byte for byte. `read_dir` order is the host
    /// filesystem's and varies between machines and runs; an image builder
    /// whose output is not reproducible is one whose output cannot be
    /// diffed.
    ///
    /// Metadata comes across per [`protection_from_metadata`] and
    /// [`datestamp_from_system_time`]; comments and owners do not exist on
    /// the host and are left empty and zero.
    pub fn add_host_tree(
        &mut self,
        parent: u64,
        dir: &std::path::Path,
    ) -> Result<(), PopulateError<Transport<S>>> {
        let host = |path: &std::path::Path, error: std::io::Error| PopulateError::Host {
            path: path.to_path_buf(),
            error,
        };

        let mut stack = vec![(parent, dir.to_path_buf())];
        while let Some((into, here)) = stack.pop() {
            let mut children: Vec<std::path::PathBuf> = Vec::new();
            for entry in std::fs::read_dir(&here).map_err(|e| host(&here, e))? {
                let entry = entry.map_err(|e| host(&here, e))?;
                children.push(entry.path());
            }
            children.sort();

            for path in children {
                let name = match path.file_name().and_then(latin1_name) {
                    Some(n) => n,
                    None => return Err(PopulateError::HostNameNotLatin1 { path }),
                };
                // symlink_metadata, so a symlink is seen as a symlink and
                // refused rather than silently followed out of the tree.
                let md = std::fs::symlink_metadata(&path).map_err(|e| host(&path, e))?;
                let meta = Metadata::new()
                    .protection(protection_from_metadata(&md))
                    .date(
                        md.modified()
                            .map(datestamp_from_system_time)
                            .unwrap_or_default(),
                    );
                if md.is_dir() {
                    let child = self.create_dir(into, &name, &meta)?;
                    stack.push((child, path));
                } else if md.is_file() {
                    let mut file = std::fs::File::open(&path).map_err(|e| host(&path, e))?;
                    let mut failure: Option<std::io::Error> = None;
                    self.create_file_with(into, &name, &meta, |buf| {
                        use std::io::Read;
                        match file.read(buf) {
                            Ok(n) => n,
                            Err(e) => {
                                failure.get_or_insert(e);
                                0
                            }
                        }
                    })?;
                    if let Some(e) = failure {
                        return Err(host(&path, e));
                    }
                } else {
                    return Err(PopulateError::HostUnsupported { path });
                }
            }
        }
        Ok(())
    }
}

/// Format a volume and fill it from a host directory, in one call.
///
/// The whole of "turn this directory into a disk image": the operation
/// amibake performs to build an HDF and Copperline performs to hand a
/// guest a drive made out of a host folder. Equivalent to
/// [`Populator::new`], [`Populator::add_host_tree`] on the root, and
/// [`Populator::finish`] — which is what to reach for when the tree needs
/// anything the plain walk does not do (a comment, an owner, an entry
/// that is not on the host at all).
#[cfg(feature = "std")]
pub fn populate_from_tree<S: BlockMedium>(
    src: S,
    opts: &FormatOptions<'_>,
    dir: &std::path::Path,
) -> Result<S, PopulateError<Transport<S>>> {
    let mut pop = Populator::new(src, opts)?;
    let root = pop.root_lba();
    pop.add_host_tree(root, dir)?;
    pop.finish()
}
