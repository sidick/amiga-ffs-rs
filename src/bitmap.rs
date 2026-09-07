//! The allocation bitmap: which blocks the volume believes are in use.
//!
//! One bit per block, and four conventions that are each easy to get
//! backwards. All four were confirmed by dumping bitmaps out of images
//! xdftool built and checking that the blocks they mark allocated are the
//! blocks the filesystem actually wrote:
//!
//! 1. **A set bit means the block is *free*.** Not allocated. A freshly
//!    formatted volume is nearly all ones. Reading this the usual way
//!    round produces an allocator that hands out the blocks in use and
//!    refuses the empty ones.
//! 2. **Bits run LSB-first within each longword.** Bit 0 of the first
//!    longword after the checksum is the first block the bitmap covers;
//!    bit 31 is the thirty-second. Big-endian longwords, little-endian
//!    bits — the format is not being consistent, and a reader that
//!    assumes it is gets a scrambled but plausible-looking answer.
//! 3. **The first bit is block `reserved`**, normally 2. The two boot
//!    blocks are never in the bitmap at all: they are not allocatable, so
//!    they have no bit, and treating bit 0 as block 0 shifts every answer
//!    by two.
//! 4. **The checksum is longword 0**, not longword 5 like every header
//!    block ([`BITMAP_CHECKSUM_INDEX`]). A bitmap block has no type
//!    longword and no own-key; the checksum is the entire header.
//!
//! # Where the pages live
//!
//! The root block holds [`BITMAP_PAGES`] (25) pointers to bitmap blocks
//! at longwords −49..=−25, and longword −24 chains to a **bitmap
//! extension block** when 25 is not enough. At 512 bytes one page covers
//! 4064 blocks, so the root alone reaches 101 600 blocks — about 50 MB,
//! which is where the historical "FFS can't do more than 50-odd MB"
//! folklore comes from: old filesystems wrote a directory's protection
//! longword into the root, at longword −24, and clobbered the extension
//! pointer.
//!
//! An extension block is not like other blocks: no type, no own key, **no
//! checksum**, just [`bitmap_ext_pointers`] page pointers with the last
//! longword chaining to the next extension block. There is nothing in one
//! to verify, which is worth knowing before trying to.
//!
//! # `bitmap_flag`
//!
//! The root's longword −50 is −1 when the bitmap is valid and 0 when it
//! is not. AmigaDOS clears it while a volume is mounted-and-dirty and
//! sets it again on a clean unmount, so a 0 means the volume was not shut
//! down cleanly and the bitmap on disk is whatever it was mid-update.
//! [`Bitmap::valid`] carries that through rather than hiding it: the bits
//! are still read, because a recovery tool wants to see them, and they
//! are still marked untrustworthy, because an allocator must not use
//! them.

use alloc::vec;
use alloc::vec::Vec;

use crate::layout::*;
use crate::read::{Error, Volume};
use crate::{be32, checksum_ok, BlockSource};

/// A volume's allocation bitmap, read whole.
///
/// Held as the bits came off the disk — one bit per block, 1 meaning
/// free — with the accessors doing the inversion once so no caller has
/// to remember it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    reserved: u64,
    block_count: u64,
    valid: bool,
    /// Packed bits for blocks `reserved..block_count`, 1 = free, in the
    /// disk's LSB-first order.
    bits: Vec<u32>,
    pages: Vec<u64>,
    ext_blocks: Vec<u64>,
}

impl Bitmap {
    /// Is the root's `bitmap_flag` −1?
    ///
    /// **False means do not trust anything else in here.** The bits are
    /// still readable — a recovery tool wants to see what the interrupted
    /// update left behind — but they are not a statement about what is
    /// allocated, and an allocator that acts on them will hand out blocks
    /// a file is using.
    pub fn valid(&self) -> bool {
        self.valid
    }

    /// The first block the bitmap covers: the volume's `reserved` count,
    /// normally 2. Blocks below it are the boot blocks, which have no bit
    /// because they are not allocatable.
    pub fn first_block(&self) -> u64 {
        self.reserved
    }

