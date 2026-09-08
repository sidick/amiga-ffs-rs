//! Creating, deleting and renaming entries in a volume that already
//! exists.
//!
//! [`Populator`](crate::Populator) writes into a volume this crate has
//! just formatted, over an extent it *knows* is free. [`Mutator`] is the
//! other case, and the one milestone 3 is about: a volume somebody else
//! wrote — the ROM's FFS, xdftool, a guest that crashed mid-write — opened
//! read-write, changed in place. Every block it hands out comes from that
//! volume's own bitmap through [`Allocator`], every block it gives back
//! goes there too, and the order the writes go down in is the whole of the
//! crash safety, because the format has no journal.
//!
//! # What it will not touch
//!
//! A volume whose root says `bitmap_flag == 0` is refused outright
//! ([`AllocError::BitmapInvalid`], surfaced as [`MutateError::Alloc`]).
//! Those bits are whatever an interrupted update left behind and an
//! allocator acting on them hands out blocks a file is using;
//! [`Volume::repair`](crate::Volume::repair) is the operation that turns
//! such a volume back into one there is something to allocate from. That
//! refusal is inherited from the allocator rather than reimplemented here.
//!
//! # Write order, per operation
//!
//! Three rules, applied everywhere, each stated again at the operation
//! that uses it:
//!
//! 1. **Bitmap first when allocating.** A block is marked, the page
//!    carrying its bit is flushed, and only then may its number appear in
//!    anybody's pointer — which [`Allocator::reference`] enforces by
//!    refusing until the flush has happened. Crash anywhere in that order
//!    and the block is allocated and unreachable: a leak
//!    ([`Finding::OrphanBlock`](crate::Finding::OrphanBlock)), which a
//!    [`repair`](crate::repair) fixes.
//! 2. **Content before the pointer that reaches it.** Data blocks,
//!    extension blocks, comment blocks and the header itself are complete
//!    on the disk before the parent's hash slot names the header. The last
//!    write of a create is a single longword in the parent, and until it
//!    lands nothing in the directory has changed.
//! 3. **Bitmap last when freeing.** The entry is spliced out of its
//!    parent's chain (and out of its target's link chain, where it is a
//!    link) and *that* write is on the disk before a single bit is
//!    cleared. Crash anywhere in that order and the blocks are, again,
//!    leaked rather than handed to a second owner.
//!
//! Every intermediate state is therefore either the old volume, the new
//! volume, or the old volume plus some leaked blocks. What never happens
//! is [`Finding::ReachableButFree`](crate::Finding::ReachableButFree) —
//! and unlike a populate, the bitmap flag stays −1 throughout, because at
//! no point does the bitmap claim a reachable block is free. The crash
//! sweep in `tests/volumes.rs` asserts exactly that, at every prefix of
//! every operation's writes.
//!
//! # File contents: the one place two blocks change together
//!
//! [`Mutator::write_file`], [`Mutator::append`] and [`Mutator::truncate`]
//! are the operations the three rules above do not fully cover, because a
//! file's *length* and its *extent* are recorded in different blocks and
//! must agree: [`Volume::file_chain`](crate::Volume::file_chain) refuses a
//! header whose `byte_size` and data-block count disagree, in both
//! directions, so any order that moves one before the other leaves a file
//! that will not read. Three decisions make that go away.
//!
//! **The file header block is the single commit.** Every size change ends
//! in exactly one write — the header, carrying `byte_size`, `high_seq`,
//! the whole data-pointer table and the extension pointer together. Before
//! that write the file is entirely the old one; after it, entirely the new
//! one.
//!
//! **The extension chain is immutable: it is rebuilt, never patched.**
//! That falls out of the previous rule rather than being a taste. A
//! `T_LIST` block is reachable only through the block before it, back to
//! the header, so *editing* one in place would commit a new block count
//! from a block that is not the header — and the header's `byte_size` would
//! still be the old one, which is precisely the unreadable state. So a
//! size change allocates a fresh chain, writes it (last block first, so no
//! `next` ever names a block that is not there), points the header at it,
//! and frees the old blocks afterwards. It costs one block per ~35 KB of
//! OFS file per size change, which is the price of the commit being one
//! write; a caller appending a byte at a time to a large file should
//! buffer, and every entry point here takes a whole slice for that reason.
//!
//! **A data block whose recorded length changes is replaced, not
//! edited** — on OFS only, where the length *is* recorded (longword 3 of
//! the block's own header). Growing a file makes its old last block full
//! and shrinking makes some earlier block short, and either way that
//! block's `size` field has to move in step with the header's `byte_size`.
//! Writing the new content to a freshly allocated block and swapping the
//! pointer in the commit keeps that single-write property. On FFS there is
//! no such field — the last block's length is `byte_size` arithmetic and
//! nothing else — so the boundary block is written in place, and the bytes
//! it gains past the old end of file are unreachable until the commit
//! makes them part of the file.
//!
//! ## The caveat: an in-place overwrite is not crash-atomic
//!
//! Data blocks that are entirely inside both the old file and the new one
//! and keep their length are **overwritten in place**, with a
//! read-modify-write for a partial first or last block. That is the one
//! place this module's "old volume, new volume, or old volume plus leaks"
//! invariant weakens, and it weakens to exactly: **old bytes or new bytes,
//! independently per block**. It is inherent to overwriting rather than a
//! shortcut — the alternative is copy-on-writing every touched block,
//! which turns every overwrite into a reallocation and makes a file's
//! blocks migrate across the volume for no gain the format can express.
//! What still holds through it is everything structural: the block count,
//! `byte_size`, the pointer tables and the bitmap are untouched by an
//! overwrite, so an interrupted one leaves a file of the right length made
//! of some old and some new blocks, which reads, validates and can simply
//! be written again. `ReachableButFree` remains impossible.
//!
//! ## Writing past the end: the gap is zero-filled
//!
//! [`Mutator::write_file`] at an offset past the current end of file, and
//! [`Mutator::truncate`] to a larger size, both **allocate every block in
//! the gap and fill it with zeroes**. The format has no other option: a
//! file's pointer table is dense by construction — data block *n* is the
//! *n*th table slot, counted through the extension chain, and slot 0 is
//! the chain's terminator — so there is no encoding for a hole and nothing
//! to be sparse with.
//!
//! Which leaves *what the gap contains*, and AmigaDOS answers that
//! deliberately loosely. `Seek()` cannot create the situation at all:
//! "You cannot Seek() beyond the end of a file" (`dos.library/Seek`
//! autodoc), and `ACTION_SEEK` "shall fail with the error code
//! `ERROR_SEEK_ERROR`" for a position "beyond the end-of-file", leaving
//! the file pointer unaltered — so seek-past-EOF-then-write is not a
//! behaviour to be compatible with, it is a refusal. The only route to an
//! extended file is `SetFileSize()`, whose autodoc says "if the file is
//! extended, no values should be assumed for the new bytes" and
//! `ACTION_SET_FILE_SIZE`'s specification says outright that "unlike other
//! operating systems, AmigaDOS does not enforce zero-initialization of the
//! extended region". Both permitted answers exist in the wild: AROS's
//! `afs.handler` grows by calling `writeData` with a NULL buffer, which on
//! FFS does not write the newly allocated block *at all* and leaves
//! whatever was on the disk; Linux's `affs` zeroes, through
//! `cont_expand_zero` on FFS and `affs_extent_file_ofs`/`affs_getzeroblk`
//! on OFS.
//!
//! This crate zeroes, and the reason is not tidiness: the bytes a
//! freshly allocated block holds are a *deleted file's*, and handing them
//! back through a new file's length is an information leak that the
//! specification permits and nobody wants. Zero is also within what every
//! caller may assume, since the specification lets them assume nothing.
//!
//! # Dircaches: regenerated, never patched
//!
//! On `DOS\4`/`DOS\5` the affected directory's whole cache chain is
//! **rebuilt from its hash chains** after every create, delete, rename and
//! metadata change — blocks reused where the new chain is the same length
//! or shorter, allocated where it grew, freed where it shrank. Surgical
//! editing of one record would be less I/O and one more place to leave the
//! cache disagreeing with the chains; regeneration cannot, because the
//! chains are the input. PLAN's rule for this milestone is "update or
//! clear, never leave stale", and after every operation here
//! [`validate`](crate::Volume::validate) reports zero
//! [`DircacheStale`](crate::Finding::DircacheStale) findings.
//!
//! The chain is written **backwards**, last block first, so a cache block
//! is on the disk before the pointer naming it is — the same rule as
//! everywhere else. A crash mid-regeneration leaves a cache that is stale,
//! which is what a cache written by any of the many tools that do not know
//! about `DOS\4` leaves too, and what `validate` reports and a rebuild
//! fixes.
//!
//! # Dates: yes, the parent is stamped — when there is a clock to stamp
//! it with
//!
//! The question "does creating a file touch its *parent's* DateStamp"
//! has an answer on real volumes, and it took reading four
//! implementations to be sure of it:
//!
//! - **amitools' `xdftool`** (the oracle this crate's tests run against)
//!   stamps both, on every create and every delete:
//!   `ADFSDir._create_node` and `_delete` each end in
//!   `update_dir_mod_time()` — the parent's longword −23 — followed by
//!   `volume.update_disk_time()` — the root's longword −10.
//! - **Linux `affs`** stamps the parent too:
//!   `affs_insert_hash` and `affs_remove_hash` (`fs/affs/amigaffs.c`)
//!   both end with `inode_set_mtime_to_ts(dir, …); mark_inode_dirty(dir)`,
//!   which `affs_write_inode` writes to `tail->change` (−23) or, for the
//!   root, `root_change` (also −23). It does *not* stamp the root's −10
//!   per operation: that happens in `affs_commit_super`, on sync and
//!   unmount.
//! - **ADFlib** stamps the parent in `adfCreateEntry` only when the new
//!   entry becomes a hash slot's head (the collision path forgets), and
//!   `adfRemoveEntry` never stamps at all — an inconsistency, not a
//!   convention.
//! - **AROS's `afs.handler`** does not stamp on create or delete, and
//!   stamps *every ancestor up to the root* on a write.
//!
//! So the rule implemented here is amitools': **the parent directory's
//! own DateStamp and the root's `disk_altered` (−10), on every create,
//! delete and rename**; the format date (−7) is left alone forever. When
//! the parent *is* the root, its DateStamp is the root's `dir_altered`,
//! which is the same longword −23 an entry uses.
//!
//! [`Mutator::set_metadata`] stamps neither, on the same authority: what
//! changed is the entry, not the directory's contents, and `xdftool
//! protect` writes the entry's block and nothing else. The entry's own
//! date is the caller's, through [`MetaUpdate::date`].
//!
//! The catch is that this crate is `no_std` and has no clock, so it
//! cannot invent a "now". [`Mutator::clock`] supplies one, and **until it
//! is set no date is written at all** — a deliberate refusal to put a
//! wrong date on the disk rather than a decision not to keep dates. Under
//! `std`, `populate::datestamp_from_system_time(SystemTime::now())` is
//! the one line that supplies it.
//!
//! [`AllocError::BitmapInvalid`]: crate::AllocError::BitmapInvalid
//!
//! # Block layout policy: passive reorganisation
//!
//! Wave 3 of PLAN.md's "Block layout policy, and compaction" entry.
//! Waves 1 and 2 gave the allocator an [`Intent`] vocabulary and gave
//! [`Mutator`] the primitives to *retroactively* fix a volume's layout
//! (`crate::compact`'s `defragment_file`/`relocate_header`) — both driven
//! by hand, or by [`Mutator::compact`]. This wave spends none of that on
//! a separate pass: it changes what [`Mutator`]'s own everyday writes do
//! with the blocks they were already going to allocate, so a volume gets
//! a little better every time something writes to it rather than only
//! when a defragmenter is run.
//!
//! Aminet's `PFS2DefragTry` (credited in PLAN.md) defragments by copying
//! a file out and back, trusting the filesystem to lay it down afresh —
//! which works on PFS2/AFS because *their* allocators seek contiguous
//! runs, and does not transfer to stock FFS, whose allocator is a bare
//! next-fit rover with no such intent (`docs/layout-survey.md` §4). But
//! this crate *is* the writer whenever [`Mutator`] is the one mutating,
//! and it chooses placement rather than hoping — so [`Mutator::write_file`]
//! recognises the copy-out-copy-back shape (a write that replaces a
//! file's entire content) and lands the replacement as one run when the
//! volume has room for it, which is the PFS2DefragTry trick working
//! *through* FFS for the first time. See `Mutator::edit_file`'s
//! `full_rewrite` for where that is decided, and
//! `tests/volumes.rs`'s `pfs2defragtry_pattern_yields_one_run` for the
//! closed loop.
//!
//! Three changes, all gated by [`Mutator::layout_policy`] (on by
//! default):
//!
//! 1. [`Mutator::create_file`] allocates its data-and-extension sequence
//!    as one [`Allocator::allocate_run`] instead of one block at a time —
//!    a new file is born as one run whenever a run exists, not merely
//!    "usually contiguous by accident of an ascending hint," which is all
//!    the pre-wave-3 per-block loop guaranteed.
//! 2. [`Mutator::create_dir`]'s `T_DIRCACHE` block places itself with
//!    [`Intent::MetadataNearRoot`] instead of next to its own header,
//!    matching wave 1's finding that dircache blocks belong near the
//!    root, not near the directory they describe.
//! 3. `Mutator::edit_file` (the private body behind
//!    [`Mutator::write_file`], [`Mutator::append`] and
//!    [`Mutator::truncate`]) places genuinely fresh blocks — growth, or
//!    every block of a full-content rewrite — with [`Intent::DataFor`];
//!    a small append still extends near the file's own last block
//!    (`Intent::DataFor`'s own hint), unchanged from before this wave,
//!    because passive means the caller's operation dictates the work and
//!    only *where* it lands is this wave's business.
//!
//! Turning the policy off reproduces the exact pre-wave-3 allocation
//! sequence — every hint this module used before this wave still exists,
//! just gated — for a caller that wants byte-predictable placement: a
//! differential comparison, or a test asserting an exact LBA as a proxy
//! for something the test actually cares about.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::allocator::{AllocError, Allocation, Allocator, Intent};
use crate::build::{
    build_comment_block, build_data_block, build_dircache_block, dircache_record, finish_checksum,
    needs_comment_block, write_date, write_entry_header, write_name_and_comment, CacheFacts,
    EntryFields,
};
use crate::format::{check_name_bytes, div_ceil, wr32, wr_date, NameProblem};
use crate::layout::*;
use crate::populate::Metadata;
use crate::read::{DateStamp, Entry, EntryKind, Volume};
use crate::{be32, checksum_ok, hash_table_size, name_hash};
use crate::{BlockMedium, Transport, MAX_NAME_CLASSIC, MAX_NAME_LONG};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything a [`Mutator`] refuses, and why.
///
/// Generic over the medium's error for the same reason every other error
/// type in this crate is: "why did the block access fail" is a question
/// only the transport can answer, and flattening it throws the answer
/// away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutateError<E> {
    /// A write to the medium failed.
    Io(E),
    /// The read side refused something, kept whole — a bad checksum, a
    /// pointer out of range, a chain that revisits a block, a file whose
    /// length and extent disagree.
    Read(crate::read::Error<E>),
    /// The allocator refused: an invalid bitmap, a full volume, a double
    /// free, a block used as a pointer target before its bitmap page was
    /// on the disk.
    Alloc(AllocError<E>),
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
    /// A comment longer than [`COMMENT_MAX`].
    CommentTooLong {
        /// The length offered.
        len: usize,
        /// The maximum, [`COMMENT_MAX`].
        max: usize,
    },
    /// The block offered as a directory is not one.
    NotADirectory {
        /// The block.
        lba: u64,
        /// Its secondary type.
        found: i32,
    },
    /// No entry of that name in that directory.
    NotFound {
        /// The directory searched.
        parent: u64,
        /// The name, raw Latin-1.
        name: Vec<u8>,
    },
    /// The target directory already holds this name, under the volume's
    /// own fold table — so `Foo` and `FOO` collide everywhere, and `café`
    /// and `CAFÉ` collide on the international variants.
    DuplicateName {
        /// The directory.
        parent: u64,
        /// The name, raw Latin-1.
        name: Vec<u8>,
    },
    /// A directory with entries in it. Deleting one would leak its whole
    /// subtree, which is a recoverable state and still not one to produce
    /// on purpose.
    DirectoryNotEmpty {
        /// The directory.
        lba: u64,
    },
    /// The entry being deleted is the target of at least one hard link.
    /// See [`Mutator::delete`] for why this is a refusal.
    LinkedTo {
        /// The entry.
        lba: u64,
        /// The first link naming it, from longword −10.
        link: u32,
    },
    /// A directory renamed into itself or into its own subtree, which
    /// would detach the subtree from the root and make a cycle out of it.
    IntoOwnSubtree {
        /// The directory being moved.
        entry: u64,
        /// The proposed new parent.
        parent: u64,
    },
    /// An operation aimed at the volume root, which is not a directory
    /// entry: it has no name field at the entry offset, no protection, no
    /// comment, and nothing to unlink it from.
    IsRoot {
        /// The root block.
        lba: u64,
    },
    /// The entry is not in the hash chain its own name hashes to, so
    /// there is no pointer to splice it out of. The state
    /// [`Finding::WrongChainSlot`](crate::Finding::WrongChainSlot)
    /// reports; putting it right is [`repair`](crate::repair)'s job, not
    /// something to guess at while holding a delete half-done.
    NotInChain {
        /// The directory.
        dir: u64,
        /// The entry that should have been in it.
        lba: u64,
    },
    /// A hard link that is not in its target's link chain — the same
    /// shape as [`MutateError::NotInChain`], one chain over.
    NotInLinkChain {
        /// The link.
        lba: u64,
        /// The object it names.
        target: u64,
    },
    /// A write, append or truncate aimed at something that is not a plain
    /// file. A directory has no data chain; a *hard link* has none of its
    /// own either — the bytes belong to the block it names, which is what
    /// [`Volume::resolve_link`](crate::Volume::resolve_link) is for, and
    /// silently following it here would write through a name the caller
    /// did not give.
    NotAFile {
        /// The block.
        lba: u64,
        /// Its secondary type.
        found: i32,
    },
    /// A file grown past what longword −47 can record. `byte_size` is a
    /// 32-bit field, so 4 GB − 1 is the format's ceiling and not this
    /// crate's.
    FileTooLarge {
        /// The length asked for.
        size: u64,
        /// The largest the format can record.
        max: u64,
    },
}

