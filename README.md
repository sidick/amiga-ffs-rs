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

**Creating a volume works** (milestone 2): `BlockSink` is the write seam
— a second trait, so a read-only source is never asked for a
`write_block` it cannot have — and `format()` lays down a fresh, empty,
valid volume of any variant at any block size: boot block, root, bitmap
with its extension blocks, and the empty dircache block a `DOS\4`/`DOS\5`
root carries from birth. xdftool mounts what it writes.

**Filling one works too**: `Populator` creates directories and files with
their metadata — every name layout, comments including the `T_COMMENT`
overflow block, OFS and FFS data blocks, `T_LIST` extension blocks and
`DOS\4`/`DOS\5` dircache maintenance — and `populate_from_tree()` turns a
host directory into an image in one call. xdftool lists, reads back and
writes into what it produces, on all eight variants.

```rust
let disk = /* anything that is both a BlockSource and a BlockSink */;
let opts = FormatOptions::new(Variant::FfsIntl, 1760, b"Workbench");
let disk = amiga_ffs::populate::populate_from_tree(disk, &opts, Path::new("./tree"))?;
```

`Populator` is append-only, over a volume this crate just formatted;
**mutating** a volume that already has something in it — delete, rename,
truncate, append — is milestone 3. See `PLAN.md`.

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
