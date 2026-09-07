# amiga-ffs

[![crates.io](https://img.shields.io/crates/v/amiga-ffs.svg)](https://crates.io/crates/amiga-ffs)
[![docs.rs](https://docs.rs/amiga-ffs/badge.svg)](https://docs.rs/amiga-ffs)
[![CI](https://github.com/sidick/amiga-ffs-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/sidick/amiga-ffs-rs/actions)

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

```sh
cargo add amiga-ffs
```

## Status

**Complete: read, create, and mutate** — the plan's three milestones
are landed and released, each closed by an oracle that is not this
crate: xdftool driving the same images for structure, and a real
Kickstart 3.1 under deterministic emulation for behaviour — mounting
volumes this crate built, reading every byte through the ROM's own FFS
handler, mutating a volume in place, and agreeing entry-for-entry with
the same operations replayed through this crate.

**Reading** (milestone 1): root blocks, directory traversal
under both fold tables, the `DOS\6`/`DOS\7` long-name layout, file data
through FFS chains and OFS data blocks, hard and soft links, metadata
(protection, owner, dates), `DOS\4`/`DOS\5` dircache blocks (read and
marked advisory — the hash chains stay authoritative), the allocation
bitmap, and a `validate()` that walks the whole volume and *reports*
rather than refusing, so a damaged volume still yields everything still
reachable.

**Creating** (milestone 2): `BlockSink` is the write seam
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

`Populator` is append-only, over a volume this crate just formatted.
**Changing a volume that already exists** (milestone 3) is the rest:
`Allocator` hands out blocks from a volume's own bitmap with the
mark-then-use ordering in the types (an allocation is not a block number
until its bitmap page is on the disk), `Volume::repair()` rebuilds a
bitmap the way the ROM's disk-validator does, and `Mutator` creates,
deletes, renames and re-describes entries — hash chains spliced under both
fold tables, `T_COMMENT` blocks moving in and out as an LNFS name grows,
`DOS\4`/`DOS\5` dircaches regenerated from the chains they cache, and a
write order whose worst crash outcome is a leaked block rather than a
block with two owners. `write_file`, `append` and `truncate` change a
file's contents in place, committing every size change in the single
header-block write that carries the length, the block count and the
pointer table together; `Volume::read_range` is the read half, touching
only the blocks a range covers. `PLAN.md` is the full map, including
what is in scope but not yet scheduled (in-place resize, the muFS
dostype survey, notes for a FUSE adapter).

```rust
let mut m = Mutator::open(Volume::open(disk, None)?)?;
let dir = m.create_dir(root, b"Devs", &Metadata::new())?;
m.create_file(dir, b"system-configuration", &Metadata::new(), &bytes)?;
m.rename(root, b"Old", dir, b"New")?;
m.delete(root, b"Doomed")?;
m.append(dir, b"system-configuration", b"...more bytes")?;
m.truncate(dir, b"system-configuration", 232)?;
```

## Why this exists

There is no permissively-licensed *writable* FFS implementation in any
language usable as a library: amitools is GPL-2, emulators' are GPL,
affs-read (MIT) is read-only and misreads long-filename volumes. This
crate is MIT OR Apache-2.0 so that emulators, image-building tools and
hobby OS projects can all use it, whatever their own licence.

Written against the layouts documented in the AmigaOS NDK and the
FFS/AFFS format literature, tested differentially against independent
implementations (xdftool on every variant, and a real Amiga ROM's FFS
mounting, reading and mutating the same images under deterministic
emulation), fuzzed, and CI-checked across stable and MSRV on both
feature sets.

## License

Dual-licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