impl<E: fmt::Display> fmt::Display for MutateError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "block write failed: {e}"),
            Self::Read(e) => write!(f, "{e}"),
            Self::Alloc(e) => write!(f, "{e}"),
            Self::NameEmpty => f.write_str("an entry name is required"),
            Self::NameTooLong { len, max } => {
                write!(f, "name of {len} bytes exceeds this variant's {max}")
            }
            Self::NameInvalidByte { byte, index } => write!(
                f,
                "byte {byte:#04x} at index {index} cannot appear in a name"
            ),
            Self::CommentTooLong { len, max } => {
                write!(f, "comment of {len} bytes exceeds {max}")
            }
            Self::NotADirectory { lba, found } => {
                write!(f, "block {lba} (secondary type {found}) is not a directory")
            }
            Self::NotFound { parent, name } => write!(
                f,
                "directory {parent} holds no entry named {:?}",
                Latin1(name)
            ),
            Self::DuplicateName { parent, name } => write!(
                f,
                "directory {parent} already holds a name folding to {:?}",
                Latin1(name)
            ),
            Self::DirectoryNotEmpty { lba } => {
                write!(f, "directory {lba} is not empty")
            }
            Self::LinkedTo { lba, link } => write!(
                f,
                "block {lba} is the target of hard link {link}; deleting it would leave the \
                 link pointing at nothing"
            ),
            Self::IntoOwnSubtree { entry, parent } => write!(
                f,
                "directory {entry} cannot be moved into block {parent}, which is inside it"
            ),
            Self::IsRoot { lba } => {
                write!(f, "block {lba} is the volume root, not a directory entry")
            }
            Self::NotInChain { dir, lba } => write!(
                f,
                "block {lba} is not in the hash chain of directory {dir} its name hashes to"
            ),
            Self::NotInLinkChain { lba, target } => write!(
                f,
                "hard link {lba} is not in the link chain of the object {target} it names"
            ),
            Self::NotAFile { lba, found } => write!(
                f,
                "block {lba} (secondary type {found}) is not a file, so it has no contents to write"
            ),
            Self::FileTooLarge { size, max } => write!(
                f,
                "a file of {size} bytes cannot be recorded: the format's byte_size field stops at \
                 {max}"
            ),
        }
    }
}

#[cfg(feature = "std")]
impl<E: std::error::Error + 'static> std::error::Error for MutateError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Read(e) => Some(e),
            Self::Alloc(e) => Some(e),
            _ => None,
        }
    }
}

impl<E> From<crate::read::Error<E>> for MutateError<E> {
    fn from(e: crate::read::Error<E>) -> Self {
        Self::Read(e)
    }
}