    /// One past the last block the bitmap covers.
    pub fn end_block(&self) -> u64 {
        self.block_count
    }

    /// Does the bitmap have a bit for this block? False for the boot
    /// blocks and for anything past the end of the volume.
    pub fn covers(&self, lba: u64) -> bool {
        (self.reserved..self.block_count).contains(&lba)
            && (lba - self.reserved) / 32 < self.bits.len() as u64
    }

    /// Is `lba` marked free? `None` for a block the bitmap does not cover.
    pub fn is_free(&self, lba: u64) -> Option<bool> {
        if !self.covers(lba) {
            return None;
        }
        let i = lba - self.reserved;
        Some(self.bits[(i / 32) as usize] >> (i % 32) & 1 == 1)
    }

    /// Is `lba` marked allocated? `None` for a block the bitmap does not
    /// cover — which is deliberately not `Some(true)`: the boot blocks
    /// *are* in use, but the bitmap does not say so, and a validator
    /// needs to tell "in use" from "not answerable".
    pub fn is_allocated(&self, lba: u64) -> Option<bool> {
        self.is_free(lba).map(|free| !free)
    }

    /// Every block the bitmap marks allocated, ascending.
    pub fn allocated(&self) -> impl Iterator<Item = u64> + '_ {
        self.covered()
            .filter(move |&lba| self.is_free(lba) == Some(false))
    }

    /// Every block the bitmap marks free, ascending.
    pub fn free(&self) -> impl Iterator<Item = u64> + '_ {
        self.covered()
            .filter(move |&lba| self.is_free(lba) == Some(true))
    }

    /// Every block the bitmap has a bit for, ascending.
    pub fn covered(&self) -> impl Iterator<Item = u64> + '_ {
        let end = self
            .block_count
            .min(self.reserved + self.bits.len() as u64 * 32);
        (self.reserved..end).filter(move |&lba| self.covers(lba))
    }

    /// How many blocks are marked allocated.
    ///
    /// Counted by popcount over the words rather than by walking blocks,
    /// because on a 2 GB volume the difference is four million iterations.
    pub fn allocated_count(&self) -> u64 {
        self.covered_count() - self.free_count()
    }

    /// How many blocks are marked free.
    pub fn free_count(&self) -> u64 {
        let covered = self.covered_count();
        let mut n = 0u64;
        for (i, w) in self.bits.iter().enumerate() {
            let base = i as u64 * 32;
            // The last word may run past the end of the volume; those
            // bits are padding, whatever value they hold.
            let usable = covered.saturating_sub(base).min(32);
            if usable == 0 {
                break;
            }
            let mask = if usable == 32 {
                u32::MAX
            } else {
                (1u32 << usable) - 1
            };
            n += (w & mask).count_ones() as u64;
        }
        n
    }

    /// How many blocks the bitmap has bits for.
    pub fn covered_count(&self) -> u64 {
        self.block_count
            .saturating_sub(self.reserved)
            .min(self.bits.len() as u64 * 32)
    }

    /// Does the bitmap have enough pages to cover the whole volume? A
    /// short bitmap is not a corrupt one — it is a volume whose last
    /// blocks can never be allocated, which is a real (and reported)
    /// state rather than an error.
    pub fn covers_whole_volume(&self) -> bool {
        self.reserved + self.covered_count() >= self.block_count
    }

    /// The bitmap blocks themselves, in page order. They are allocated
    /// blocks like any other — and a validator that forgets to count them
    /// reachable reports every one as an orphan.
    pub fn pages(&self) -> &[u64] {
        &self.pages
    }

    /// The bitmap extension blocks walked to find the pages past the
    /// root's 25, in chain order.
    pub fn ext_blocks(&self) -> &[u64] {
        &self.ext_blocks
    }
}

