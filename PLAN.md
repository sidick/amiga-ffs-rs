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

**Landed.** Write code with nothing to corrupt: format a fresh volume,
populate from a host tree, read it straight back. The API Copperline
(dynamic OFS/FFS drives from directories) and amibake (dir→hdf) actually
want.

Wave 1 was the seam, `format()` and CI; wave 2 `Populator`, the
round-trip property tests and the fuzzers; the guest-mount proof closed
it under a real Kickstart 3.1.

- [x] **`BlockSink`** mirroring `BlockSource` — the shape amiga-rdb has
      already settled on, adopted verbatim: a *second* trait, not a
      bound on `BlockSource` and not `BlockSink: BlockSource`, because
      read-only sources are the common case and one trait would force
      every one of them to supply a `write_block` that can only fail at
      runtime. `format()` takes `BlockSink` alone (it writes and never
      reads); anything needing both says `S: BlockSource + BlockSink`,
      which is what the test images are.
- [x] **Format**: boot block, root block, bitmap covering the volume, for
      every variant and block size — `format()` in `src/format.rs`, with
      the three placement decisions each settled against an
      xdftool-formatted image and each stated in the module docs. Bitmap
      pages go straight after the root, extension blocks between the two;
      `DOS\4`/`DOS\5` roots carry an empty dircache block from birth (the
      oracle's do, so a null pointer there would be wrong), placed after
      the bitmap rather than at the oracle's own allocator-artefact
      position. The boot block's checksum is left **zero** by default —
      that is what makes it non-bootable, which is what `Format` produces
      and `Install` fixes; one that checksums over 1012 zero bytes is
      strictly worse, because the ROM would accept it and jump in.
      `FormatOptions::boot_checksum` opts in for a caller writing its own
      boot code. Every variant × {512, 1024, 4096, 32768} opens, validates
      with zero findings, and has a bitmap marking exactly the blocks the
      layout allocated; xdftool mounts, lists, writes to and reads back a
      `DOS\1` and a `DOS\3` ADF this crate formatted, and the two
      implementations' fresh images agree longword for longword bar the
      dates and the root's longword −4.
- [x] **Populate from tree**: `Populator` in `src/populate.rs`, and
      `populate_from_tree()` on top of it under `std`. Files, directories,
      protection/comment/date/owner, both name layouts (with the
      `T_COMMENT` overflow block when a long LNFS name crowds the comment
      out), OFS and FFS data blocks, `T_LIST` extension blocks, and
      `DOS\4`/`DOS\5` dircache maintenance. Name validation is
      `format()`'s own `check_name_bytes`, generalised over the maximum
      rather than reimplemented — 30 bytes classic, 107 LNFS, Latin-1, no
      `:`/`/`, no control codes.

      Three decisions worth stating. **The allocator is a cursor**, not a
      bitmap search: a populator is constructed from the `FormatLayout`
      the format returned, so the used set is *known* rather than
      discovered, allocation walks upward from block `reserved` (using the
      half of the volume that lives *below* the root, which an
      after-the-root cursor would throw away) stepping over the format's
      one contiguous run, and the bitmap is written **once** at
      `finish()`. Nothing here ever decides whether a block on disk is
      free, so nothing here can decide it wrongly — which is precisely the
      discipline M3 has to acquire and this milestone gets to skip.
      **The session is honest about being unfinished**: construction
      clears the root's `bitmap_flag`, so an interrupted or `abandon()`ed
      populate leaves a volume saying "my bitmap is mid-update" rather
      than one claiming blocks are free while files use them; `finish()`
      restores it last, after the pages it describes are down. **Entries
      go in at the head of their hash chain**, which is what the oracle
      does: three names hashing to slot 54 written in order to a `DOS\3`
      ADF by xdftool come back newest-first, and this crate reproduces
      that order exactly. Dircache records are appended in creation order
      into a chain that spills at block boundaries, as xdftool's do, with
      the one difference that the record's secondary-type byte is filled
      in where xdftool leaves it zero — strictly more information, and a
      validator reads a zero there as "not recorded" either way.

      Host metadata maps as documented on `protection_from_metadata`: the
      owner nibble *denies*, so `u+r`/`u+w`/`u+x` **clear** the R/W/E
      bits; the D bit is always clear because POSIX governs deletion by
      the directory's write bit and there is nothing per-file to map; the
      group and other nibbles grant in the normal sense and are mapped
      straight through; archive/script/pure/hidden stay clear because they
      are facts about a backup, a Shell and an Amiga that the host does
      not have; and the owner longword is **not** derived from the host
      uid/gid, because truncating one into a 16-bit muFS UID would invent
      an owner. Names are converted from UTF-8 to Latin-1 where every
      character fits and refused by name where one does not.
- [x] **Round-trip property tests**: `tests/volumes.rs` builds a seeded
      pseudo-random tree (a xorshift written out rather than a dependency
      taken) for **every variant × {512, 1024, 4096}** — varying depth,
      name length including past 30 bytes on LNFS and Latin-1 bytes only
      the intl table folds, file sizes from 0 to ~100 KB so a header's
      pointer table is crossed where the arithmetic says it must be,
      comments up to and past what fits beside a name, and random
      protection/owner/date — writes it, reads it back and compares
      *every* field, then validates with zero findings. Names are compared
      byte for byte rather than under the fold table, because case is
      preserved on disk and a writer that upper-cased what it stored would
      still pass every lookup.

      The differential leg is in `tests/differential.rs` and runs the
      other way: this crate populates the fixture tree on all eight
      variants, xdftool lists it, reads every byte of every file back out
      (including the one that crosses an extension block), *writes a new
      file into it* — allocating from the bitmap `finish()` laid down and
      chaining into a hash table this crate filled — and the result still
      validates here, dircache agreement included. Same skip-if-absent
      rule as the rest of that file.
- [x] **Guest-mount proof**: an image created here boots/mounts under a
      real Amiga ROM (the consumer that cannot be argued with).
      Done under Kickstart 3.1 (A1200) booting Workbench 3.1, via a
      deterministic headless Copperline run: `examples/guest-adf.rs`
      builds a `DOS\3` ADF (formatted and populated entirely by this
      crate — a comment, a 40 000-byte extension-block crosser, a
      Latin-1 `Café-Latin1` name), the volume appears on the Workbench
      desktop by name, and an AmigaShell `list df1: all` +
      `type df1:readme` through the ROM's own FFS handler lists every
      entry with sizes, comment and protection intact and prints the
      file's content — the lowercase `readme` resolving against the
      stored `ReadMe` proving the guest's intl fold agrees with ours.
      Mount is *not* boot: the image is deliberately non-bootable
      (`format()` leaves the boot checksum zero, as `Format` does;
      `Install` is a different program), so Workbench boots from df0
      and the proof volume rides df1. Reproduce with
      `cargo run --example guest-adf -- proof.adf` and a scripted
      `copperline --model A1200 <KS3.1 ROM> --insert-disk-after 0 df1
      proof.adf ...` — deterministic, so the same script yields the
      same screenshots.
- [x] **CI**: stable + MSRV 1.63, `--no-default-features`, clippy
      `-D warnings`, rustfmt, docs; differential job where xdftool is
      installable. Pulled into this milestone because the write side is
      where an untested-configuration regression starts corrupting
      images rather than misreading them. `.github/workflows/ci.yml`
      runs exactly the commands this repository is checked with locally
      — nothing in it is a check the working tree does not already pass.
      **It has not had a live run**: the workflow lands with the code and
      the first push is its first execution. Wave 2 adds a nightly
      `cargo fuzz build` job — see the fuzzing box for why it builds and
      does not run.
- [x] **Fuzzing**: `fuzz/`, cargo-fuzz, nightly-only, with its own
      `[workspace]` so `cargo test`, `cargo clippy --all-targets` and the
      MSRV build never see it. Two targets, because the writer gives the
      fuzzer a second question:

      - `fuzz_read` takes arbitrary bytes as a volume, opens it, walks
        every directory, resolves every link, streams every file, reads
        every comment, dircache, soft link and bitmap, and validates —
        and must never panic, hang or OOM. Files are read with
        `read_file_with` and never `read_file`: the convenient form
        allocates `byte_size` bytes and `byte_size` comes off the disk,
        which is a documented footgun rather than a bug, and a fuzzer
        that tripped it would be reporting the documentation. The walk
        carries its own visited set, because `read_dir` refuses a cycle
        *within* one directory and a cycle across two is only visible to
        the walker. The image is capped at 256 KB: the bugs live in block
        contents, not in block counts.
      - `fuzz_roundtrip` reads the input as a little tree program and
        asserts the property this milestone is for — **any tree the
        writer accepts reads back identical**. `DuplicateName` and
        `VolumeFull` are skipped because they are the writer working;
        every other error is a failure, since they are all statements
        about a caller's input and this caller's input has been made
        legal by construction.

      Sixty seconds each on the seed corpus: 248 843 runs of `fuzz_read`
      (958 edges covered) and 42 005 of `fuzz_roundtrip` (1268), no
      crashes, no timeouts, no OOMs, nothing in `artifacts/`. Nothing
      needed fixing — which is a statement about the read side's existing
      chain guards and length clamps rather than about the fuzzer, and
      the corpus is kept so the next change to either side starts from
      here rather than from noise. The seed corpus is **generated,
      not committed** — `cargo run --example seed-corpus` writes one
      populated volume per variant per block size — for the same reason
      no ADF is checked into `tests/`: a binary blob in a repository is a
      thing nobody can review, and one that drifts out of step with the
      writer that made it is worse than no seed at all.

      CI **builds** the targets on nightly and does not run them. A CI job
      is the wrong place to find a fuzz bug (a minute of libFuzzer proves
      nothing; an hour on every push is somebody else's electricity) and
      exactly the right place to catch the harnesses rotting, which is
      what happens to code that lives outside the workspace and is
      therefore never compiled by `cargo test`. The same job runs the
      corpus generator, so that stays honest too.

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
- [ ] **Validator repair**: the write-side half of `validate()`, doing
      what the ROM disk-validator does — rebuild the bitmap from the
      reachability walk (which `validate()` already performs and
      `pack_bits` already serialises), write fresh pages, stamp
      `bm_flag` valid; optionally sever the unfixable (a corrupt chain
      truncated at the last good link, orphans left leaked — leaks are
      the recoverable direction). First consumer of the allocator after
      the mutators themselves, and a prerequisite for resize, which
      must rebuild the bitmap rather than hand FFS an invalid flag the
      way AmiPart does. Repair only ever *adds* allocation and only
      ever removes reachability — the two directions `validate()`
      already distinguishes — so a repaired volume can be worse than a
      healthy one but never worse than the damaged one it started as.

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