impl<E> From<AllocError<E>> for MutateError<E> {
    fn from(e: AllocError<E>) -> Self {
        match e {
            AllocError::Read(e) => Self::Read(e),
            other => Self::Alloc(other),
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

// ---------------------------------------------------------------------------
// Metadata updates
// ---------------------------------------------------------------------------

/// Which of an entry's four metadata fields to change.
///
/// Every field is optional and `None` means "leave it alone", which is the
/// difference between this and [`Metadata`]: the latter describes an entry
/// being created, where every field has a value whether the caller thought
/// about it or not, and this describes a change to one that exists, where
/// "the caller did not mention the comment" and "the caller wants no
/// comment" are different instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetaUpdate<'a> {
    /// The full 32-bit protection longword, group and other bits
    /// included.
    pub protection: Option<u32>,
    /// The comment, raw Latin-1, at most [`COMMENT_MAX`] bytes. An empty
    /// slice removes the comment — and, on LNFS, frees the overflow block
    /// that was holding it.
    pub comment: Option<&'a [u8]>,
    /// The entry's DateStamp.
    pub date: Option<DateStamp>,
    /// The raw owner longword: UID in the high word, GID in the low.
    pub owner: Option<u32>,
}

impl<'a> MetaUpdate<'a> {
    /// Change nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the protection longword.
    pub fn protection(mut self, protection: u32) -> Self {
        self.protection = Some(protection);
        self
    }

    /// Set the comment; an empty slice removes it.
    pub fn comment(mut self, comment: &'a [u8]) -> Self {
        self.comment = Some(comment);
        self
    }

    /// Set the DateStamp.
    pub fn date(mut self, date: DateStamp) -> Self {
        self.date = Some(date);
        self
    }

    /// Set the owner longword.
    pub fn owner(mut self, owner: u32) -> Self {
        self.owner = Some(owner);
        self
    }
}

// ---------------------------------------------------------------------------
// The mutator
// ---------------------------------------------------------------------------

/// A volume open for change: the [`Volume`] and an [`Allocator`] over its
/// bitmap.
///
/// The pair is the whole type. Holding the allocator across operations is
/// the only reason this is a session rather than a set of methods on
/// [`Volume`]: reading a 2 GB volume's bitmap costs a hundred block reads
/// and doing it once per created file would dominate everything else.
/// Nothing else is cached — every operation re-reads the blocks it is
/// about to change, because the volume is the truth and a second copy of
/// it here would be a second thing to keep in step.
///
/// **Every public operation leaves the volume consistent**, bitmap
/// flushed and (on `DOS\6`/`DOS\7`) `NumBlocksUsed` stamped, so there is
/// no `finish()` to forget: [`Mutator::into_volume`] simply hands the
/// volume back. That is the opposite of [`Populator`](crate::Populator),
/// which deliberately marks the volume mid-update for its whole session —
/// and it is affordable here precisely because this module never lets the
/// bitmap say "free" about a block something reaches.
pub struct Mutator<S: BlockMedium> {
    pub(crate) vol: Volume<S>,
    pub(crate) alloc: Allocator<Transport<S>>,
    clock: Option<DateStamp>,
    layout_policy: bool,
}

impl<S: BlockMedium> Mutator<S> {
    /// Open a volume for mutation, reading its bitmap.
    ///
    /// Refuses a volume whose `bitmap_flag` is 0 — see this module's
    /// documentation, and [`Allocator::load`], which is where the refusal
    /// actually lives.
    ///
    /// The block-layout policy (this module's "Block layout policy:
    /// passive reorganisation" section) starts on; [`Mutator::layout_policy`]
    /// turns it off.
    pub fn open(mut vol: Volume<S>) -> Result<Self, MutateError<Transport<S>>> {
        let alloc = Allocator::load(&mut vol)?;
        Ok(Self {
            vol,
            alloc,
            clock: None,
            layout_policy: true,
        })
    }

    /// Set the session's notion of "now".
    ///
    /// With one set, every operation stamps the affected directory's
    /// DateStamp and the root's `disk_altered`/`dir_altered`, the way a
    /// real filesystem does. Without one — the default — those dates are
    /// left untouched, because a `no_std` crate has no clock and a made-up
    /// date on disk is worse than an old true one.
    pub fn clock(mut self, now: DateStamp) -> Self {
        self.clock = Some(now);
        self
    }

    /// Turn the block-layout policy (this module's "Block layout policy:
    /// passive reorganisation" section) on or off. On by default.
    ///
    /// The policy only changes *where* a write that was already going to
    /// allocate blocks puts them — it never changes what gets written, so
    /// there is no compatibility risk in leaving it on. Turn it off for a
    /// caller that wants this session's allocation sequence to match the
    /// pre-wave-3 one exactly: a differential comparison against a tool
    /// that predicts LBAs, or a test asserting an exact placement as a
    /// proxy for something else it actually means.
    pub fn layout_policy(mut self, enabled: bool) -> Self {
        self.layout_policy = enabled;
        self
    }

    /// The volume, for reading: lookups, listings, file contents.
    pub fn volume(&mut self) -> &mut Volume<S> {
        &mut self.vol
    }

    /// The allocator, for its accounting.
    pub fn allocator(&self) -> &Allocator<Transport<S>> {
        &self.alloc
    }

    /// Hand the volume back.
    ///
    /// No flush is needed and none is done: every operation already
    /// finished by flushing the bitmap pages it dirtied.
    pub fn into_volume(self) -> Volume<S> {
        self.vol
    }

    /// The longest name this volume's variant can store.
    pub fn max_name_len(&self) -> usize {
        if self.vol.variant().has_long_names() {
            MAX_NAME_LONG
        } else {
            MAX_NAME_CLASSIC
        }
    }

    // -- creating ----------------------------------------------------------

    /// Create a directory in `parent`.
    ///
    /// Write order: the new directory's own (empty) `T_DIRCACHE` block
    /// where the variant has them — allocated, its bitmap page flushed,
    /// and written before the header that names it — then the header
    /// block complete with its hash chain pointing at the slot's old head,
    /// then the one longword in `parent` that puts it in the directory.
    /// Everything before that last write is unreachable, so an
    /// interruption leaks and nothing more. The parent's dircache is
    /// regenerated afterwards, because a cache record naming an entry that
    /// is not in the chains is worse than one missing an entry that is.
    pub fn create_dir(
        &mut self,
        parent: u64,
        name: &[u8],
        meta: &Metadata<'_>,
    ) -> Result<u64, MutateError<Transport<S>>> {
        let prep = self.prepare(parent, name, meta)?;
        let bs = self.bs();
        let mut hdr = vec![0u8; bs];

        // A directory on DOS\4/DOS\5 has a cache block from birth: an
        // empty cache is not a null pointer, and the oracle's directories
        // have one too.
        if self.vol.variant().has_dircache() {
            // Near the root, not next to the directory's own header --
            // wave 1's finding (`docs/layout-survey.md` §4a's addendum,
            // `Intent::MetadataNearRoot`'s own documentation): a dircache
            // block is touched during a walk, not at open, so its
            // locality wants the walk's one anchor.
            let dc = if self.layout_policy {
                self.alloc.allocate_for(Intent::MetadataNearRoot {
                    root_lba: self.vol.root_lba(),
                })?
            } else {
                self.alloc.allocate_near(prep.lba)?
            };
            self.flush()?;
            let dc = self.alloc.reference(&dc)?;
            let mut buf = vec![0u8; bs];
            build_dircache_block(&mut buf, dc, prep.lba, &[], 0, 0);
            self.write(dc, &buf)?;
            wr32(&mut hdr, tail(bs, TL_EXTENSION), dc as u32);
        }

        self.emit(&prep, EntryKind::Directory, 0, &mut hdr)
    }

    /// Create a file in `parent` from bytes already in memory.
    ///
    /// Write order: every block the file needs — header, data blocks,
    /// `T_LIST` extension blocks, an LNFS overflow comment block — is
    /// allocated *first* and the bitmap flushed once, so every pointer
    /// written afterwards names a block the disk already agrees is taken
    /// ([`Allocator::reference`] refuses otherwise). Then the data blocks,
    /// then the extension blocks that point at them, then the header that
    /// points at those, then the parent's hash slot. Each write only ever
    /// names blocks already on the disk.
    ///
    /// The whole file is held by the caller rather than streamed.
    /// [`Populator::create_file_with`](crate::Populator::create_file_with)
    /// is the streaming form for a volume being built from nothing; here,
    /// a caller with more bytes than memory creates the file empty and
    /// [`appends`](Mutator::append), which costs one rebuild of the
    /// extension chain per call and is why the argument is a slice.
    pub fn create_file(
        &mut self,
        parent: u64,
        name: &[u8],
        meta: &Metadata<'_>,
        data: &[u8],
    ) -> Result<u64, MutateError<Transport<S>>> {
        let prep = self.prepare(parent, name, meta)?;
        let bs = self.bs();
        let ffs = self.vol.variant().is_ffs();
        let payload = data_payload_size(bs, ffs);
        let slots = hash_table_size(bs) as usize;
        let n_data = div_ceil(data.len() as u64, payload as u64) as usize;
        // The header's own table holds the first `slots` pointers; every
        // extension block another `slots`.
        let n_ext = n_data.saturating_sub(slots);
        let n_ext = n_ext / slots + usize::from(n_ext % slots != 0);

        // Everything up front, then one flush: after this point every
        // block number below is durable, so `reference` never refuses and
        // no pointer can name a block the bitmap still calls free.
        let mut data_at: Vec<Allocation> = Vec::with_capacity(n_data);
        let mut ext_at: Vec<Allocation> = Vec::with_capacity(n_ext);
        if self.layout_policy {
            // Born as one run whenever a run exists: the interleaved
            // fetch-order positions (data blocks with `T_LIST` extension
            // blocks spliced in exactly where a streaming reader meets
            // them, `docs/layout-survey.md` §4a's wave-1 addendum), filled
            // from one `allocate_run` near the header instead of one block
            // at a time — a stronger guarantee than the per-block hint
            // loop below ever gave, which was merely usually contiguous.
            let positions = interleaved_positions(n_data, n_ext, slots);
            let total = positions.len() as u64;
            let mut got = 0u64;
            let mut hint = prep.lba;
            let mut allocs: Vec<Allocation> = Vec::with_capacity(positions.len());
            while got < total {
                let run = self.alloc.allocate_run_hinted(
                    total - got,
                    Intent::DataFor {
                        header_lba: prep.lba,
                    },
                    hint,
                )?;
                hint = run.last().map(|a| a.block() + 1).unwrap_or(hint);
                got += run.len() as u64;
                allocs.extend(run);
            }
            for (is_ext, a) in positions.into_iter().zip(allocs) {
                if is_ext {
                    ext_at.push(a);
                } else {
                    data_at.push(a);
                }
            }
        } else {
            let mut hint = prep.lba;
            for i in 0..n_data {
                if i >= slots && (i - slots) % slots == 0 {
                    let a = self.alloc.allocate_near(hint)?;
                    hint = a.block();
                    ext_at.push(a);
                }
                let a = self.alloc.allocate_near(hint)?;
                hint = a.block();
                data_at.push(a);
            }
        }
        self.flush()?;

        let mut data_lba = Vec::with_capacity(n_data);
        for a in &data_at {
            data_lba.push(self.alloc.reference(a)?);
        }
        let mut ext_lba = Vec::with_capacity(n_ext);
        for a in &ext_at {
            ext_lba.push(self.alloc.reference(a)?);
        }

        let mut buf = vec![0u8; bs];
        for (i, &lba) in data_lba.iter().enumerate() {
            let from = i * payload;
            let to = (from + payload).min(data.len());
            let next = data_lba.get(i + 1).copied().unwrap_or(0) as u32;
            build_data_block(&mut buf, prep.lba, i as u32 + 1, &data[from..to], next, ffs);
            self.write(lba, &buf)?;
        }

        let mut hdr = vec![0u8; bs];
        for (k, &lba) in ext_lba.iter().enumerate() {
            let first = slots + k * slots;
            let count = (n_data - first).min(slots);
            let mut eb = vec![0u8; bs];
            wr32(&mut eb, OFF_TYPE, T_LIST);
            wr32(&mut eb, OFF_OWN_KEY, lba as u32);
            wr32(&mut eb, OFF_HIGH_SEQ, count as u32);
            wr32(&mut eb, tail(bs, TL_PARENT), prep.lba as u32);
            wr32(&mut eb, tail(bs, TL_SECONDARY_TYPE), ST_FILE as u32);
            wr32(
                &mut eb,
                tail(bs, TL_EXTENSION),
                ext_lba.get(k + 1).copied().unwrap_or(0) as u32,
            );
            for i in 0..count {
                let off = data_pointer_offset(bs, i as u32 + 1);
                wr32(&mut eb, off, data_lba[first + i] as u32);
            }
            finish_checksum(&mut eb);
            self.write(lba, &eb)?;
        }

        let in_header = n_data.min(slots);
        wr32(&mut hdr, OFF_HIGH_SEQ, in_header as u32);
        wr32(
            &mut hdr,
            OFF_FIRST_DATA,
            data_lba.first().copied().unwrap_or(0) as u32,
        );
        for (i, &lba) in data_lba.iter().take(in_header).enumerate() {
            wr32(&mut hdr, data_pointer_offset(bs, i as u32 + 1), lba as u32);
        }
        wr32(
            &mut hdr,
            tail(bs, TL_EXTENSION),
            ext_lba.first().copied().unwrap_or(0) as u32,
        );

        self.emit(&prep, EntryKind::File, data.len() as u32, &mut hdr)
    }

    // -- file contents -----------------------------------------------------

    /// Write `data` into an existing file at `offset`, extending it if the
    /// range runs past the end.
    ///
    /// Returns the file's length afterwards, which is
    /// `max(byte_size, offset + data.len())` — a write wholly inside the
    /// file does not shorten it, and one that runs past the end grows it.
    ///
    /// Blocks the range covers entirely are overwritten in place; a
    /// partial first or last block is read, patched and written back. See
    /// this module's documentation for the write order, for why the header
    /// block is the single commit, and for the one caveat this operation
    /// carries — an interrupted overwrite leaves old bytes or new bytes,
    /// per block, and nothing structural.
    ///
    /// An `offset` past the current end of file is legal and **zeroes the
    /// gap**, allocating every block of it: the format has no
    /// representation for a hole, `Seek()` on AmigaDOS refuses to position
    /// past the end at all, and `SetFileSize()` — the only way to get
    /// there — leaves the extended region explicitly undefined. Zero is
    /// the answer that does not hand a deleted file's bytes to a new one.
    pub fn write_file(
        &mut self,
        parent: u64,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<u64, MutateError<Transport<S>>> {
        let entry = self.expect_file(parent, name)?;
        let end = offset.saturating_add(data.len() as u64);
        let new_size = end.max(entry.byte_size as u64);
        self.edit_file(&entry, offset, data, new_size)
    }

    /// Append `data` to a file: the common case, and sugar for
    /// [`Mutator::write_file`] at the current end of file.
    ///
    /// Deliberately sugar rather than a second implementation. "Where does
    /// the file end" is one question with one answer — longword −47 — and
    /// an append that computed the position itself would be a second place
    /// to get the last block's short length wrong.
    pub fn append(
        &mut self,
        parent: u64,
        name: &[u8],
        data: &[u8],
    ) -> Result<u64, MutateError<Transport<S>>> {
        let entry = self.expect_file(parent, name)?;
        let at = entry.byte_size as u64;
        self.edit_file(&entry, at, data, at.saturating_add(data.len() as u64))
    }

    /// Set a file's length, in either direction — AmigaDOS's
    /// `SetFileSize`.
    ///
    /// **Shrinking** frees the data blocks and extension blocks past the
    /// cut, and does it in that order: the header block is rewritten with
    /// the new `byte_size`, `high_seq` and table — and, on OFS, the block
    /// that is now last is replaced by one whose recorded length is the
    /// short one — and only once all of that is on the disk is a single
    /// bit cleared. A crash before the header write leaves the file as it
    /// was; a crash after it leaks the blocks past the cut, which
    /// [`repair`](crate::repair) reclaims. The other order would leave the
    /// bitmap calling a block free while the file still pointed at it.
    ///
    /// **Growing** allocates blocks and zero-fills them, for the reasons
    /// given in this module's documentation — the format cannot express a
    /// hole, and the specification's "no values should be assumed" leaves
    /// the choice open, so this takes the one that cannot leak a deleted
    /// file's contents.
    ///
    /// On FFS a shrink is `byte_size` arithmetic and a table: there is no
    /// per-block length anywhere to correct. On OFS there is exactly one,
    /// in the new last block, and it is the reason that block is rewritten
    /// rather than left alone.
    pub fn truncate(
        &mut self,
        parent: u64,
        name: &[u8],
        new_size: u64,
    ) -> Result<(), MutateError<Transport<S>>> {
        let entry = self.expect_file(parent, name)?;
        // Offset at the new end with nothing to write: everything between
        // the old end and there — if there is anything — is the gap, and
        // the gap is zeroes.
        self.edit_file(&entry, new_size, &[], new_size)?;
        Ok(())
    }

    /// The one implementation behind write, append and truncate.
    ///
    /// `data` lands at `offset`; the file ends at `new_size`; anything
    /// between the old end and `new_size` that `data` does not cover is
    /// zeroes. Every caller above is a way of choosing those three
    /// numbers, which is the point — the block arithmetic, the ordering
    /// and the OFS header bookkeeping exist once.
    fn edit_file(
        &mut self,
        entry: &Entry,
        offset: u64,
        data: &[u8],
        new_size: u64,
    ) -> Result<u64, MutateError<Transport<S>>> {
        if new_size > u32::MAX as u64 {
            return Err(MutateError::FileTooLarge {
                size: new_size,
                max: u32::MAX as u64,
            });
        }
        let bs = self.bs();
        let ffs = self.vol.variant().is_ffs();
        let payload = data_payload_size(bs, ffs) as u64;
        let slots = hash_table_size(bs) as usize;
        let header = entry.lba;

        let chain = self.vol.file_chain(header)?;
        let old_size = chain.byte_size as u64;
        let n_old = chain.blocks.len();
        let n_new = div_ceil(new_size, payload) as usize;

        // The copy-out-copy-back shape: a write that replaces the file's
        // entire visible content (`offset == 0`, and `data` reaches at
        // least as far as the old end, so nothing of the old file
        // survives unread). This is deliberately narrower than "big
        // write" — an append, or a write into the middle, never qualifies
        // no matter its size, because passive means the caller's own
        // operation dictates the work; this only changes *where* a write
        // that was already discarding the whole old chain puts the
        // replacement. See this module's "Block layout policy" section:
        // this is the PFS2DefragTry pattern closed through the one writer
        // that can choose placement instead of hoping the allocator will.
        let full_rewrite = self.layout_policy && offset == 0 && data.len() as u64 >= old_size;
        // For every decision below about which *positions* reuse an old
        // block in place, a full rewrite treats the old chain as if it
        // had nothing to reuse — every position is fresh. The real
        // `n_old` is still what the final free pass and the OFS
        // read-modify-write path (guarded by `!untouchable`, which a full
        // rewrite never reaches) need, so both names stay in scope.
        let eff_n_old = if full_rewrite { 0 } else { n_old };
        let kept = eff_n_old.min(n_new);

        // How many bytes of block `i` the file uses at a given length.
        let len_at = |size: u64, i: usize| -> usize {
            size.saturating_sub(i as u64 * payload).min(payload) as usize
        };

        // At most one *kept* block changes its recorded length: the last
        // one either file has in common. Every earlier kept block is full
        // in both, and everything past `kept` is new or gone.
        let relength = (kept > 0 && len_at(old_size, kept - 1) != len_at(new_size, kept - 1))
            .then(|| kept - 1);
        // Only OFS records that length, so only OFS has to replace the
        // block; on FFS the same bytes go back where they were.
        let cow_index = if ffs { None } else { relength };

        // Whether the extension chain's *contents* change. The header's
        // own table holds the first `slots` pointers, so a file that stays
        // inside it never touches the chain at all. A full rewrite always
        // rebuilds it: every data pointer in it is about to be fresh.
        let n_ext_new = ext_block_count(n_new, slots);
        let ext_dirty = full_rewrite
            || (n_new != eff_n_old && (n_new > slots || eff_n_old > slots))
            || matches!(cow_index, Some(i) if i >= slots);

        // Everything up front, then one flush: after it every block number
        // below is durable, so `reference` never refuses.
        let mut fresh: Vec<Allocation> = Vec::with_capacity(n_new.saturating_sub(eff_n_old));
        let mut cow_at: Option<Allocation> = None;
        let mut ext_at: Vec<Allocation> = Vec::new();
        if full_rewrite {
            // One contiguous run, interleaved with the extension chain at
            // its natural fetch-order position — `Mutator::create_file`'s
            // own reasoning, applied here to a replacement instead of a
            // birth. `allocate_run`'s own documented fallback (the
            // longest run available, called again for the remainder)
            // means a badly fragmented volume still finishes the write,
            // just not in one piece. `cow_index` is always `None` here —
            // `kept` is 0, since `eff_n_old` is — so there is no
            // copy-on-write block to interleave in as well.
            let positions = interleaved_positions(n_new, n_ext_new, slots);
            let total = positions.len() as u64;
            let mut got = 0u64;
            let mut hint = header;
            let mut allocs: Vec<Allocation> = Vec::with_capacity(positions.len());
            while got < total {
                let run = self.alloc.allocate_run_hinted(
                    total - got,
                    Intent::DataFor { header_lba: header },
                    hint,
                )?;
                hint = run.last().map(|a| a.block() + 1).unwrap_or(hint);
                got += run.len() as u64;
                allocs.extend(run);
            }
            for (is_ext, a) in positions.into_iter().zip(allocs) {
                if is_ext {
                    ext_at.push(a);
                } else {
                    fresh.push(a);
                }
            }
        } else {
            // The pre-wave-3 shape, unchanged: growth extends near the
            // file's own last block (`Intent::DataFor`'s own hint,
            // already what this hint was before it had a name), then the
            // copy-on-write block, then a rebuilt extension chain, one
            // hint threaded through all three so each lands next to the
            // last thing this call allocated.
            let mut hint = chain.blocks.last().map(|&b| b as u64).unwrap_or(entry.lba);
            for _ in eff_n_old..n_new {
                let a = self.alloc.allocate_near(hint)?;
                hint = a.block();
                fresh.push(a);
            }
            if cow_index.is_some() {
                let a = self.alloc.allocate_near(hint)?;
                hint = a.block();
                cow_at = Some(a);
            }
            if ext_dirty {
                for _ in 0..n_ext_new {
                    let a = self.alloc.allocate_near(hint)?;
                    hint = a.block();
                    ext_at.push(a);
                }
            }
        }
        self.flush()?;

        let mut fresh_lba = Vec::with_capacity(fresh.len());
        for a in &fresh {
            fresh_lba.push(self.alloc.reference(a)?);
        }
        let cow_lba = match &cow_at {
            Some(a) => Some(self.alloc.reference(a)?),
            None => None,
        };
        let mut ext_lba = Vec::with_capacity(ext_at.len());
        for a in &ext_at {
            ext_lba.push(self.alloc.reference(a)?);
        }

        // The file's blocks as they will be: kept, replaced, or new.
        let mut blocks: Vec<u64> = Vec::with_capacity(n_new);
        for i in 0..n_new {
            if i >= eff_n_old {
                blocks.push(fresh_lba[i - eff_n_old]);
            } else if Some(i) == cow_index {
                blocks.push(cow_lba.expect("a cow index implies a cow block"));
            } else {
                blocks.push(chain.blocks[i] as u64);
            }
        }

        let write_lo = offset;
        let write_hi = offset + data.len() as u64;
        // The grow region: everything between the old end and the new one,
        // which `data` may or may not cover and the rest of which is
        // zeroes.
        let grow_lo = old_size.min(new_size);

        let mut buf = vec![0u8; bs];
        // Two passes, in this order: blocks nothing reaches first, blocks
        // the old file already reaches second. The first pass is free —
        // an interruption in it leaks. The second is the documented
        // in-place caveat.
        for pass in 0..2 {
            for i in 0..n_new {
                let untouchable = i >= eff_n_old || Some(i) == cow_index;
                if untouchable != (pass == 0) {
                    continue;
                }
                let start = i as u64 * payload;
                let len = len_at(new_size, i);
                let next = blocks.get(i + 1).copied().unwrap_or(0) as u32;
                if !untouchable {
                    // A kept block is rewritten only if something in it
                    // actually differs: its bytes, or — on OFS, where it
                    // is recorded — which block follows it.
                    let old_next = chain.blocks.get(i + 1).copied().unwrap_or(0);
                    let bytes_change = overlaps(start, len, write_lo, write_hi)
                        || overlaps(start, len, grow_lo, new_size);
                    let next_changes = !ffs && old_next as u64 != next as u64;
                    if !bytes_change && !next_changes {
                        continue;
                    }
                }

                // Whatever the old file had here survives, unless the
                // write covers it: read-modify-write, through the ranged
                // read so an OFS block is verified on the way in rather
                // than trusted.
                let mut payload_buf = vec![0u8; len];
                let keep = len_at(old_size, i).min(len);
                if keep > 0 && i < eff_n_old {
                    let got = self
                        .vol
                        .read_range(&chain, start, &mut payload_buf[..keep])?;
                    debug_assert_eq!(got, keep);
                }
                let lo = start.max(write_lo);
                let hi = (start + len as u64).min(write_hi);
                if lo < hi {
                    let n = (hi - lo) as usize;
                    let from = (lo - write_lo) as usize;
                    let to = (lo - start) as usize;
                    payload_buf[to..to + n].copy_from_slice(&data[from..from + n]);
                }

                build_data_block(&mut buf, header, i as u32 + 1, &payload_buf, next, ffs);
                self.write(blocks[i], &buf)?;
            }
        }

        // The fresh extension chain, written backwards so a `next` never
        // names a block that is not on the disk yet. Nothing points at its
        // first block until the header does.
        for (k, &lba) in ext_lba.iter().enumerate().rev() {
            let first = slots + k * slots;
            let count = (n_new - first).min(slots);
            let mut eb = vec![0u8; bs];
            wr32(&mut eb, OFF_TYPE, T_LIST);
            wr32(&mut eb, OFF_OWN_KEY, lba as u32);
            wr32(&mut eb, OFF_HIGH_SEQ, count as u32);
            wr32(&mut eb, tail(bs, TL_PARENT), header as u32);
            wr32(&mut eb, tail(bs, TL_SECONDARY_TYPE), ST_FILE as u32);
            wr32(
                &mut eb,
                tail(bs, TL_EXTENSION),
                ext_lba.get(k + 1).copied().unwrap_or(0) as u32,
            );
            for i in 0..count {
                wr32(
                    &mut eb,
                    data_pointer_offset(bs, i as u32 + 1),
                    blocks[first + i] as u32,
                );
            }
            finish_checksum(&mut eb);
            self.write(lba, &eb)?;
        }

        // The commit: one write, carrying the length, the count, the whole
        // table and the extension pointer. Before it the file is entirely
        // the old one; after it, entirely the new one.
        let in_header = n_new.min(slots);
        let mut hdr = self.get(header)?;
        for b in hdr[OFF_HASH_TABLE..OFF_HASH_TABLE + slots * 4].iter_mut() {
            *b = 0;
        }
        wr32(&mut hdr, OFF_HIGH_SEQ, in_header as u32);
        wr32(
            &mut hdr,
            OFF_FIRST_DATA,
            blocks.first().copied().unwrap_or(0) as u32,
        );
        for (i, &lba) in blocks.iter().take(in_header).enumerate() {
            wr32(&mut hdr, data_pointer_offset(bs, i as u32 + 1), lba as u32);
        }
        let first_ext = if ext_dirty {
            ext_lba.first().copied().unwrap_or(0)
        } else {
            chain.extensions.first().copied().unwrap_or(0) as u64
        };
        wr32(&mut hdr, tail(bs, TL_EXTENSION), first_ext as u32);
        wr32(&mut hdr, tail(bs, TL_BYTE_SIZE), new_size as u32);
        if let Some(now) = self.clock {
            write_date(&mut hdr, self.vol.variant(), now);
        }
        self.put(header, &mut hdr)?;

        // Advisory records after the authoritative one, then the frees:
        // a bit is cleared only once nothing on the disk names the block.
        self.refresh_dircache(entry.parent as u64)?;
        self.touch_disk()?;
        // A full rewrite reused none of the old data blocks (`eff_n_old`
        // was 0 throughout), so every one of them — not just the ones
        // past the new length — is now unreachable.
        let free_from = if full_rewrite { 0 } else { n_new };
        for i in free_from..n_old {
            self.alloc.free(chain.blocks[i] as u64)?;
        }
        if let Some(i) = cow_index {
            self.alloc.free(chain.blocks[i] as u64)?;
        }
        if ext_dirty {
            for &e in &chain.extensions {
                self.alloc.free(e as u64)?;
            }
        }
        self.flush()?;
        self.stamp_blocks_used()?;
        Ok(new_size)
    }

    // -- deleting ----------------------------------------------------------

    /// Delete `name` from `parent`, and return how many blocks came back.
    ///
    /// Write order, and the reason for it: the entry is spliced out of the
    /// **metadata** first — out of its target's link chain if it is a hard
    /// link, then out of its parent's hash chain, then the parent's
    /// dircache is regenerated — and only after all of that is a single
    /// bit cleared in the bitmap. A crash before the unlink changes
    /// nothing; a crash after it leaks the blocks, which
    /// [`repair`](crate::repair) reclaims. The other order would leave the
    /// bitmap calling a block free while a live directory still pointed at
    /// it, and the *next* allocation would then give one file's block to
    /// another.
    ///
    /// # What it refuses
    ///
    /// - A **directory with anything in it**
    ///   ([`MutateError::DirectoryNotEmpty`]): AmigaDOS refuses this too,
    ///   and deleting it here would leak the entire subtree.
    /// - An entry that is the **target of a hard link**
    ///   ([`MutateError::LinkedTo`]) — the interesting one, and the one
    ///   worth stating the evidence for, because the two implementations
    ///   that handle it at all *do not agree on what the volume looks
    ///   like afterwards*:
    ///
    ///   - **AmigaOS** promotes. Its own documentation is explicit: "if
    ///     the object a hard link points to is deleted, then the first
    ///     hard link in the chain is altered so that it becomes the new
    ///     file header block. The original file header block is then
    ///     freed." The object's header block therefore *changes number*,
    ///     which invalidates every cached block pointer to it — including
    ///     the `parent` longword in every child of a promoted directory.
    ///   - **Linux `affs`** does the opposite, and says why in a comment:
    ///     "we can't remove the head of the link, as its blocknr is still
    ///     used as ino, so we remove the block of the first link
    ///     instead" (`affs_remove_link`, `fs/affs/amigaffs.c`). It keeps
    ///     the original block, `memcpy`s the *link's* 32-byte name into
    ///     it, `affs_insert_hash`es it into the **link's** directory, and
    ///     frees the link's block. The file survives — under a different
    ///     name, in a different directory from the one it was deleted
    ///     from.
    ///   - **ADFlib** refuses links outright (`adfRemoveEntry`:
    ///     "secType %d not supported"), **amitools** has no hard-link
    ///     code at all, and **AROS's `afs.handler`** defines
    ///     `BLK_ORIGINAL`/`BLK_LINKCHAIN` and never reads them, so its
    ///     `deleteObject` leaves the links dangling.
    ///
    ///   Two shipping behaviours that produce different volumes, three
    ///   implementations that do neither, and either of the two requires
    ///   several blocks rewritten with no ordering that makes an
    ///   interruption harmless — a crash halfway through a promotion
    ///   leaves either two blocks claiming one data chain or a directory
    ///   whose children's `parent` longwords name a freed block. So this
    ///   wave refuses, in the type, and leaves the choice where it can
    ///   still be made: deleting the *links* first and then the target
    ///   works today and is exactly what the refusal points a caller at.
    ///   Dangling links, which are what a silent success would produce,
    ///   are the one outcome ruled out.
    /// - A **file whose chain does not read** — the error comes back from
    ///   [`Volume::file_chain`](crate::Volume::file_chain) as
    ///   [`MutateError::Read`]. Freeing a set of blocks this code could
    ///   not enumerate with confidence is the one unrecoverable mistake
    ///   available here, so a damaged file is repair's to sort out, not a
    ///   delete's to guess at.
    pub fn delete(&mut self, parent: u64, name: &[u8]) -> Result<u64, MutateError<Transport<S>>> {
        let entry = self.expect_entry(parent, name)?;
        let free = self.blocks_of(&entry)?;

        // A link is spliced out of its target's chain while it is still
        // reachable from the directory: the other order would leave the
        // target naming a block that is about to be freed.
        if matches!(entry.kind, EntryKind::LinkFile | EntryKind::LinkDir) {
            self.unlink_from_link_chain(&entry)?;
        } else if entry.next_link != 0 {
            return Err(MutateError::LinkedTo {
                lba: entry.lba,
                link: entry.next_link,
            });
        }

        self.unlink(parent, &entry)?;
        self.refresh_dircache(parent)?;
        self.touch(parent)?;

        for lba in &free {
            self.alloc.free(*lba)?;
        }
        self.flush()?;
        self.stamp_blocks_used()?;
        Ok(free.len() as u64)
    }

    // -- renaming ----------------------------------------------------------

    /// Move and/or rename an entry, within one directory or between two on
    /// the same volume.
    ///
    /// Write order: unlink from the old chain, rewrite the header (new
    /// name, new parent, new chain pointer), link into the new chain,
    /// regenerate both dircaches, then flush the bitmap if a comment block
    /// was allocated or freed. The entry is unreachable in the middle of
    /// that, which is the point: linking it into the new chain first would
    /// put one block in two chains with one `hash_chain` longword to serve
    /// both, and a crash there would leave a directory that enumerates a
    /// file twice.
    ///
    /// Renaming to the **same name in different case** works and is the
    /// operation FFS is unusual for supporting: the name hashes to the
    /// same slot under the volume's fold table, the duplicate check sees
    /// the entry is itself rather than a collision, and the stored bytes
    /// change while lookups keep matching. The new head of the target slot
    /// is deliberately read *after* the unlink, since for a same-slot
    /// rename the unlink is what changed it.
    ///
    /// On LNFS a rename can move the comment: the name and comment share
    /// one 112-byte field, so a longer name pushes the comment out into a
    /// [`T_COMMENT`] block (allocated, flushed and written before the
    /// header points at it) and a shorter one pulls it back inline (the
    /// block freed *after* the header stopped pointing at it).
    ///
    /// # What it refuses
    ///
    /// A name already in the target directory, the root, and a directory
    /// moved into its own subtree — the last checked by walking `parent`
    /// pointers upward from the proposed new parent with a cycle guard,
    /// because the volume may already contain the loop this is trying not
    /// to create.
    pub fn rename(
        &mut self,
        parent: u64,
        name: &[u8],
        new_parent: u64,
        new_name: &[u8],
    ) -> Result<(), MutateError<Transport<S>>> {
        let entry = self.expect_entry(parent, name)?;
        self.check_name(new_name)?;
        self.expect_directory(new_parent)?;
        if let Some(clash) = self.vol.lookup(new_parent, new_name)? {
            if clash.lba != entry.lba {
                return Err(MutateError::DuplicateName {
                    parent: new_parent,
                    name: new_name.to_vec(),
                });
            }
        }
        if entry.kind.is_directory() {
            self.refuse_own_subtree(&entry, new_parent)?;
        }

        let bs = self.bs();
        let variant = self.vol.variant();
        let comment = self.vol.comment(&entry)?;

        // Where the comment has to live under the *new* name.
        let want_block = needs_comment_block(variant, new_name.len(), comment.len());
        let mut drop_block = 0u32;
        let mut comment_block = entry.comment_block;
        if want_block && entry.comment_block == 0 {
            let a = self.alloc.allocate_near(entry.lba)?;
            self.flush()?;
            let cb = self.alloc.reference(&a)?;
            let mut buf = vec![0u8; bs];
            build_comment_block(&mut buf, cb, entry.lba, &comment);
            self.write(cb, &buf)?;
            comment_block = cb as u32;
        } else if !want_block && entry.comment_block != 0 {
            drop_block = entry.comment_block;
            comment_block = 0;
        }

        self.unlink(parent, &entry)?;

        // After the unlink, because for a rename within one slot the
        // unlink is exactly what changed this slot's head.
        let slot = self.slot_of(new_name);
        let head = self.vol.hash_table(new_parent)?[slot];

        let mut hdr = self.get(entry.lba)?;
        write_name_and_comment(&mut hdr, variant, new_name, &comment, comment_block);
        wr32(&mut hdr, tail(bs, TL_HASH_CHAIN), head);
        wr32(&mut hdr, tail(bs, TL_PARENT), new_parent as u32);
        self.put(entry.lba, &mut hdr)?;

        let mut dir = self.get(new_parent)?;
        wr32(&mut dir, OFF_HASH_TABLE + slot * 4, entry.lba as u32);
        self.put(new_parent, &mut dir)?;

        self.refresh_dircache(parent)?;
        if new_parent != parent {
            self.refresh_dircache(new_parent)?;
        }
        self.touch(parent)?;
        if new_parent != parent {
            self.touch(new_parent)?;
        }

        if drop_block != 0 {
            self.alloc.free(drop_block as u64)?;
        }
        self.flush()?;
        self.stamp_blocks_used()?;
        Ok(())
    }

    // -- metadata ----------------------------------------------------------

    /// Change an entry's protection, comment, date or owner in place.
    ///
    /// One header write for all four fields, checksum recomputed — the
    /// block is rewritten whole, so there is no intermediate state in
    /// which some fields have changed and others have not.
    ///
    /// The comment is the only field that can cost a block. On the classic
    /// layout it has a field of its own and never does. On LNFS it shares
    /// the name's 112 bytes, so setting a long comment on an entry with a
    /// long name allocates a [`T_COMMENT`] block (bitmap flushed, block
    /// written, *then* the header points at it) and clearing it frees the
    /// block (header rewritten first, bit cleared after) — the same
    /// mark-then-use and unlink-then-free ordering as everything else
    /// here.
    ///
    /// The parent's dircache is regenerated afterwards, since a record
    /// caches protection, owner, date, size and comment as well as the
    /// name.
    pub fn set_metadata(
        &mut self,
        lba: u64,
        update: &MetaUpdate<'_>,
    ) -> Result<(), MutateError<Transport<S>>> {
        if lba == self.vol.root_lba() {
            return Err(MutateError::IsRoot { lba });
        }
        let entry = self.vol.entry_at(lba)?;
        let bs = self.bs();
        let variant = self.vol.variant();

        let existing = self.vol.comment(&entry)?;
        let comment: &[u8] = match update.comment {
            Some(c) => c,
            None => &existing,
        };
        if comment.len() > COMMENT_MAX {
            return Err(MutateError::CommentTooLong {
                len: comment.len(),
                max: COMMENT_MAX,
            });
        }

        let want_block = needs_comment_block(variant, entry.name.len(), comment.len());
        let mut comment_block = entry.comment_block;
        let mut drop_block = 0u32;
        if want_block {
            // An existing overflow block is reused: it already names this
            // header, and rewriting its text in place is one write either
            // way.
            let cb = if entry.comment_block != 0 {
                entry.comment_block as u64
            } else {
                let a = self.alloc.allocate_near(lba)?;
                self.flush()?;
                self.alloc.reference(&a)?
            };
            let mut buf = vec![0u8; bs];
            build_comment_block(&mut buf, cb, lba, comment);
            self.write(cb, &buf)?;
            comment_block = cb as u32;
        } else if entry.comment_block != 0 {
            drop_block = entry.comment_block;
            comment_block = 0;
        }

        let mut hdr = self.get(lba)?;
        if let Some(p) = update.protection {
            wr32(&mut hdr, tail(bs, TL_PROTECTION), p);
        }
        if let Some(o) = update.owner {
            wr32(&mut hdr, tail(bs, TL_OWNER), o);
        }
        if let Some(d) = update.date {
            write_date(&mut hdr, variant, d);
        }
        write_name_and_comment(&mut hdr, variant, &entry.name, comment, comment_block);
        self.put(lba, &mut hdr)?;

        if drop_block != 0 {
            self.alloc.free(drop_block as u64)?;
        }
        self.refresh_dircache(entry.parent as u64)?;
        self.flush()?;
        self.stamp_blocks_used()?;
        Ok(())
    }

    // -- shared machinery --------------------------------------------------

    pub(crate) fn bs(&self) -> usize {
        self.vol.block_size()
    }

    /// The hash slot a name lands in on this volume, under its own fold
    /// table.
    pub(crate) fn slot_of(&self, name: &[u8]) -> usize {
        let fold = self.vol.variant().fold();
        name_hash(name, fold, hash_table_size(self.bs())) as usize
    }

    fn check_name(&self, name: &[u8]) -> Result<(), MutateError<Transport<S>>> {
        check_name_bytes(name, self.max_name_len()).map_err(|p| match p {
            NameProblem::Empty => MutateError::NameEmpty,
            NameProblem::TooLong { len, max } => MutateError::NameTooLong { len, max },
            NameProblem::InvalidByte { byte, index } => {
                MutateError::NameInvalidByte { byte, index }
            }
        })
    }

    pub(crate) fn expect_directory(&mut self, lba: u64) -> Result<(), MutateError<Transport<S>>> {
        if lba == self.vol.root_lba() {
            return Ok(());
        }
        let entry = self.vol.entry_at(lba)?;
        if !entry.kind.is_directory() {
            return Err(MutateError::NotADirectory {
                lba,
                found: entry.kind.secondary_type(),
            });
        }
        Ok(())
    }

    /// The entry `name` in `parent`, refusing anything whose contents are
    /// not its own to write.
    fn expect_file(
        &mut self,
        parent: u64,
        name: &[u8],
    ) -> Result<Entry, MutateError<Transport<S>>> {
        let entry = self.expect_entry(parent, name)?;
        if entry.kind != EntryKind::File {
            return Err(MutateError::NotAFile {
                lba: entry.lba,
                found: entry.kind.secondary_type(),
            });
        }
        Ok(entry)
    }

    fn expect_entry(
        &mut self,
        parent: u64,
        name: &[u8],
    ) -> Result<Entry, MutateError<Transport<S>>> {
        self.expect_directory(parent)?;
        match self.vol.lookup(parent, name)? {
            Some(e) => Ok(e),
            None => Err(MutateError::NotFound {
                parent,
                name: name.to_vec(),
            }),
        }
    }

    /// Everything a create must settle before a block is allocated: the
    /// name and comment are legal, the parent is a directory, the name is
    /// not already there — and then the header block and, where the LNFS
    /// merged field cannot hold both, the comment block.
    fn prepare<'a>(
        &mut self,
        parent: u64,
        name: &'a [u8],
        meta: &'a Metadata<'a>,
    ) -> Result<Prepared<'a>, MutateError<Transport<S>>> {
        self.check_name(name)?;
        if meta.comment.len() > COMMENT_MAX {
            return Err(MutateError::CommentTooLong {
                len: meta.comment.len(),
                max: COMMENT_MAX,
            });
        }
        self.expect_directory(parent)?;
        if self.vol.lookup(parent, name)?.is_some() {
            return Err(MutateError::DuplicateName {
                parent,
                name: name.to_vec(),
            });
        }

        // A file or directory's own header, near the directory it is
        // filed in — `Intent::HeaderIn`'s own reasoning, and (`hint()`
        // returning `dir_lba` unchanged) exactly the hint this call used
        // before wave 3 gave it a name.
        let a = self
            .alloc
            .allocate_for(Intent::HeaderIn { dir_lba: parent })?;
        self.flush()?;
        let lba = self.alloc.reference(&a)?;

        let comment_block =
            if needs_comment_block(self.vol.variant(), name.len(), meta.comment.len()) {
                let cb = self.alloc.allocate_near(lba)?;
                self.flush()?;
                let cb = self.alloc.reference(&cb)?;
                let mut buf = vec![0u8; self.bs()];
                build_comment_block(&mut buf, cb, lba, meta.comment);
                self.write(cb, &buf)?;
                cb as u32
            } else {
                0
            };

        Ok(Prepared {
            lba,
            parent,
            comment_block,
            name,
            meta,
        })
    }

    /// Finish a create: write the header, chain it into its parent, then
    /// bring the advisory records up to date.
    ///
    /// The parent's hash slot is the last authoritative write, and the
    /// only one that changes what the directory contains.
    fn emit(
        &mut self,
        prep: &Prepared<'_>,
        kind: EntryKind,
        byte_size: u32,
        hdr: &mut [u8],
    ) -> Result<u64, MutateError<Transport<S>>> {
        let (lba, parent) = (prep.lba, prep.parent);
        let slot = self.slot_of(prep.name);
        // Head insertion, as the oracle does it: three names hashing to
        // one slot come back newest-first off a volume xdftool wrote.
        let head = self.vol.hash_table(parent)?[slot];
        write_entry_header(
            hdr,
            self.vol.variant(),
            &EntryFields {
                lba,
                parent,
                kind,
                byte_size,
                hash_chain: head,
                name: prep.name,
                meta: prep.meta,
                comment_block: prep.comment_block,
            },
        );
        self.write(lba, hdr)?;

        // The one authoritative write: until this longword lands, the
        // directory has not changed and everything above it is a leak.
        let mut dir = self.get(parent)?;
        wr32(&mut dir, OFF_HASH_TABLE + slot * 4, lba as u32);
        self.put(parent, &mut dir)?;

        self.refresh_dircache(parent)?;
        self.touch(parent)?;
        self.stamp_blocks_used()?;
        Ok(lba)
    }

    /// Every block an entry owns: its header, its overflow comment, and
    /// its contents — a file's extension and data blocks, a directory's
    /// dircache chain.
    ///
    /// This is the set [`Mutator::delete`] frees, so it is also where the
    /// non-empty-directory refusal lives: the check and the enumeration
    /// are the same walk, and separating them would let them disagree.
    pub(crate) fn blocks_of(
        &mut self,
        entry: &Entry,
    ) -> Result<Vec<u64>, MutateError<Transport<S>>> {
        let mut out = vec![entry.lba];
        if entry.comment_block != 0 {
            out.push(entry.comment_block as u64);
        }
        match entry.kind {
            EntryKind::Directory => {
                if self.vol.hash_table(entry.lba)?.iter().any(|&s| s != 0) {
                    return Err(MutateError::DirectoryNotEmpty { lba: entry.lba });
                }
                out.extend(self.vol.read_dircache(entry.lba)?.blocks);
            }
            EntryKind::File => {
                let chain = self.vol.file_chain(entry.lba)?;
                out.extend(chain.extensions.iter().map(|&e| e as u64));
                out.extend(chain.blocks.iter().map(|&b| b as u64));
            }
            // A link owns its header and nothing else — the data and the
            // hash table are the target's — and a soft link owns its
            // header and the path inside it.
            EntryKind::LinkFile | EntryKind::LinkDir | EntryKind::SoftLink => {}
        }
        Ok(out)
    }

    /// Splice an entry out of its parent's hash chain: the directory's
    /// own slot when it is the head, the previous entry's longword −4
    /// otherwise.
    ///
    /// The successor is re-read from the entry's block rather than taken
    /// from the caller's [`Entry`], so an entry that has been re-chained
    /// since it was looked up cannot be spliced with a stale pointer.
    pub(crate) fn unlink(
        &mut self,
        dir: u64,
        entry: &Entry,
    ) -> Result<(), MutateError<Transport<S>>> {
        let bs = self.bs();
        let block_count = self.vol.block_count();
        let slot = self.slot_of(&entry.name);
        let mut next = self.vol.hash_table(dir)?[slot];
        let mut prev = 0u64;
        let mut steps = 0u64;
        while next != 0 && next as u64 != entry.lba {
            steps += 1;
            if steps > block_count {
                return Err(MutateError::Read(crate::read::Error::ChainTooLong {
                    lba: next as u64,
                }));
            }
            let e = self.vol.entry_at(next as u64)?;
            prev = next as u64;
            next = e.hash_chain;
        }
        if next == 0 {
            return Err(MutateError::NotInChain {
                dir,
                lba: entry.lba,
            });
        }
        let successor = self.vol.entry_at(entry.lba)?.hash_chain;
        if prev == 0 {
            let mut buf = self.get(dir)?;
            wr32(&mut buf, OFF_HASH_TABLE + slot * 4, successor);
            self.put(dir, &mut buf)
        } else {
            let mut buf = self.get(prev)?;
            wr32(&mut buf, tail(bs, TL_HASH_CHAIN), successor);
            self.put(prev, &mut buf)
        }
    }

    /// Splice a hard link out of the chain of links naming its target:
    /// walk longword −10 from the target, and write the link's own −10
    /// into whichever block pointed at it.
    ///
    /// Done *before* the link leaves its directory, so at no point does a
    /// live object's link chain name a block that is on its way to being
    /// freed.
    fn unlink_from_link_chain(&mut self, link: &Entry) -> Result<(), MutateError<Transport<S>>> {
        let bs = self.bs();
        let block_count = self.vol.block_count();
        let target = link.real_entry as u64;
        if target == 0 {
            return Err(MutateError::Read(crate::read::Error::LinkTargetMissing {
                lba: link.lba,
            }));
        }
        let mut prev = target;
        let mut steps = 0u64;
        loop {
            let mut buf = self.get(prev)?;
            let next = be32(&buf, tail(bs, TL_NEXT_LINK));
            if next == 0 {
                return Err(MutateError::NotInLinkChain {
                    lba: link.lba,
                    target,
                });
            }
            if next as u64 == link.lba {
                wr32(&mut buf, tail(bs, TL_NEXT_LINK), link.next_link);
                return self.put(prev, &mut buf);
            }
            steps += 1;
            if steps > block_count {
                return Err(MutateError::Read(crate::read::Error::ChainTooLong {
                    lba: next as u64,
                }));
            }
            prev = next as u64;
        }
    }

    /// Refuse moving a directory into itself or into anything below it.
    ///
    /// Walks `parent` pointers upward from the proposed new parent, with
    /// a visited set *and* a length bound: the volume may already contain
    /// the loop this is trying not to create, and a cycle check that
    /// hangs on a damaged volume is not a cycle check.
    fn refuse_own_subtree(
        &mut self,
        entry: &Entry,
        new_parent: u64,
    ) -> Result<(), MutateError<Transport<S>>> {
        let root = self.vol.root_lba();
        let block_count = self.vol.block_count();
        let mut here = new_parent;
        let mut visited: Vec<u64> = Vec::new();
        loop {
            if here == entry.lba {
                return Err(MutateError::IntoOwnSubtree {
                    entry: entry.lba,
                    parent: new_parent,
                });
            }
            if here == root || here == 0 {
                return Ok(());
            }
            if visited.contains(&here) || visited.len() as u64 >= block_count {
                return Err(MutateError::Read(crate::read::Error::ChainCycle {
                    lba: here,
                }));
            }
            visited.push(here);
            here = self.vol.entry_at(here)?.parent as u64;
        }
    }

    // -- the dircache ------------------------------------------------------

    /// Rebuild a directory's whole dircache chain from its hash chains.
    ///
    /// The chains are the input, so the result cannot disagree with them
    /// — which is the entire argument for regenerating rather than
    /// patching, and why
    /// [`validate`](crate::Volume::validate) reports no
    /// [`DircacheStale`](crate::Finding::DircacheStale) finding after any
    /// operation in this module.
    ///
    /// Blocks are reused where the new chain is no longer than the old,
    /// allocated where it grew (bitmap flushed before the pointer naming
    /// them is written) and freed where it shrank (after the shortened
    /// chain is on the disk). The blocks are written **backwards**, last
    /// first, so every `next` pointer names a block that is already
    /// there.
    ///
    /// A no-op on the six variants without dircaches, so callers do not
    /// have to ask.
    pub(crate) fn refresh_dircache(&mut self, dir: u64) -> Result<(), MutateError<Transport<S>>> {
        if !self.vol.variant().has_dircache() {
            return Ok(());
        }
        let bs = self.bs();
        let capacity = bs - OFF_DIRCACHE_RECORDS_START;

        // The facts, from the authority.
        let entries = self.vol.read_dir(dir)?;
        let mut pages: Vec<(Vec<u8>, u32)> = Vec::new();
        let mut cur: Vec<u8> = Vec::new();
        let mut count = 0u32;
        for e in &entries {
            let comment = self.vol.comment(e)?;
            let record = dircache_record(&CacheFacts {
                entry: e.lba,
                byte_size: e.byte_size,
                protection: e.protection,
                owner: e.owner,
                date: e.date,
                kind: e.kind,
                name: &e.name,
                comment: &comment,
            });
            if cur.len() + record.len() > capacity {
                pages.push((core::mem::take(&mut cur), count));
                count = 0;
            }
            cur.extend_from_slice(&record);
            count += 1;
        }
        // Always at least one block, even for an empty directory: an
        // empty cache is not a null pointer, and every directory this
        // crate or the oracle creates has one from birth.
        pages.push((cur, count));

        let old = self.vol.read_dircache(dir)?.blocks;
        let mut blocks = old.clone();

        // Light-touch directory pull (this module's "Block layout policy"
        // section, item 3): every *kept* dircache block past the first
        // gets its content rewritten below regardless of whether the
        // chain grew or shrank, so trying to relocate one costs nothing
        // beyond the relocated block itself -- no extra read, since the
        // write that already had to happen just lands somewhere else, and
        // no extra write when the attempt is declined (a tentative
        // allocation that turns out not to help is freed again before it
        // is ever flushed, so it never reaches the disk).
        //
        // The chain's own *first* block is deliberately left out: moving
        // it would also require a second write to retarget `dir`'s own
        // `TL_EXTENSION` pointer, which every other kept block does not
        // need (its predecessor's `next` field is already being rewritten
        // below) -- outside the "at most the relocated blocks themselves"
        // budget this item was given, so it is skipped rather than forced.
        //
        // "Nearer" means nearer to `dir`, not blindly nearer to the root:
        // a candidate is adopted only when it measurably beats what is
        // already there, so a directory whose cache is already well
        // placed pays nothing here at all.
        let mut pull: Vec<(usize, Allocation)> = Vec::new();
        if self.layout_policy {
            let root_lba = self.vol.root_lba();
            let kept = old.len().min(pages.len());
            for (i, &current) in blocks.iter().enumerate().take(kept).skip(1) {
                let candidate = self
                    .alloc
                    .allocate_for(Intent::MetadataNearRoot { root_lba })?;
                let current_dist = (current as i64 - dir as i64).unsigned_abs();
                let candidate_dist = (candidate.block() as i64 - dir as i64).unsigned_abs();
                if candidate_dist < current_dist {
                    pull.push((i, candidate));
                } else {
                    self.alloc.free(candidate.block())?;
                }
            }
        }

        let mut fresh: Vec<Allocation> = Vec::new();
        if pages.len() > blocks.len() {
            let mut hint = dir;
            for _ in blocks.len()..pages.len() {
                let a = self.alloc.allocate_near(hint)?;
                hint = a.block();
                fresh.push(a);
            }
        }
        if !pull.is_empty() || !fresh.is_empty() {
            self.flush()?;
        }
        let mut pulled_old: Vec<u64> = Vec::with_capacity(pull.len());
        for (i, a) in &pull {
            pulled_old.push(blocks[*i]);
            blocks[*i] = self.alloc.reference(a)?;
        }
        for a in &fresh {
            blocks.push(self.alloc.reference(a)?);
        }

        for (i, (records, count)) in pages.iter().enumerate().rev() {
            let next = if i + 1 < pages.len() {
                blocks[i + 1]
            } else {
                0
            };
            let mut buf = vec![0u8; bs];
            build_dircache_block(&mut buf, blocks[i], dir, records, *count, next);
            self.write(blocks[i], &buf)?;
        }

        // A directory that had no cache at all — one created by a tool
        // that did not know about DOS\4 — gets its pointer now, after the
        // block it names is on the disk.
        if old.is_empty() {
            let mut buf = self.get(dir)?;
            wr32(&mut buf, tail(bs, TL_EXTENSION), blocks[0] as u32);
            self.put(dir, &mut buf)?;
        }

        for &lba in blocks.iter().skip(pages.len()) {
            self.alloc.free(lba)?;
        }
        for lba in pulled_old {
            self.alloc.free(lba)?;
        }
        self.flush()
    }

    // -- dates -------------------------------------------------------------

    /// Stamp a directory's DateStamp and the root's `disk_altered`, if
    /// this session has a clock. See the module documentation for the
    /// four implementations this rule was read out of.
    pub(crate) fn touch(&mut self, dir: u64) -> Result<(), MutateError<Transport<S>>> {
        let now = match self.clock {
            Some(now) => now,
            None => return Ok(()),
        };
        let bs = self.bs();
        let root = self.vol.root_lba();
        let variant = self.vol.variant();
        if dir != root {
            let mut buf = self.get(dir)?;
            write_date(&mut buf, variant, now);
            self.put(dir, &mut buf)?;
        }
        let mut buf = self.get(root)?;
        if dir == root {
            // The root's "directory altered" is longword −23, which is
            // exactly where a classic entry keeps its own date; the root
            // does not move on LNFS.
            wr_date(&mut buf, tail(bs, TL_ROOT_DIR_ALTERED), now);
        }
        wr_date(&mut buf, tail(bs, TL_ROOT_DISK_ALTERED), now);
        self.put(root, &mut buf)
    }

    /// Stamp the root's `disk_altered` (−10) and nothing else — what a
    /// change to a *file's contents* alters.
    ///
    /// Deliberately not [`Mutator::touch`]: writing to a file does not
    /// change what its directory contains, and the four implementations
    /// surveyed in this module's documentation agree on that much even
    /// where they disagree about everything else. amitools stamps a
    /// directory only from `_create_node`/`_delete`; Linux's `affs` stamps
    /// the *file's* inode on a write and the directory's only on an insert
    /// or a remove; AROS stamps every ancestor on a write, which is the
    /// outlier. The file's own DateStamp is stamped too — in the header
    /// write that commits the change, so it costs nothing.
    fn touch_disk(&mut self) -> Result<(), MutateError<Transport<S>>> {
        let now = match self.clock {
            Some(now) => now,
            None => return Ok(()),
        };
        let bs = self.bs();
        let root = self.vol.root_lba();
        let mut buf = self.get(root)?;
        wr_date(&mut buf, tail(bs, TL_ROOT_DISK_ALTERED), now);
        self.put(root, &mut buf)
    }

    // -- block I/O and the bitmap ------------------------------------------

    /// Read a metadata block whole, checksum verified.
    pub(crate) fn get(&mut self, lba: u64) -> Result<Vec<u8>, MutateError<Transport<S>>> {
        self.vol.read_raw(lba)?;
        if !checksum_ok(&self.vol.buf) {
            return Err(MutateError::Read(crate::read::Error::Checksum { lba }));
        }
        Ok(self.vol.buf.clone())
    }

    /// Fix a metadata block's checksum at longword 5 and write it back.
    pub(crate) fn put(
        &mut self,
        lba: u64,
        buf: &mut [u8],
    ) -> Result<(), MutateError<Transport<S>>> {
        finish_checksum(buf);
        self.write(lba, buf)?;
        // The `Volume`'s parsed root is a copy of the bytes as they were;
        // writing the root has just invalidated it, and every later
        // `hash_table(root)` would otherwise answer from the stale one.
        if lba == self.vol.root_lba() {
            self.vol.reload_root()?;
        }
        Ok(())
    }

    pub(crate) fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), MutateError<Transport<S>>> {
        if lba >= self.vol.block_count() {
            return Err(MutateError::Read(crate::read::Error::LbaOutOfRange {
                lba,
                block_count: self.vol.block_count(),
            }));
        }
        self.vol.src.write_block(lba, buf).map_err(MutateError::Io)
    }

