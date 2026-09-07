# Implementation plan

A complete FFS/OFS implementation — read, create, mutate — staged by
risk. Boxes get ticked as they land; anything discovered missing gets
added here first so the plan stays the map. Companion to amiga-rdb's
plan, which owns everything outside the partition.

## Already landed (0.1)

- [x] `BlockSource`: runtime block size, `u64` LBAs, typed errors,
      `&mut self` — each choice deliberate, each confirmed by watching
      another implementation choose differently and pay for it
- [x] `Variant`: the eight `DOS\0`–`DOS\7` types with their axes
      (FFS/OFS, intl, dircache, long names); unknown dostypes refused
- [x] Both checksum algorithms (standard zero-sum at any block size;
      boot block's end-around-carry) with round-trip tests
- [x] Both case-folding tables (classic ASCII-only vs intl Latin-1,
      with the `÷`/`ÿ` exceptions) and the name-hash function,
      hash-table sizing by block size
- [x] BCPL strings, hostile length bytes clamped

## Milestone 1 — read

- [x] **Root block**: locate (from geometry / reserved blocks), parse
      name, dates, hash table, bitmap pointers. Detect the variant from
      the *partition's* dostype but verify against what the volume
      actually contains — mismatches exist and must be diagnosable.
- [x] **Directory traversal**: hash-chain walk for lookup by name
      (both fold tables), full enumeration for listing. Classic
      entries first.
- [x] **Long-filename entries** (`DOS\6`/`DOS\7`): the different
      header-block name layout. **Regression to assert from day one:
      every name on a long-name volume non-empty** — the exact failure
      now observed in *two* independent readers against the same
      fixture: affs-read, and AROS's own `afs.handler`, which masks the
      dostype low byte away, mounts a `DOS\7` volume as classic FFS,
      reads the volume name (classic offset in both layouts), and
      resolves no directory entry. Accepting a variant's dostype
      without implementing its layout is this format's signature trap;
      this crate refuses what it cannot parse instead.
- [ ] **File reading**: FFS data-block chains via file-header block
      lists and extension blocks; OFS data blocks with their headers
      (and use those headers to *verify*, since they're there).
- [ ] **Metadata**: protection bits — the full 32-bit long, including
      the group/other RWED bits — comments, dates (ticks since
      1978-01-01, the 1900-leap-year rules), and the owner longword
      (UID/GID). Owner and the extended bits are first-class, not
      "where present": muFS is in scope (below), and plain FFS carries
      the same fields zeroed.
- [ ] **Hard/soft links**: link chains resolved, loops refused.
- [ ] **Dircache blocks** (`DOS\4`/`DOS\5`): read them, but treat the
      hash chains as authoritative — caches go stale, and a reader
      that trusts a stale cache invents a directory that isn't there.
- [ ] **Bitmap**: read and verify (block-in-use vs reachable-from-root)
      — the read-side half of `validate()`, and the foundation the
      allocator will stand on.
- [ ] **`validate()`**: checksums, hash-chain membership matches hash
      of name, bitmap consistency, orphan blocks. Parse damaged volumes
      where possible — recovery needs the read side most of all.
- [ ] **Differential suite**: same tree read through this crate,
      xdftool (GPL oracle — run, never copy), affs-read (MIT —
      readable when outputs disagree), and AROS's `afs.handler`
      (readable for understanding, licence-incompatible for copying —
      but it runs *inside a guest* against the same images, which
      makes it the one oracle that is also a real consumer). Its
      `getHashKey` already confirms our hash structurally: seed with
      length, `*13 + fold(c) & 0x7FF`, modulo table size, flags
      selecting the fold table. Fixtures from amibake images
      (redistributable AROS DOS\7 included) plus synthetic minimal
      volumes per variant, mixed block sizes 512..=32K.

## Milestone 2 — create

Write code with nothing to corrupt: format a fresh volume, populate
from a host tree, read it straight back. The API Copperline (dynamic
OFS/FFS drives from directories) and amibake (dir→hdf) actually want.

- [ ] **`BlockSink`** mirroring `BlockSource` (align the shape with
      whatever amiga-rdb settles on — same seam, same decisions).
- [ ] **Format**: boot block (valid checksum, non-bootable is fine),
      root block, bitmap covering the volume, for every variant and
      block size. What `Format` does, minus the icon.
- [ ] **Populate from tree**: files, directories, metadata mapping
      (host mtime → ticks, mode → protection bits), name validation
      per variant (30 vs 107 bytes, Latin-1, no `:`/`/`).
- [ ] **Round-trip property tests**: create → read back → tree-equal,
      for every variant × block size; then create → xdftool reads it →
      trees match — the independent-implementation proof.
- [ ] **Guest-mount proof**: an image created here boots/mounts under a
      real Amiga ROM (the consumer that cannot be argued with).

## Milestone 3 — mutate

Gated on the milestone-1 validator and differential suite.

- [ ] **Allocator**: bitmap-based block allocation with the volume's
      own policy quirks documented as discovered.
- [ ] **Create/delete/rename** in existing volumes: hash-chain
      insertion/removal under both fold tables, dircache invalidation
      (`DOS\4`/`5`: update or clear, never leave stale).
- [ ] **File write/append/truncate**: FFS chains and OFS headers both.
- [ ] **Crash-shape discipline**: data blocks before metadata, chain
      pointers flipped last, bitmap updated in an order that at worst
      *leaks* blocks (validator-recoverable) rather than double-uses
      them. The format has no journal; ordering is all there is.
- [ ] **Differential mutation tests**: same operation sequence applied
      through this crate and through the guest's own filesystem on a
      copy; resulting volumes must agree (allowing documented
      don't-care fields — dates, allocation order).

## In scope, not scheduled

- **muFS**: the MultiUser filesystem is explicitly in scope for this
  crate — it is not another family but FFS with the owner field and
  extended permission bits actually used and enforced; same blocks,
  same hashing, same chains. Independently confirmed by AROS's
  `afs.handler`, which mounts exactly two dostype families: `DOS` and
  `muFS`. Deferred, not excluded: no milestone
  depends on it, and the metadata work above (owner longword and full
  protection long read as first-class on *every* variant) means adding
  it later is dostype acceptance plus a survey, not a rework. That
  survey — which dostype values real muFS volumes carry (`muFS` =
  0x6D754653 is documented; whether per-variant `muF\x` forms exist in
  the wild) — happens when the add-on does. Enforcement semantics stay
  with the consumer either way: the crate reports ownership, it doesn't
  police it.

## Cross-cutting

- [ ] Errors: `Display` everywhere, `std::error::Error` under `std`
- [ ] Fuzzing: parse arbitrary volumes without panic; fuzz the hash
      chains and name lengths specifically
- [ ] CI: stable + MSRV 1.63, `--no-default-features`, clippy
      `-D warnings`, rustfmt, docs; differential job where fixtures
      are buildable
- [ ] crates.io publish at read-complete; `#![deny(missing_docs)]`
      once the surface settles

## Non-goals

- **Partition tables** — amiga-rdb's job, composed via the seam.
- **Other filesystem families** (PFS3, SFS) — own crates, per the
  one-family-per-crate rule. (muFS is *not* on this list: it is FFS
  with the owner/permission fields used, and is in scope — see
  milestone 1.)
- **Hunk loading, bootblock execution** — bytes in, bytes out.
- **A VFS/handler layer** — DosPacket semantics live in the consumer
  (m68k-machine's transport card, a FUSE wrapper, whatever); this
  crate is the format, not the filesystem *service*.
