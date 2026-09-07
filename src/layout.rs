//! On-disk block layout: the offsets, and *why* they are where they are.
//!
//! Every FFS metadata block is split in two halves that grow towards
//! each other. The head — type, own key, sequence, size, first data,
//! checksum, then the hash or data-pointer table — is indexed from
//! longword 0 forward. The tail — dates, name, comment, protection,
//! hash chain, secondary type — is indexed from the *end* of the block,
//! because the table in the middle changes length with the block size
//! (`block_size/4 - 56` entries) and everything after it would otherwise
//! move. Hence [`tail`]: a field at "longword −20" lives at
//! `block_size - 80`, on a 512-byte block and on a 32 KB one alike.
//!
//! The two halves meet exactly: the head occupies `24 + 4*(bs/4 - 56)`
//! = `bs - 200` bytes, and the first tail field ([`TL_BITMAP_FLAG`],
//! longword −50) starts at `bs - 200`. Any block size from 512 to 32768
//! keeps that identity, which is the whole reason the format indexes the
//! tail backwards.
//!
//! # Two name layouts
//!
//! Classic variants (`DOS\0`–`DOS\5`) store the comment as a BCPL string
//! at longword −46 (80 bytes used) and the name as a BCPL string at
//! longword −20 (32 bytes, 30 characters usable), with the entry's
//! DateStamp between them at −23.
//!
//! Long-name variants (`DOS\6`/`DOS\7`, "LNFS", added with the OS 3.1.4
//! / 3.2-era FastFileSystem reimplementation) merge those into one
//! 112-byte field at longword −46, [`TL_NAC`] — *N*ame *a*nd *C*omment,
//! two BCPL strings back to back — and push the DateStamp down to
//! longword −15, freeing −18 for a [`TL_COMMENT_BLOCK`] pointer used
//! when the comment no longer fits beside a long name.
//!
//! This is the trap the crate exists to not fall into. A reader using
//! the classic name offset on an LNFS volume reads byte 104 of `NaC` as
//! a length byte — padding, so zero — and reports **every entry's name
//! as empty**. Two independent implementations do exactly that. The
//! layouts below are transcribed from the AmigaOS `LongName*Block`
//! structure definitions, and the field-by-field arithmetic that proves
//! each offset is in this module's tests.

/// `T_HEADER` / `TYPE_SHORT`: root, directory, file-header and hard-link
/// blocks all carry this in longword 0.
pub const T_HEADER: u32 = 2;
/// `T_LIST`: file extension (file-list) blocks.
pub const T_LIST: u32 = 16;
/// `T_DATA`: OFS data blocks (FFS data blocks have no header at all).
pub const T_DATA: u32 = 8;
/// `T_DIRCACHE`: `DOS\4`/`DOS\5` directory-cache blocks.
pub const T_DIRCACHE: u32 = 33;
/// `TYPE_COMMENT`: the LNFS overflow-comment block referenced by
/// [`TL_COMMENT_BLOCK`].
pub const T_COMMENT: u32 = 64;

/// Secondary type: the volume root.
pub const ST_ROOT: i32 = 1;
/// Secondary type: a directory.
pub const ST_USERDIR: i32 = 2;
/// Secondary type: a soft link (a stored path, resolved by the caller).
pub const ST_SOFTLINK: i32 = 3;
/// Secondary type: a hard link to a directory.
pub const ST_LINKDIR: i32 = 4;
/// Secondary type: a file. Negative, as the FileInfoBlock contract
/// requires — `ST_FILE` is `-3`, not `0xFFFFFFFD` by accident.
pub const ST_FILE: i32 = -3;
/// Secondary type: a hard link to a file.
pub const ST_LINKFILE: i32 = -4;

// --- head, indexed forward from longword 0 -------------------------------