    /// Get every dirty bitmap page onto the disk.
    ///
    /// The hinge of the whole ordering: after this returns,
    /// [`Allocator::reference`] will hand out the blocks just allocated,
    /// and before it does it refuses them.
    pub(crate) fn flush(&mut self) -> Result<(), MutateError<Transport<S>>> {
        self.alloc.flush(&mut self.vol.src)?;
        Ok(())
    }

    /// Refresh the LNFS root's `NumBlocksUsed`, which is the only piece
    /// of allocation accounting the format keeps outside the bitmap.
    ///
    /// A no-op on the six variants that have no such field, rather than a
    /// redundant root write on every operation.
    pub(crate) fn stamp_blocks_used(&mut self) -> Result<(), MutateError<Transport<S>>> {
        let variant = self.vol.variant();
        if !variant.has_long_names() {
            return Ok(());
        }
        let root = self.vol.root_lba();
        self.alloc
            .mark_bitmap_valid(&mut self.vol.src, root, variant)?;
        self.vol.reload_root()?;
        Ok(())
    }
}

/// What [`Mutator::prepare`] settled before anything was written.
///
/// It carries the caller's `name` and metadata onward rather than making
/// [`Mutator::emit`] take them again: they were already validated here,
/// and a second copy of the argument list is a second chance to pass them
/// in a different order.
struct Prepared<'a> {
    lba: u64,
    parent: u64,
    comment_block: u32,
    name: &'a [u8],
    meta: &'a Metadata<'a>,
}

