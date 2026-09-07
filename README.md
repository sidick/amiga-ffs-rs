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

Foundations: the `BlockSource` seam (runtime block size, `u64` LBAs,
typed errors), the DOS-type/variant model, both checksum algorithms,
BCPL strings, and both name-hash case-folding tables — the primitives
where a subtle mistake produces a filesystem that *mostly* works, so
they come first, with tests. Directory and file reading are next; see
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