/// Longword 0: primary block type.
pub const OFF_TYPE: usize = 0;
/// Longword 1: the block's own number (0 in the root).
pub const OFF_OWN_KEY: usize = 4;
/// Longword 2: high sequence number — data-pointer count in a file
/// header, unused (0) in a directory or the root.
pub const OFF_HIGH_SEQ: usize = 8;
/// Longword 3: hash-table size in the root, unused elsewhere.
pub const OFF_HASH_TABLE_SIZE: usize = 12;
/// Longword 4: first data block, unused (0) in the root.
pub const OFF_FIRST_DATA: usize = 16;
/// Longword 5: the block checksum.
pub const OFF_CHECKSUM: usize = 20;
/// The checksum's longword index, for [`crate::checksum_compute`].
pub const CHECKSUM_INDEX: usize = 5;
/// Longword 6: the first hash-table slot (or data-pointer slot).
pub const OFF_HASH_TABLE: usize = 24;

// --- tail, indexed backward from the end ---------------------------------

/// Root only, longword −50: bitmap valid flag (−1 valid, 0 invalid).
pub const TL_BITMAP_FLAG: usize = 50;
/// Root only, longword −49: the first of [`BITMAP_PAGES`] bitmap block
/// pointers (−49..=−25).
pub const TL_BITMAP_PAGES: usize = 49;
/// How many bitmap block pointers the root itself holds.
pub const BITMAP_PAGES: usize = 25;
/// Root only, longword −24: pointer to a bitmap-extension block.
///
/// Occupying the same longword that a directory uses for
/// [`TL_PROTECTION`] is the historical reason old filesystems capped
/// partitions near 53 MB: they wrote protection bits into the root and
/// clobbered this pointer.
pub const TL_BITMAP_EXT: usize = 24;
/// Root only, longword −23: date the root directory was last altered.
pub const TL_ROOT_DIR_ALTERED: usize = 23;
/// Root only, longword −20: the volume name, BCPL, 30 characters max —
/// *including* on LNFS volumes, where the root name field does not move
/// and the 30-character limit still applies.
pub const TL_ROOT_NAME: usize = 20;
/// Root, LNFS only, longword −11: blocks allocated per the bitmap.
/// Classic roots have the volume name's padding here.
pub const TL_ROOT_NUM_BLOCKS_USED: usize = 11;
/// Root only, longword −10: date any part of the volume was last
/// altered.
pub const TL_ROOT_DISK_ALTERED: usize = 10;
/// Root only, longword −7: date the volume was formatted.
pub const TL_ROOT_DISK_MADE: usize = 7;
/// Root, LNFS only, longword −4: the volume's own dostype, repeating
/// the boot block's signature. Zero on every non-LNFS variant — which
/// makes a non-zero value here a free cross-check that the dostype the
/// caller was handed matches the volume actually on disk.
pub const TL_ROOT_FS_TYPE: usize = 4;

/// Longword −49: owner UID (high word) and GID (low word), as one raw
/// longword. Zero on plain FFS; meaningful under muFS.
pub const TL_OWNER: usize = 49;
/// Longword −48: the full 32-bit protection long, group and other bits
/// included.
pub const TL_PROTECTION: usize = 48;
/// File headers only, longword −47: the file's length in bytes.
pub const TL_BYTE_SIZE: usize = 47;
/// Classic layout, longword −46: the comment, BCPL, 79 characters max.
pub const TL_COMMENT: usize = 46;
/// Longest comment AmigaDOS will store.
pub const COMMENT_MAX: usize = 79;
/// Classic layout, longword −23: the entry's DateStamp.
pub const TL_DATE: usize = 23;
/// Classic layout, longword −20: the entry name, BCPL, 30 characters.
pub const TL_NAME: usize = 20;