/// How many `T_LIST` blocks a file of `n_data` data blocks needs: the
/// header's own table holds the first `slots` pointers and every extension
/// block another `slots`.
fn ext_block_count(n_data: usize, slots: usize) -> usize {
    let over = n_data.saturating_sub(slots);
    over / slots + usize::from(over % slots != 0)
}

/// The interleaved fetch-order positions for a **new** chain of `n_data`
/// data blocks and `n_ext` extension blocks, before any of them has an
/// LBA yet: `true` at each position an extension block belongs, `false`
/// at a data block. Mirrors `crate::compact`'s `fetch_order`, which walks
/// the same interleaving over a chain that already exists; this is the
/// version for one that is about to be allocated in one run.
///
/// An extension block lands right after the last data block of the
/// `slots`-sized group it closes — matching the order the pre-wave-3
/// per-block loop already produced by allocating it just before that
/// group's first data block, which is the same position, one call
/// earlier — so the run this produces reads in the order a streaming
/// reader actually walks it.
fn interleaved_positions(n_data: usize, n_ext: usize, slots: usize) -> Vec<bool> {
    let mut out = Vec::with_capacity(n_data + n_ext);
    let mut ext_used = 0usize;
    for i in 0..n_data {
        out.push(false);
        if ext_used < n_ext && (i + 1) % slots == 0 {
            out.push(true);
            ext_used += 1;
        }
    }
    debug_assert_eq!(ext_used, n_ext);
    out
}

/// Does the block starting at `start` and `len` bytes long overlap the
/// byte range `lo..hi`? An empty range overlaps nothing, which is what
/// makes a pure shrink touch no data block on FFS at all.
fn overlaps(start: u64, len: usize, lo: u64, hi: u64) -> bool {
    lo < hi && start < hi && lo < start + len as u64
}