impl<S: BlockSource> Volume<S> {
    /// Read the whole allocation bitmap: the root's 25 page pointers,
    /// then as many extension blocks as the volume's size needs.
    ///
    /// A zero page pointer ends the list — the format packs them from the
    /// front — so a volume needing fewer than 25 pages simply stops. Each
    /// page's checksum is verified at [`BITMAP_CHECKSUM_INDEX`], and a
    /// page whose checksum fails is an [`Error::Checksum`]: bits that do
    /// not sum are not bits to allocate from.
    ///
    /// An invalid `bitmap_flag` is **not** an error here. The bits are
    /// read and returned with [`Bitmap::valid`] false, because the state
    /// a recovery tool most needs to inspect is exactly the one an error
    /// return would hide.
    pub fn read_bitmap(&mut self) -> Result<Bitmap, Error<S::Error>> {
        let bs = self.block_size();
        let reserved = self.reserved();
        let block_count = self.block_count();
        let valid = self.root().bitmap_flag == -1;

        let mut pages: Vec<u64> = Vec::new();
        let mut ext_blocks: Vec<u64> = Vec::new();
        for &p in self.root().bitmap_pages.iter() {
            if p == 0 {
                break;
            }
            pages.push(p as u64);
        }

        // Only walk the extension chain if the root's pages were all
        // used: an extension pointer beside a half-empty root page list
        // is a leftover, and following it would invent coverage.
        let mut next = if pages.len() == BITMAP_PAGES {
            self.root().bitmap_ext
        } else {
            0
        };
        let per_ext = bitmap_ext_pointers(bs);
        let mut visited: Vec<u64> = Vec::new();
        while next != 0 {
            let lba = next as u64;
            self.guard_chain(&mut visited, lba)?;
            // A bitmap extension block has no type, no own key and no
            // checksum: there is nothing in it to verify, only pointers
            // to range-check as they are used.
            self.read_raw(lba)?;
            ext_blocks.push(lba);
            for i in 0..per_ext {
                let p = be32(&self.buf, i * 4);
                if p == 0 {
                    break;
                }
                pages.push(p as u64);
            }
            next = be32(&self.buf, bitmap_ext_next(bs));
        }

        let per_page = bitmap_bits_per_block(bs);
        let words_per_page = (bs - OFF_BITMAP_BITS) / 4;
        let need = block_count.saturating_sub(reserved);
        let mut bits: Vec<u32> =
            Vec::with_capacity((pages.len() * words_per_page).min((need / 32 + 1) as usize));
        for (i, &page) in pages.iter().enumerate() {
            // Stop reading pages the volume has no blocks for, however
            // many the root lists: a bitmap longer than the volume is a
            // formatting artefact, not coverage.
            if i as u64 * per_page >= need {
                break;
            }
            self.read_raw(page)?;
            if !checksum_ok(&self.buf) {
                return Err(Error::Checksum { lba: page });
            }
            for w in 0..words_per_page {
                bits.push(be32(&self.buf, OFF_BITMAP_BITS + w * 4));
            }
        }

        Ok(Bitmap {
            reserved,
            block_count,
            valid,
            bits,
            pages,
            ext_blocks,
        })
    }
}

/// Build a bitmap in memory from an explicit allocated set — the shape
/// [`Volume::validate`](crate::validate) compares against, and the shape
/// a formatter will write.
///
/// Present here rather than in a writer because the bit conventions
/// belong in one place: whatever writes a bitmap must invert, order and
/// offset exactly the way [`Bitmap`] reads, and the cheapest way to
/// guarantee that is for both to be this module's problem.
pub fn pack_bits(
    reserved: u64,
    block_count: u64,
    allocated: &[u64],
    block_size: usize,
) -> Vec<u32> {
    let words_per_page = (block_size - OFF_BITMAP_BITS) / 4;
    let per_page = bitmap_bits_per_block(block_size);
    let need = block_count.saturating_sub(reserved);
    let pages = (need / per_page + u64::from(need % per_page != 0)) as usize;
    // Every bit starts free, which is also what the padding bits past the
    // end of the volume are left as.
    let mut words = vec![u32::MAX; pages * words_per_page];
    for &lba in allocated {
        if lba < reserved {
            continue;
        }
        let i = lba - reserved;
        if let Some(w) = words.get_mut((i / 32) as usize) {
            *w &= !(1u32 << (i % 32));
        }
    }
    words
}