/// LNFS layout, longword −46: the merged name-and-comment field, two
/// BCPL strings laid end to end in [`NAC_LEN`] bytes. Spans longwords
/// −46..=−19 inclusive (28 longwords), swallowing the classic comment
/// field, the spare longwords, the classic DateStamp *and* the classic
/// name field.
pub const TL_NAC: usize = 46;
/// The merged field's length. 112 bytes holds a 107-byte name
/// (`1 + 107`) with four bytes left for a short comment's length byte
/// and up to three characters; longer comments go to a
/// [`TL_COMMENT_BLOCK`].
pub const NAC_LEN: usize = 112;
/// LNFS layout, longword −18: block number of an overflow comment
/// block ([`T_COMMENT`]), or 0.
pub const TL_COMMENT_BLOCK: usize = 18;
/// LNFS layout, longword −15: the entry's DateStamp, moved down from
/// −23 to make room for [`TL_NAC`].
pub const TL_DATE_LONG: usize = 15;

/// Longword −10: first hard link pointing at this entry (`next_link` in
/// the classic naming). Same place in both layouts.
pub const TL_NEXT_LINK: usize = 10;
/// Longword −4: next entry in this hash chain, 0 at the end. Zero in a
/// root block, which is nobody's directory entry.
pub const TL_HASH_CHAIN: usize = 4;
/// Longword −3: the parent directory's block. Zero in a root block.
pub const TL_PARENT: usize = 3;
/// Longword −2: extension — the first file-extension block for a file,
/// the directory-cache list for a directory or the root.
pub const TL_EXTENSION: usize = 2;
/// Longword −1: the secondary type (`ST_*`).
pub const TL_SECONDARY_TYPE: usize = 1;

/// Smallest filesystem block size the format allows.
pub const MIN_BLOCK_SIZE: usize = 512;
/// Largest filesystem block size the format allows.
pub const MAX_BLOCK_SIZE: usize = 32768;

/// Byte offset of the field `lw_from_end` longwords before the end of a
/// `block_size`-byte block: `TL_NAME` (20) on 512 bytes is byte 432, on
/// 4096 bytes byte 4016.
#[inline]
pub fn tail(block_size: usize, lw_from_end: usize) -> usize {
    block_size - lw_from_end * 4
}

