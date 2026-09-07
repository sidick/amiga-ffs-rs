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

**Landed** (commits `db33576`, `76989f3`, `a837b68`): the read side is
complete — every variant, every block size, 82 tests green on stable
and MSRV across both feature sets, and the xdftool differential leg
passing on all eight variants. One box below stays open for the parts
of the differential suite that need more than `cargo test` can reach.

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
- [x] **File reading**: FFS data-block chains via file-header block
      lists and extension blocks; OFS data blocks with their headers
      (and use those headers to *verify*, since they're there).
- [x] **Metadata**: protection bits — the full 32-bit long, including
      the group/other RWED bits — comments, dates (ticks since
      1978-01-01, the 1900-leap-year rules), and the owner longword
      (UID/GID). Owner and the extended bits are first-class, not
      "where present": muFS is in scope (below), and plain FFS carries
      the same fields zeroed.
- [x] **Hard/soft links**: link chains resolved, loops refused.
- [x] **Dircache blocks** (`DOS\4`/`DOS\5`): read them, but treat the
      hash chains as authoritative — caches go stale, and a reader
      that trusts a stale cache invents a directory that isn't there.
      `read_dircache()` is documented advisory and nothing in the crate
      resolves a name through it; `validate()` compares cache against
      chains and reports the six ways they can disagree. Record layout
      (three *words* of DateStamp, a signed type byte, two counted
      strings, word-aligned) confirmed byte for byte against `DOS\5`
      images built by xdftool. The pointer is longword −2, the field a
      file header uses for its extension chain — no collision, because
      the meaning follows the secondary type.
- [x] **Bitmap**: read and verify (block-in-use vs reachable-from-root)
      — the read-side half of `validate()`, and the foundation the
      allocator will stand on. Four inversion-prone conventions each
      confirmed against xdftool-built images: **1 = free**, LSB-first
      within each big-endian longword, first bit is block `reserved`,
      and the checksum is longword **0** rather than 5. Bitmap extension
      blocks are bare pointer arrays — no type, no own key, no checksum
      — with the last longword chaining onward. `bitmap_flag == 0` is
      surfaced (`Bitmap::valid()`), never silently trusted.
- [x] **`validate()`**: checksums, hash-chain membership matches hash
      of name, bitmap consistency, orphan blocks. Parse damaged volumes
      where possible — recovery needs the read side most of all. Returns
      a `Report` of typed `Finding`s rather than erroring: the walk
      records and continues, so a directory with one corrupt chain still
      yields the other seventy-one. Orphans (leaked) and
      reachable-but-free (double-allocation risk) are reported as
      *different* findings, because the mutation ordering in milestone 3
      exists precisely to fail in the first direction and not the second.
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

      **Partly landed**, so the box stays open. `tests/differential.rs`
      does the xdftool leg: it generates all eight `DOS\0`–`DOS\7`
      variants at test time (nothing checked in — a committed ADF is a
      blob nobody can review) and reads each back through this crate —
      whole tree, every byte of every file including one that crosses an
      extension block, dircache against the chains, bitmap against
      xdftool's own `info` accounting, and `validate()` clean on all
      eight. It skips with a printed reason when xdftool is absent, so
      it is never a build dependency.

      What remains, and why it is not done rather than merely not done
      yet:

      - **affs-read and AROS `afs.handler` as oracles.** xdftool is the
        leg that catches the mistakes this crate and its own synthetic
        builder would make *together*; the other two matter for
        understanding a disagreement once there is one. `afs.handler`
        additionally needs a guest to run in, which is Copperline's seam
        and not something `cargo test` can reach.
      - **Block sizes other than 512.** xdftool's ADF and HDF images are
        512-blocked; larger block sizes live behind an RDB, which is
        `rdbtool`'s and amiga-rdb's territory. Covered synthetically at
        512/1024/4096 in `tests/volumes.rs` meanwhile.
      - **Comments.** xdftool's `comment` command raises a `TypeError`
        before writing anything (amitools 0.7.x), so no oracle-written
        volume can carry one. Covered synthetically in both layouts,
        `T_COMMENT` overflow block included.
      - **The amibake AROS `DOS\7` fixture** — a real-world image rather
        than a generated one. Wanted; needs a fixture pipeline, not just
        a test.

## Milestone 2 — create

**Next up.** Write code with nothing to corrupt: format a fresh volume, populate
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

- **Resize** (grow/shrink an existing volume in place): gated on the
  M3 allocator and mutation discipline. The filesystem half only — the
  RDB's `high_cyl` move is amiga-rdb's, composed by the consumer. The
  algorithm exists in shipping form in AmiPart's `ffsresize.c` (John
  Hertell, MIT — licence-compatible; port the algorithm, cite the
  source): FFS recomputes the root LBA from geometry on every mount, so
  the root must *move* to the new midpoint — copy root, free the old
  block, re-parent the root's direct children (their parent longword
  names the root; deeper entries and hard-link `real_entry` pointers
  are unaffected because no child header moves). Grow: new bitmap
  pages at natural positions (`reserved + N × bits-per-page`), root
  last, read-back verification. Shrink: refuse unless every allocated
  block past the new end is movable metadata; a read-only minimum-size
  estimate falls out of `Bitmap` already. Do better than AmiPart
  where it punts: rebuild the bitmap ourselves instead of stamping
  `bm_flag = 0` for FFS to fix on mount, handle dircache chains and
  LNFS root fields, and support all block sizes, not just 512/1024.
  AmiPart's code independently confirms wave 3's bitmap conventions
  (LSB-first, `(block − reserved)` indexing, checksum longword 0,
  checksum-less ext blocks) and the `canonical_root_lba` formula.

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

- [x] Errors: `Display` everywhere, `std::error::Error` under `std` —
      `Error<E>` and every `validate()` `Finding` state their
      consequence, and the transport error survives via `source()`
- [ ] Fuzzing: parse arbitrary volumes without panic; fuzz the hash
      chains and name lengths specifically
- [ ] CI: stable + MSRV 1.63, `--no-default-features`, clippy
      `-D warnings`, rustfmt, docs; differential job where fixtures
      are buildable
- [ ] crates.io publish at read-complete; `#![deny(missing_docs)]`
      once the surface settles. Read-complete is *reached* — the gate
      now is deciding the surface is one worth freezing, which is
      worth a pass over the API after M2 shapes the write side

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
