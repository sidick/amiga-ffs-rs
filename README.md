# amiga-ffs

The Amiga Fast File System (and its OFS ancestor) as a pure-Rust,
permissively-licensed library: every `DOS\0`–`DOS\7` variant, including
the `DOS\6`/`DOS\7` long-filename layouts that classic-offset readers
silently misread.

The filesystem half of a deliberate split — partition tables live in
[amiga-rdb](https://github.com/sidick/amiga-rdb-rs), one filesystem
family per crate, composed by the consumer through a block-device seam.
Nothing here knows what an RDB is; a volume is a run of blocks.

`no_std` + `alloc` at the core; the `std` feature (default) adds only
conveniences. No dependencies. MSRV 1.63.

## Status

**Reading is complete** (milestone 1): root blocks, directory traversal
under both fold tables, the `DOS\6`/`DOS\7` long-name layout, file data
through FFS chains and OFS data blocks, hard and soft links, metadata
(protection, owner, dates), `DOS\4`/`DOS\5` dircache blocks (read and
marked advisory — the hash chains stay authoritative), the allocation
bitmap, and a `validate()` that walks the whole volume and *reports*
rather than refusing, so a damaged volume still yields everything still
reachable.

**Creating a volume works** (milestone 2, wave 1): `BlockSink` is the
write seam — a second trait, so a read-only source is never asked for a
`write_block` it cannot have — and `format()` lays down a fresh, empty,
valid volume of any variant at any block size: boot block, root, bitmap
with its extension blocks, and the empty dircache block a `DOS\4`/`DOS\5`
root carries from birth. xdftool mounts what it writes.

Populating a volume from a host tree, allocating from the bitmap and
mutating directories are the rest of milestone 2 and milestone 3; see
`PLAN.md`.

## Why this exists

There is no permissively-licensed *writable* FFS implementation in any
language usable as a library: amitools is GPL-2, emulators' are GPL,
affs-read (MIT) is read-only and misreads long-filename volumes. This
crate is MIT OR Apache-2.0 so that emulators, image-building tools and
hobby OS projects can all use it, whatever their own licence.

Written against the layouts documented in the AmigaOS NDK and the
FFS/AFFS format literature, and tested differentially against
independent implementations (xdftool, affs-read, and a real Amiga ROM
filesystem driving the same images).

## License

Dual-licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