/// Is `block_size` one this crate will parse: a power of two in
/// 512..=32768? Anything else has no defined hash-table size, and
/// guessing one produces a filesystem that mostly works.
#[inline]
pub fn block_size_ok(block_size: usize) -> bool {
    (MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&block_size) && block_size.is_power_of_two()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_table_size;

    /// Walk the published structure definitions field by field and check
    /// that every offset above falls where the C layout puts it. This is
    /// the arithmetic a reviewer would otherwise have to do by hand, and
    /// the only defence against a transcription slip in a constant.
    #[test]
    fn classic_directory_fields_match_the_c_layout() {
        // struct UserDirectoryBlock, 512-byte block:
        //   LONG Type, OwnKey, Reserved1..3, Checksum   6 longs -> 24
        //   ULONG HashTable[72]                         288     -> 312
        //   Reserved4, OwnerXID, Protection, Reserved6  16      -> 328
        //   char Comment[92]                            92      -> 420
        //   ULONG Created[3]                            12      -> 432
        //   char DirName[36]                            36      -> 468
        //   LONG Reserved7[7]                           28      -> 496
        //   HashChain, Parent, Reserved8, SecondaryType 16      -> 512
        let bs = 512;
        assert_eq!(OFF_HASH_TABLE, 24);
        assert_eq!(OFF_HASH_TABLE + 4 * hash_table_size(bs) as usize, 312);
        assert_eq!(tail(bs, TL_OWNER), 316);
        assert_eq!(tail(bs, TL_PROTECTION), 320);
        assert_eq!(tail(bs, TL_BYTE_SIZE), 324);
        assert_eq!(tail(bs, TL_COMMENT), 328);
        assert_eq!(tail(bs, TL_DATE), 420);
        assert_eq!(tail(bs, TL_NAME), 432);
        assert_eq!(tail(bs, TL_NEXT_LINK), 472);
        assert_eq!(tail(bs, TL_HASH_CHAIN), 496);
        assert_eq!(tail(bs, TL_PARENT), 500);
        assert_eq!(tail(bs, TL_EXTENSION), 504);
        assert_eq!(tail(bs, TL_SECONDARY_TYPE), 508);
    }

    #[test]
    fn lnfs_directory_fields_match_the_c_layout() {
        // struct LongNameUserDirectoryBlock, 512-byte block:
        //   head + HashTable[72]                        312
        //   Spare2, Owner/GroupID, Protection, Spare3   16      -> 328
        //   char NaC[112]                               112     -> 440
        //   ULONG CommentBlock                          4       -> 444
        //   LONG Spare4[2]                              8       -> 452
        //   ULONG Created[3]                            12      -> 464
        //   LONG Spare5[2]                              8       -> 472
        //   ULONG FirstLink                             4       -> 476
        //   LONG Spare6[5]                              20      -> 496
        //   HashChain, Parent, DirList, SecondaryType   16      -> 512
        let bs = 512;
        assert_eq!(tail(bs, TL_NAC), 328);
        assert_eq!(tail(bs, TL_NAC) + NAC_LEN, 440);
        assert_eq!(tail(bs, TL_COMMENT_BLOCK), 440);
        assert_eq!(tail(bs, TL_DATE_LONG), 452);
        assert_eq!(tail(bs, TL_NEXT_LINK), 472);
        assert_eq!(tail(bs, TL_HASH_CHAIN), 496);
        // The merged field swallows the classic comment, date and name
        // fields whole: -46..=-19 inclusive, 28 longwords.
        assert!(tail(bs, TL_NAC) <= tail(bs, TL_NAME));
        assert!(tail(bs, TL_NAME) < tail(bs, TL_NAC) + NAC_LEN);
        // And a 107-byte name plus its length byte still leaves room for
        // a comment's length byte.
        const _: () = assert!(1 + crate::MAX_NAME_LONG < NAC_LEN);
    }

    #[test]
    fn root_fields_match_the_c_layout() {
        // struct RootBlock: head + HashTable[72] = 312, BitmapFlag,
        // BitmapKeys[25] = 100, BitmapExtend, DirAltered[3], Name[..],
        // then (LNFS) Reserved1, NumBlocksUsed, DiskAltered[3],
        // DiskMade[3], FileSystemType, Reserved2, DirList, SecondaryType.
        let bs = 512;
        assert_eq!(tail(bs, TL_BITMAP_FLAG), 312);
        assert_eq!(tail(bs, TL_BITMAP_PAGES), 316);
        assert_eq!(tail(bs, TL_BITMAP_PAGES) + 4 * BITMAP_PAGES, 416);
        assert_eq!(tail(bs, TL_BITMAP_EXT), 416);
        assert_eq!(tail(bs, TL_ROOT_DIR_ALTERED), 420);
        assert_eq!(tail(bs, TL_ROOT_NAME), 432);
        assert_eq!(tail(bs, TL_ROOT_NUM_BLOCKS_USED), 468);
        assert_eq!(tail(bs, TL_ROOT_DISK_ALTERED), 472);
        assert_eq!(tail(bs, TL_ROOT_DISK_MADE), 484);
        assert_eq!(tail(bs, TL_ROOT_FS_TYPE), 496);
        assert_eq!(tail(bs, TL_SECONDARY_TYPE), 508);
    }

    #[test]
    fn head_and_tail_meet_exactly_at_every_block_size() {
        let mut bs = MIN_BLOCK_SIZE;
        while bs <= MAX_BLOCK_SIZE {
            assert!(block_size_ok(bs));
            let head_end = OFF_HASH_TABLE + 4 * hash_table_size(bs) as usize;
            assert_eq!(head_end, tail(bs, TL_BITMAP_FLAG), "block size {bs}");
            assert_eq!(head_end, bs - 200);
            bs *= 2;
        }
        assert!(!block_size_ok(256));
        assert!(!block_size_ok(65536));
        assert!(!block_size_ok(1000));
    }
}
