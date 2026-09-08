//! In-place volume resize: grow or shrink a volume without reformatting.
//!
//! # Why the root has to move
//!
//! FFS never stores its own size. The root's location is *derived* from
//! the geometry the mounter hands in —
//! [`canonical_root_lba`](crate::canonical_root_lba), `reserved + (blocks -
//! reserved - 1) / 2` — recomputed on every mount, never read off the
//! disk. So on any change to `block_count` the root **must move** to the
//! new midpoint; leaving it where it was produces a volume that mounts as
//! Uninitialized, because nothing is at the LBA a fresh mount looks at.
//!
//! Moving it means copying the root block's contents to the new LBA and
//! **re-parenting its direct children**: every top-level entry's `parent`
//! longword (tail −3) names the root by block number, and the root itself
//! is the only block whose own number changes. Entries deeper in the tree
//! are untouched — their parent is their own directory, not the root —
//! and hard-link `real_entry` pointers are untouched too, because no child
//! *header* block moves, only the root. Two more things point at the
//! root's old number and need the same fix: the root's own dircache chain
//! on `DOS\4`/`DOS\5` (each block's `parent` field, longword 2, names the
//! directory it caches) and the boot block's advisory root pointer
//! (longword 2 — never consulted by a mount, since that recomputes the
//! root anyway, but worth keeping honest). Nothing else references the
//! root by number: soft-link paths are text, not block pointers, and
//! bitmap pages belong to no directory.
//!
//! Ported from the algorithm in AmiPart's `src/ffsresize.c` (John
//! Hertell, MIT — <https://github.com/ChuckyGang/AmiPart>), read for its
//! shape and cited rather than copied; the AmigaOS `LongName*`/`RootBlock`
//! field layouts and this crate's own [`layout`](crate::layout) are the
//! actual source for every offset used here. Three places this
//! deliberately goes further than AmiPart does:
//!
//! - AmiPart refuses block sizes other than 512/1024 and does not touch
//!   `DOS\4`/`DOS\5` dircaches or LNFS fields; this handles every block
//!   size 512..=32768, relocates the root's dircache chain when a shrink's
//!   cut would otherwise strand it, and keeps `NumBlocksUsed` and
//!   `FileSystemType` correct on `DOS\6`/`DOS\7`.
//! - AmiPart stamps `bm_flag = 0` and leaves the bitmap for FFS to rebuild
//!   on the next mount. This rebuilds it immediately, by reusing
//!   [`Volume::repair`] — the same reachability walk
//!   [`validate`](crate::Volume::validate) uses — so [`Volume::resize`]
//!   hands back a volume with a **valid** bitmap, not one waiting for
//!   something else to fix it.
//! - AmiPart has no shrink-refusal estimate; [`Volume::minimum_size`] is a
//!   read-only query a caller can run before committing to anything.
//!
//! # What counts as "movable metadata"
//!
//! A shrink's cut can only pass through blocks nothing but the *format
//! itself* depends on the exact location of: the root, the bitmap's pages
//! and extension blocks, and — a deliberate choice, documented here rather
//! than left implicit — the root's own dircache chain, because this module
//! already has to relocate it for the reparenting fix above and refusing a
//! shrink over a strandable dircache block would be refusing something
//! this code can fix. A *directory's* dircache is not on that list: moving
//! one means finding and patching the one pointer that names it (its
//! owning directory's tail −2), which is ordinary [`Mutator`](crate::Mutator)
//! territory this operation does not reach into. Anything else — a file's
//! header, its data, its extension chain, another directory's header, an
//! overflow comment block — sitting at or past the new end refuses the
//! shrink outright, naming the offending block.
//!
//! # Ordering: two roots for a window, and which one is honest
//!
//! By the time [`Volume::resize`] runs, geometry has already changed
//! underneath it — see "Whose job is what" below — so a mount attempt
//! already computes the *new* root position. The first disk write this
//! function makes is therefore the one that matters most: a full copy of
//! the (still entirely valid) old root at the new LBA, with `bitmap_flag`
//! forced to 0. One block write, and from that instant a fresh mount finds
//! a structurally valid root that honestly declares its own bitmap
//! untrustworthy — exactly the state [`Volume::repair`] exists to fix, and
//! exactly the state a normal mid-update mutation leaves behind. Every
//! write after that — reparenting children, relocating the dircache,
//! freeing what the move and the shrink make obsolete, rebuilding the
//! bitmap — only ever improves on that floor: nothing between the first
//! write and the last (`bitmap_flag = -1`, the same "flag last" rule
//! [`repair`](crate::repair) and [`Allocator`](crate::Allocator) use) turns
//! a safe state into an unsafe one. An interruption anywhere in that
//! window leaves a volume with some pointers still naming the old root —
//! reported as `ParentMismatch`/`DircacheStale` by
//! [`validate`](crate::Volume::validate), not data loss, since the hash
//! chains themselves are never touched and every file stays reachable and
//! byte-correct throughout.
//!
//! # Retrying
//!
//! [`Volume::repair`] alone only ever rebuilds the bitmap — it has no
//! opinion on a stray `parent` longword or a dircache chain mid-move, so
//! calling it after an interrupted [`Volume::resize`] leaves a volume with
//! a valid bitmap and *may* still leave `ParentMismatch`/`DircacheStale`
//! findings behind. What finishes the job is calling [`Volume::resize`]
//! **again with the same `new_block_count`**: every write past the first
//! is idempotent (the same parent value, the same dircache content,
//! written again is a no-op on disk), so a retry redoes only what an
//! earlier call left undone, rather than being a no-op itself — a plain
//! "nothing to do" return only happens when the size is already right
//! *and* `bitmap_flag` already reads valid, which is precisely the signal
//! that nothing was left mid-update.
//!
//! Two things a retry cannot recover, both bounded to the safe (never
//! `ReachableButFree`) direction and both pinned by the shrink crash sweep
//! in `tests/volumes.rs` rather than left as prose:
//!
//! - The *very first* root position, from before any call in the
//!   sequence, if the crash landed after that root moved but before its
//!   block was explicitly freed. A retry's notion of "the old root" is
//!   the position the *current* call found on entry, which by then is
//!   already the new one — so the original block is not rediscovered, and
//!   stays allocated as an ordinary, harmless
//!   [`Finding::OrphanBlock`](crate::Finding::OrphanBlock) leak rather
//!   than being freed.
//! - The root's own dircache chain, if the crash landed after the first
//!   write (which forces `bitmap_flag` to 0 but otherwise carries the old
//!   root's fields verbatim, dircache pointer included) but before that
//!   pointer was updated — a retry can then find the pointer naming a
//!   block a shrink has since sliced clean off the volume, unreadable
//!   through any bound this call still has a use for. Rather than fail
//!   the whole retry over a cache, the pointer is cleared and the chain
//!   is treated as gone: [`Finding::DircacheStale`](crate::Finding::DircacheStale)
//!   reports it, and any
//!   later [`Mutator`](crate::Mutator) operation on the root regenerates
//!   it from the (untouched, never-at-risk) hash chains.
//!
//! Both are losses of *bookkeeping*, not of data: nothing here ever frees
//! a block something still reaches, and nothing here ever loses a file.
//!
//! # Whose job is what
//!
//! This module changes only what is *inside* the partition. The RDB entry
//! — `high_cyl`, in amiga-rdb's terms — is the caller's, and the order
//! matters in both directions because a volume must never be told it is
//! bigger than the medium underneath it actually is:
//!
//! - **Grow**: enlarge the partition first, then call [`Volume::resize`].
//!   [`Volume::resize`] refuses ([`ResizeError::SinkTooSmall`]) if the
//!   medium reports fewer blocks than the target, so calling it before the
//!   partition is enlarged fails safely rather than writing past the
//!   medium's own idea of its size.
//! - **Shrink**: call [`Volume::resize`] first, then shrink the partition.
//!   Shrinking the partition first would place blocks this crate still
//!   considers part of the volume (however briefly) outside what the
//!   medium reports, which is the same hazard from the other direction.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::allocator::AllocError;
use crate::bitmap::Bitmap;
use crate::build::finish_checksum;
use crate::format::{div_ceil, wr32, OFF_BOOT_ROOT};
use crate::layout::*;
use crate::read::{Error as ReadError, Volume};
use crate::repair::{RepairOptions, RepairReport};
use crate::{be32, checksum_compute, checksum_ok, BlockMedium, BlockSource, Transport};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything [`Volume::resize`] refuses, and why.
///
/// Generic over the medium's error for the same reason every other error
/// type in this crate is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResizeError<E> {
    /// A read failed.
    Read(ReadError<E>),
    /// The bitmap rebuild ([`Volume::repair`]) refused — most likely
    /// [`AllocError::VolumeFull`], a target too small for its own
    /// metadata.
    Alloc(AllocError<E>),
    /// A write to the medium failed.
    Io(E),
    /// `new_block_count` leaves no room for a root block at all.
    NoRoot {
        /// The requested block count.
        block_count: u64,
        /// Blocks reserved at the front.
        reserved: u64,
    },
    /// `new_block_count` exceeds what a 32-bit block pointer can name.
    VolumeTooLarge {
        /// The requested block count.
        block_count: u64,
    },
    /// Growing past what the medium itself reports having.
    ///
    /// See this module's documentation: the RDB (or whatever geometry
    /// authority the caller has) must enlarge the partition *before*
    /// calling [`Volume::resize`] to grow, and this is the refusal that
    /// catches a caller who did it the other way round.
    SinkTooSmall {
        /// The requested block count.
        block_count: u64,
        /// What the medium reports.
        sink_blocks: u64,
    },
    /// The new root's block is already in use by something this
    /// operation does not relocate.
    ///
    /// The new midpoint can, on a sufficiently full or sufficiently
    /// asymmetric volume, land on a block a file is currently using. This
    /// operation moves the *root*, not arbitrary user data, so it refuses
    /// rather than displacing whatever is there.
    RootTargetOccupied {
        /// The block the new root would occupy.
        lba: u64,
    },
    /// A shrink refused: this block is allocated, sits at or past
    /// `new_block_count`, and is not movable metadata (the root, a
    /// bitmap page or extension block, or a block of the root's own
    /// dircache chain).
    UserDataPastCut {
        /// The offending block.
        lba: u64,
        /// The size that was refused.
        new_block_count: u64,
    },
    /// A shrink needed to relocate a piece of metadata below the new cut
    /// and found no free block there to put it. Vanishingly unlikely —
    /// it means the volume is packed solid right up to the new edge —
    /// but a shrink that cannot place what it must move is refused rather
    /// than attempted.
    NoRoomBelowCut,
}

impl<E: fmt::Display> fmt::Display for ResizeError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(e) => write!(f, "{e}"),
            Self::Alloc(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "block write failed: {e}"),
            Self::NoRoot {
                block_count,
                reserved,
            } => write!(
                f,
                "{block_count} blocks with {reserved} reserved has no room for a root block"
            ),
            Self::VolumeTooLarge { block_count } => write!(
                f,
                "{block_count} blocks is more than a 32-bit block pointer can name"
            ),
            Self::SinkTooSmall {
                block_count,
                sink_blocks,
            } => write!(
                f,
                "cannot grow to {block_count} blocks: the medium reports only {sink_blocks} -- \
                 enlarge the partition before resizing the filesystem"
            ),
            Self::RootTargetOccupied { lba } => write!(
                f,
                "block {lba}, where the new root would go, is already in use and this operation \
                 does not relocate user data"
            ),
            Self::UserDataPastCut {
                lba,
                new_block_count,
            } => write!(
                f,
                "block {lba} is in use and not movable metadata, but a shrink to \
                 {new_block_count} would put it past the end"
            ),
            Self::NoRoomBelowCut => f.write_str(
                "a shrink needed to relocate a metadata block below the new end and found no \
                 free block there",
            ),
        }
    }
}

#[cfg(feature = "std")]
impl<E: std::error::Error + 'static> std::error::Error for ResizeError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(e) => Some(e),
            Self::Alloc(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl<E> From<ReadError<E>> for ResizeError<E> {
    fn from(e: ReadError<E>) -> Self {
        Self::Read(e)
    }
}

impl<E> From<AllocError<E>> for ResizeError<E> {
    fn from(e: AllocError<E>) -> Self {
        Self::Alloc(e)
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// What [`Volume::resize`] did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResizeReport {
    /// The block count before.
    pub old_block_count: u64,
    /// The block count after — `new_block_count`, echoed back.
    pub new_block_count: u64,
    /// Where the root was.
    pub old_root_lba: u64,
    /// Where the root is now. Equal to `old_root_lba` when the midpoint
    /// did not move, which happens for some small size changes.
    pub new_root_lba: u64,
    /// Direct children of the root whose `parent` longword was rewritten.
    /// Zero when the root did not move.
    pub children_reparented: u64,
    /// Blocks of the root's own dircache chain that had to move because a
    /// shrink's cut would otherwise have stranded them past the new end.
    pub dircache_blocks_relocated: u64,
    /// Blocks explicitly freed: the old root (when it moved) and any
    /// bitmap page or extension block the new size no longer needs.
    /// Distinct from or the bitmap rebuild's own accounting below —
    /// this is the "we know these are free" set patched in *before*
    /// [`Volume::repair`] ever runs, so it does not get reported as a
    /// leak.
    pub blocks_freed: u64,
    /// The [`Volume::repair`] call this operation ends with — the bitmap
    /// rebuild, in full.
    pub repair: RepairReport,
}

// ---------------------------------------------------------------------------
// The operation
// ---------------------------------------------------------------------------

impl<S: BlockSource> Volume<S> {
    /// A read-only estimate of the smallest `new_block_count` a shrink
    /// could target without cutting off anything but movable metadata.
    ///
    /// Computed from the bitmap and, on `DOS\4`/`DOS\5`, the root's own
    /// dircache chain — nothing here writes. Movable, per
    /// [`Volume::resize`]'s documentation: the root, every bitmap page and
    /// extension block, and the root's own dircache chain. Everything
    /// else the bitmap marks allocated is a floor: `resize(minimum_size())`
    /// succeeds, `resize(minimum_size() - 1)` refuses with
    /// [`ResizeError::UserDataPastCut`] naming the block this function's
    /// scan found.
    ///
    /// An untrustworthy bitmap (`bitmap_flag == 0`) is not refused here —
    /// the bits are read exactly as [`Volume::read_bitmap`] returns them,
    /// which is honestly what an *estimate* over unreliable data should
    /// do — but [`Volume::resize`] itself refuses one via the same
    /// [`AllocError::BitmapInvalid`] [`Volume::repair`] would, so run
    /// [`Volume::repair`] first if this looks suspicious.
    ///
    /// This also accounts for [`Volume::resize`]'s one other shrink
    /// refusal, [`ResizeError::RootTargetOccupied`]: the new root's own
    /// midpoint has to land somewhere free (or on other movable metadata),
    /// and on a volume packed solid from `reserved` upward that can push
    /// the achievable minimum well past the last non-movable block, since
    /// this operation relocates the root but not arbitrary user data in
    /// its way. A caller that wants the *theoretical* floor — what a tool
    /// willing to relocate data in the root's path could reach — has to
    /// defragment first; this reports what [`Volume::resize`] will
    /// actually accept.
    pub fn minimum_size(&mut self) -> Result<u64, ReadError<S::Error>> {
        let reserved = self.reserved();
        let block_count = self.block_count();
        let bitmap = self.read_bitmap()?;
        let movable = movable_metadata(self, &bitmap)?;

        let mut floor = reserved + 1;
        for lba in bitmap.allocated() {
            if lba + 1 > floor && !movable.contains(&lba) {
                floor = lba + 1;
            }
        }
        if floor > block_count {
            return Ok(floor);
        }

        // The smallest achievable size is tied to the smallest achievable
        // *root position*: search forward from the root position `floor`
        // would imply until one lands on a block [`Volume::resize`] can
        // safely put the root on — free, the current root, or a bitmap
        // page/extension block, but *not* the root's own dircache chain,
        // for the same ordering reason [`Volume::resize`] itself excludes
        // it from the collision exemption — then convert back to the size
        // that puts the root exactly there.
        let root_lba = self.root_lba();
        let root_safe = |lba: u64| -> bool {
            lba == root_lba || bitmap.pages().contains(&lba) || bitmap.ext_blocks().contains(&lba)
        };
        let mut root_target = crate::canonical_root_lba(floor, reserved).unwrap_or(reserved);
        while root_target < block_count
            && bitmap.is_allocated(root_target) == Some(true)
            && !root_safe(root_target)
        {
            root_target += 1;
        }
        let min = reserved + 1 + 2 * (root_target - reserved);
        Ok(min.max(floor))
    }
}

impl<S: BlockMedium> Volume<S> {
    /// Grow or shrink this volume in place to `new_block_count` blocks.
    ///
    /// See this module's documentation for the algorithm, the crash
    /// ordering and what this deliberately does differently from AmiPart's
    /// `ffsresize.c`. A no-op when `new_block_count` already equals the
    /// current size (returns immediately, no writes).
    ///
    /// This changes only what is *inside* the partition — see "Whose job
    /// is what" above for the RDB ordering rule the caller must follow in
    /// each direction.
    ///
    /// Refuses (without writing anything) if:
    /// - there is no room for a root at the new size
    ///   ([`ResizeError::NoRoot`]),
    /// - `new_block_count` will not fit in a 32-bit block pointer
    ///   ([`ResizeError::VolumeTooLarge`]),
    /// - growing past what the medium itself reports
    ///   ([`ResizeError::SinkTooSmall`]),
    /// - the new root's block is occupied by something this operation does
    ///   not relocate ([`ResizeError::RootTargetOccupied`]), or
    /// - shrinking would cut off something that is not movable metadata
    ///   ([`ResizeError::UserDataPastCut`]) — see [`Volume::minimum_size`]
    ///   to check ahead of time.
    ///
    /// The bitmap rebuild this ends with ([`Volume::repair`]) can also
    /// refuse — [`ResizeError::Alloc`] — most plausibly with
    /// [`AllocError::VolumeFull`] if `new_block_count` leaves no room for
    /// the volume's own metadata; and it refuses outright if the bitmap
    /// was already untrustworthy before this call
    /// ([`AllocError::BitmapInvalid`]), the same refusal
    /// [`crate::Mutator::open`] gives — run [`Volume::repair`] first.
    pub fn resize(
        &mut self,
        new_block_count: u64,
    ) -> Result<ResizeReport, ResizeError<Transport<S>>> {
        let bs = self.block_size();
        let reserved = self.reserved();
        let old_block_count = self.block_count();
        let old_root_lba = self.root_lba();

        // A true no-op only when the size is already right *and* nothing
        // was left unfinished: `bitmap_flag == -1` means either nothing
        // has touched this volume, or a previous call (a resize or
        // anything else) already ran the whole thing, root pointer moves
        // and all, to completion. Everything below this point is
        // otherwise safe to redo — see "Retrying" below — which is what
        // makes a same-target retry able to finish a resize a crash
        // interrupted, rather than only being able to leave the bitmap
        // valid over an otherwise-unfinished move.
        if new_block_count == old_block_count && self.root().bitmap_flag == -1 {
            return Ok(ResizeReport {
                old_block_count,
                new_block_count,
                old_root_lba,
                new_root_lba: old_root_lba,
                ..Default::default()
            });
        }
        if new_block_count > u64::from(u32::MAX) {
            return Err(ResizeError::VolumeTooLarge {
                block_count: new_block_count,
            });
        }
        let new_root_lba =
            crate::canonical_root_lba(new_block_count, reserved).ok_or(ResizeError::NoRoot {
                block_count: new_block_count,
                reserved,
            })?;
        if new_block_count > old_block_count {
            if let Some(sink_blocks) = BlockSource::block_count(self.source_mut()) {
                if sink_blocks < new_block_count {
                    return Err(ResizeError::SinkTooSmall {
                        block_count: new_block_count,
                        sink_blocks,
                    });
                }
            }
        }

        // A retry of an interrupted resize: the current root may still
        // carry a stale copy of the *previous* geometry's bitmap
        // pointers, which the ordinary (trusting) read below cannot
        // survive if any of them are now out of range. Rebuild first,
        // through the same tolerant reader `repair` always uses, so
        // everything from here on can trust what it reads. A no-op, and
        // harmless, whenever there was nothing to fix.
        if self.root().bitmap_flag != -1 {
            self.repair(&RepairOptions::default())?;
        }

        // Everything read-only, before a single byte is written: the old
        // bitmap (the authority for every "is this free" question asked
        // below), the movable set, and — for a shrink — the refusal check
        // itself.
        let old_bitmap = self.read_bitmap()?;
        // The root's own dircache pointer is not something `repair` fixes
        // (it rebuilds the bitmap, nothing else), so on the same kind of
        // retry it can still be a stale pointer this call can no longer
        // read — most likely one a *previous* attempt was mid-relocating
        // when the crash landed. There being nothing left to recover
        // through it is treated the same as there being no chain at all:
        // the pointer is cleared once a fresh one is known (see below),
        // and losing it costs nothing but the cache itself, which is
        // advisory and gets regenerated by the next `Mutator` operation
        // that touches the root.
        let mut dircache_pointer_unreadable = false;
        let root_dircache = if self.variant().has_dircache() {
            match self.read_dircache(old_root_lba) {
                Ok(dc) => dc.blocks,
                Err(_) => {
                    dircache_pointer_unreadable = true;
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let movable = movable_metadata_from(old_root_lba, &old_bitmap, &root_dircache);

        // A bitmap page or extension block in the way is safe to land the
        // new root on: repair()'s own tolerant reader only reuses a page
        // pointer that is neither reached nor claimed, and a page the new
        // root has just occupied fails that check and gets a fresh
        // replacement allocated in its place. The root's own dircache
        // chain is *not* exempted here even though it is otherwise
        // movable metadata — this code reads those blocks by their old
        // LBA after the new root is already written, and a collision
        // would read the root's own bytes back as a dircache block.
        let root_safe = new_root_lba == old_root_lba
            || old_bitmap.pages().contains(&new_root_lba)
            || old_bitmap.ext_blocks().contains(&new_root_lba);
        if !root_safe
            && new_root_lba < old_block_count
            && old_bitmap.is_allocated(new_root_lba) == Some(true)
        {
            return Err(ResizeError::RootTargetOccupied { lba: new_root_lba });
        }
        if new_block_count < old_block_count {
            for lba in old_bitmap.allocated() {
                if lba >= new_block_count && !movable.contains(&lba) {
                    return Err(ResizeError::UserDataPastCut {
                        lba,
                        new_block_count,
                    });
                }
            }
        }

        // Where the root's own dircache chain will live afterwards: in
        // place where it still fits under the new end, relocated to a
        // free block below the cut where it does not. Decided before any
        // write, so a `NoRoomBelowCut` refusal still leaves the volume
        // untouched.
        let mut claimed: Vec<u64> = vec![new_root_lba];
        let mut dircache_final: Vec<u64> = Vec::with_capacity(root_dircache.len());
        for &old in &root_dircache {
            let final_lba = if old < new_block_count && old != new_root_lba {
                old
            } else {
                pick_free(&old_bitmap, &claimed, new_block_count)
                    .ok_or(ResizeError::NoRoomBelowCut)?
            };
            claimed.push(final_lba);
            dircache_final.push(final_lba);
        }

        let write_bound = old_block_count.max(new_block_count);
        let root_moved = new_root_lba != old_root_lba;

        // The one write that matters most: a full copy of the (still
        // entirely valid) root at the new LBA, bitmap_flag forced to 0.
        // See this module's documentation for why this goes first.
        self.read_checked(old_root_lba)?;
        let mut root_buf = self.buf.clone();
        wr32(&mut root_buf, tail(bs, TL_BITMAP_FLAG), 0);
        finish_checksum(&mut root_buf);
        write_raw(self, new_root_lba, write_bound, &root_buf)?;

        // From here on the volume's own idea of its geometry is the new
        // one: writes below range-check against `write_bound` (which
        // covers both), but every *read* through the ordinary Volume
        // methods (guard_chain, entry_at, reload_root...) needs
        // `block_count` and `root.lba` to already be right.
        self.block_count = new_block_count;
        self.root.lba = new_root_lba;
        self.reload_root()?;

        let mut children_reparented = 0u64;
        let mut dircache_blocks_relocated = 0u64;

        // Not gated on `root_moved`: every write below is idempotent
        // (the same parent value, the same dircache content, written
        // again is a no-op on disk) and unconditional, so that a retry —
        // calling `resize` again with the same target after a crash, with
        // `bitmap_flag` still 0 from before — finishes whatever a
        // previous call left half-done rather than only rebuilding the
        // bitmap around it. See "Retrying" in this module's
        // documentation.
        {
            // Direct children only: the entries the root's own hash table
            // names. A deeper entry's parent is its own directory, not
            // the root, and is untouched.
            let table = self.root().hash_table.clone();
            let mut visited: Vec<u64> = Vec::new();
            for &head in &table {
                let mut next = head;
                while next != 0 {
                    let lba = next as u64;
                    self.guard_chain(&mut visited, lba)?;
                    self.read_checked(lba)?;
                    let mut buf = self.buf.clone();
                    let chain_next = be32(&buf, tail(bs, TL_HASH_CHAIN));
                    wr32(&mut buf, tail(bs, TL_PARENT), new_root_lba as u32);
                    finish_checksum(&mut buf);
                    write_raw(self, lba, write_bound, &buf)?;
                    children_reparented += 1;
                    next = chain_next;
                }
            }

            // The root's own dircache chain: parent field on every block
            // (DOS\4/DOS\5 names the directory it caches at longword 2),
            // and relocated where the plan above put it somewhere new.
            // Written backwards, last block first, so a `next` pointer
            // never names a block that is not there yet — the same rule
            // `Mutator::refresh_dircache` uses.
            for i in (0..root_dircache.len()).rev() {
                let old_lba = root_dircache[i];
                let final_lba = dircache_final[i];
                let next_final = dircache_final.get(i + 1).copied().unwrap_or(0);
                // Not `self.read_checked`: `old_lba` may be at or past
                // `new_block_count` when this chain is being relocated
                // *because* it would otherwise be stranded there, and
                // `self.block_count` is already the new, smaller value.
                let mut buf = read_raw_bounded(self, old_lba, write_bound)?;
                if !checksum_ok(&buf) {
                    return Err(ResizeError::Read(ReadError::Checksum { lba: old_lba }));
                }
                wr32(&mut buf, OFF_OWN_KEY, final_lba as u32);
                wr32(&mut buf, OFF_DIRCACHE_PARENT, new_root_lba as u32);
                wr32(&mut buf, OFF_DIRCACHE_NEXT, next_final as u32);
                finish_checksum(&mut buf);
                write_raw(self, final_lba, write_bound, &buf)?;
                if final_lba != old_lba {
                    dircache_blocks_relocated += 1;
                }
            }
            // Written whenever there is a fresh head to point at, *or*
            // the previous pointer turned out to be unreadable and has to
            // be cleared rather than left dangling.
            if !dircache_final.is_empty() || dircache_pointer_unreadable {
                let new_head = dircache_final.first().copied().unwrap_or(0);
                self.read_checked(new_root_lba)?;
                let mut buf = self.buf.clone();
                wr32(&mut buf, tail(bs, TL_EXTENSION), new_head as u32);
                finish_checksum(&mut buf);
                write_raw(self, new_root_lba, write_bound, &buf)?;
                self.reload_root()?;
            }
        }

        // Advisory, and cheap: the boot block's root pointer. Nothing in
        // this crate's own mount path reads it back.
        self.read_raw(0)?;
        let mut boot = self.buf.clone();
        wr32(&mut boot, OFF_BOOT_ROOT, new_root_lba as u32);
        write_raw(self, 0, write_bound, &boot)?;

        // Everything this operation knows for certain is now free: the
        // old root (once nothing points at it any more) and any bitmap
        // page or extension block the new size no longer needs. Patched
        // directly into the *old*, still-physically-intact bitmap pages
        // before `repair` ever runs — otherwise repair's one-direction
        // rule (never remove allocation an incomplete walk might have
        // missed) would keep every one of these as a permanent leak.
        let per_page = bitmap_bits_per_block(bs);
        let per_ext = bitmap_ext_pointers(bs);
        let new_need_pages = div_ceil(new_block_count.saturating_sub(reserved), per_page) as usize;
        let new_need_ext = if new_need_pages > BITMAP_PAGES {
            div_ceil((new_need_pages - BITMAP_PAGES) as u64, per_ext as u64) as usize
        } else {
            0
        };
        let mut to_free: Vec<u64> = Vec::new();
        if root_moved && old_root_lba < new_block_count {
            to_free.push(old_root_lba);
        }
        if old_bitmap.pages().len() > new_need_pages {
            to_free.extend(
                old_bitmap.pages()[new_need_pages..]
                    .iter()
                    .filter(|&&p| p < new_block_count),
            );
        }
        if old_bitmap.ext_blocks().len() > new_need_ext {
            to_free.extend(
                old_bitmap.ext_blocks()[new_need_ext..]
                    .iter()
                    .filter(|&&e| e < new_block_count),
            );
        }
        let mut blocks_freed = 0u64;
        for lba in to_free {
            if free_bit_on_disk(self, &old_bitmap, reserved, bs, lba, write_bound)? {
                blocks_freed += 1;
            }
        }

        // The rebuild: the same reachability walk `validate()` runs,
        // reused rather than duplicated (see this module's
        // documentation). Ends with `bitmap_flag = -1`, last of all.
        let repair = self.repair(&RepairOptions::default())?;

        Ok(ResizeReport {
            old_block_count,
            new_block_count,
            old_root_lba,
            new_root_lba,
            children_reparented,
            dircache_blocks_relocated,
            blocks_freed,
            repair,
        })
    }
}

/// The set of blocks a shrink's cut is allowed to pass through: the root,
/// every bitmap page and extension block, and the root's own dircache
/// chain. See this module's documentation for why the dircache chain is
/// on this list and a directory's own cache is not.
fn movable_metadata<S: BlockSource>(
    vol: &mut Volume<S>,
    bitmap: &Bitmap,
) -> Result<Vec<u64>, ReadError<S::Error>> {
    let root_lba = vol.root_lba();
    let root_dircache = if vol.variant().has_dircache() {
        vol.read_dircache(root_lba)?.blocks
    } else {
        Vec::new()
    };
    Ok(movable_metadata_from(root_lba, bitmap, &root_dircache))
}

fn movable_metadata_from(root_lba: u64, bitmap: &Bitmap, root_dircache: &[u64]) -> Vec<u64> {
    let mut v = Vec::with_capacity(
        2 + bitmap.pages().len() + bitmap.ext_blocks().len() + root_dircache.len(),
    );
    v.push(root_lba);
    v.extend_from_slice(bitmap.pages());
    v.extend_from_slice(bitmap.ext_blocks());
    v.extend_from_slice(root_dircache);
    v
}

/// The lowest block the old bitmap calls free, under `new_block_count`
/// and not already spoken for this session.
fn pick_free(bitmap: &Bitmap, claimed: &[u64], new_block_count: u64) -> Option<u64> {
    bitmap
        .free()
        .find(|&lba| lba < new_block_count && !claimed.contains(&lba))
}

/// Clear one block's bit directly in whichever *old* bitmap page still
/// physically covers it, without going through an [`Allocator`](crate::Allocator)
/// session. Used only for blocks this operation knows for certain are
/// free — the old root, and bitmap pages/extension blocks the new size
/// has made obsolete — before [`Volume::repair`] gets a chance to
/// preserve them as leaks.
///
/// A no-op, not an error, when the page that would need patching is
/// itself past `new_block_count`: [`Volume::repair`] will replace that
/// page wholesale from the walk (its pointer fails the same range check),
/// and a page about to be discarded and rebuilt has nothing here worth
/// patching.
fn free_bit_on_disk<S: BlockMedium>(
    vol: &mut Volume<S>,
    old_bitmap: &Bitmap,
    reserved: u64,
    bs: usize,
    lba: u64,
    write_bound: u64,
) -> Result<bool, ResizeError<Transport<S>>> {
    if lba < reserved {
        return Ok(false);
    }
    let per_page = bitmap_bits_per_block(bs);
    let bit_in_volume = lba - reserved;
    let page_index = (bit_in_volume / per_page) as usize;
    let page_lba = match old_bitmap.pages().get(page_index) {
        Some(&p) => p,
        None => return Ok(false),
    };
    if page_lba >= vol.block_count() {
        return Ok(false);
    }
    let bit_in_page = bit_in_volume % per_page;
    let word_off = OFF_BITMAP_BITS + (bit_in_page / 32) as usize * 4;
    let bit = (bit_in_page % 32) as u32;

    vol.read_raw(page_lba)?;
    let mut buf = vol.buf.clone();
    let word = be32(&buf, word_off);
    if word >> bit & 1 == 1 {
        return Ok(false); // Already free.
    }
    wr32(&mut buf, word_off, word | (1 << bit));
    let ck = checksum_compute(&buf, BITMAP_CHECKSUM_INDEX);
    wr32(&mut buf, BITMAP_CHECKSUM_INDEX * 4, ck);
    write_raw(vol, page_lba, write_bound, &buf)?;
    Ok(true)
}

/// A range-checked read against `write_bound` rather than the `Volume`'s
/// own `block_count` — the read-side counterpart of [`write_raw`], and for
/// the same reason: a block this function already knows is valid may
/// currently be on the wrong side of whichever bound `self.block_count`
/// happens to hold mid-resize.
fn read_raw_bounded<S: BlockMedium>(
    vol: &mut Volume<S>,
    lba: u64,
    write_bound: u64,
) -> Result<Vec<u8>, ResizeError<Transport<S>>> {
    if lba >= write_bound {
        return Err(ResizeError::Read(ReadError::LbaOutOfRange {
            lba,
            block_count: write_bound,
        }));
    }
    let mut buf = vec![0u8; vol.block_size()];
    vol.source_mut()
        .read_block(lba, &mut buf)
        .map_err(ResizeError::Io)?;
    Ok(buf)
}

/// A range-checked write against `write_bound` rather than the `Volume`'s
/// own `block_count` — which, mid-resize, may not yet (grow) or no longer
/// (shrink) be the bound that matters for a specific block this function
/// already knows is valid.
fn write_raw<S: BlockMedium>(
    vol: &mut Volume<S>,
    lba: u64,
    write_bound: u64,
    buf: &[u8],
) -> Result<(), ResizeError<Transport<S>>> {
    if lba >= write_bound {
        return Err(ResizeError::Read(ReadError::LbaOutOfRange {
            lba,
            block_count: write_bound,
        }));
    }
    vol.source_mut()
        .write_block(lba, buf)
        .map_err(ResizeError::Io)
}
