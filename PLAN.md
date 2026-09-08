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
and MSRV across both feature sets, and both differential legs passing —
xdftool on all eight variants, fstool on the six it can read. One box
below stays open for the parts of the differential suite that need more
than `cargo test` can reach.

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
      xdftool (GPL oracle — run, never copy), fstool (MIT —
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

      **The `fstool` leg has since landed too**, in the same file and the
      same shape (`AMIGA_FFS_FSTOOL`, then `PATH`, skip with a printed
      reason). Six tests, and CI `cargo install fstool --locked`s it and
      asserts presence rather than skipping:

      - **fstool reads what we write**, `DOS\0`–`DOS\5`: the variant
        string from `info`, the whole tree entry-for-entry through `ls`,
        and every byte of every file through `cat` — the 40 000-byte
        extension-block crosser, the empty file, and a Latin-1 `Café`
        whose `é` is one byte on the volume and two in fstool's output.
        `DOS\4`/`DOS\5` read correctly through it even though it knows
        nothing about dircache blocks, because its reader walks all 72
        buckets rather than hashing a name to find one.
      - **We read what it writes**, `DOS\0`–`DOS\3`: tree, bytes,
        `validate()` clean with zero findings, at the ADF geometry and at
        its own default 1 MiB (geometry is a documented don't-care; the
        root-block formula agrees at both sizes, and its boot block leaves
        the root pointer zero, which is fine because `canonical_root_lba`
        derives it).
      - **Mutation, both directions.** `fstool add`/`rm` and the shell's
        `mkdir`/`put` mutate a volume this crate populated — allocating
        from our bitmap, splicing into our hash chains, freeing an
        extension chain we wrote — and we read it back clean, no orphans,
        no reachable-but-free. And the reverse: `Mutator` creates,
        renames across directories (Latin-1 on both ends), deletes and
        sets metadata on a volume `fstool create` produced, fstool reads
        every byte back, then writes into it again. Two independent
        writers taking turns on one image, three turns deep.

      What it cannot do, established by reading its source rather than
      guessing, and asserted so the limits stay facts:

      - **No `DOS\6`/`DOS\7`.** Confirmed, and worse than a refusal:
        `Affs::open` accepts any boot flag 0..=7 and decodes bit 2 as
        "dircache", so a long-name volume opens, is mislabelled `+DC`,
        and `read_name` reads the BCPL name at the classic offset `0x1b0`
        clamped to 30 bytes — which is not where the name is. Long names
        stay this crate's own, and the long-name regression stays the
        thing only we assert.
      - **No `DOS\4`/`DOS\5` writing** — and it does not refuse, which is
        the finding this leg paid for. Two real bugs in fstool 0.4.26,
        both cited in the test that asserts them:
        `Variant::from_flag` sets `intl = flag & 2 != 0`, but
        directory-cache mode *implies* international folding, so
        `hash_name` puts an accented name in the classic-fold slot —
        enumeration finds the file, `Lock()` never will; and `AffsEditor`
        has no notion of a dircache, so the cache AmigaDOS actually
        serves `List` from is left stale. This crate's validator names
        both. Nothing else about the resulting volume is wrong, which is
        what makes them two specific bugs rather than a broken writer.
      - **No comments, no dircache, no block size but 512** (`BSIZE` is a
        compile-time constant), and no protection/owner/date knobs on
        `create`. `-O` takes exactly `fstype`, `intl` and `volume_label`.
      What still remains, and why it is not done rather than merely not
      done yet:

      - **AROS `afs.handler` as an oracle.** The two subprocess legs now
        catch the mistakes this crate and its own synthetic builder would
        make together, and `fstool` supplies the readable second
        implementation the plan wanted `affs-read` for — so `affs-read`
        itself is no longer wanted and is dropped from this box.
        `afs.handler` is still worth having and is still the only oracle
        that is also a *real consumer*, running inside a guest against the
        same images. It needs that guest, which is Copperline's seam and
        not something `cargo test` can reach.
      - **Block sizes other than 512.** xdftool's ADF and HDF images are
        512-blocked and fstool's AFFS `BSIZE` is a constant; larger block
        sizes live behind an RDB, which is `rdbtool`'s and amiga-rdb's
        territory. Covered synthetically at 512/1024/4096 in
        `tests/volumes.rs` meanwhile.
      - **Comments.** xdftool's `comment` command raises a `TypeError`
        before writing anything (amitools 0.7.x), and fstool has no
        comment surface at all, so no oracle-written volume can carry
        one. Covered synthetically in both layouts, `T_COMMENT` overflow
        block included.
      - **The amibake AROS `DOS\7` fixture** — a real-world image rather
        than a generated one. Wanted; needs a fixture pipeline, not just
        a test.

      Two of the four oracles this box names are now landed and wired
      into CI, which is the substance of it; the box stays unticked
      because the remaining two are the two that cannot be reached from
      `cargo test` — `afs.handler` needs a guest, and the amibake fixture
      needs a pipeline. Neither is blocked on anything here.

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

**Landed.** Wave 1 — the allocator and the validator's repair half, the
two pieces everything else in this milestone stands on; wave 2 — create,
delete, rename and set-metadata in volumes this crate did not write,
through `Mutator`; wave 3 — file write, append and truncate, the ranged
read that shares their chain machinery, and the crash sweep over all of
it. The differential mutation suite closed last, guest leg included: the
ROM's own FFS and this crate's `Mutator` applying the same operations to
copies of the same volume, and agreeing about everything either can
observe.

- [x] **Allocator**: bitmap-based block allocation with the volume's
      own policy quirks documented as discovered. `src/allocator.rs`,
      over `S: BlockMedium` (which moved to the crate root, since the
      allocator and the repairer want the same bound `Populator` defined).

      **The policy is first fit from a hint, forward, wrapping once**, and
      the hintless form rotates a cursor so consecutive calls climb rather
      than rescanning the same full region. That is behaviourally what the
      shipping implementations do. Linux's `affs` (`fs/affs/bitmap.c`, GPL
      — read to describe, never copied) takes a *goal* from the caller,
      defaults it to the volume's first allocatable block, scans forward
      within the goal's bitmap page, moves to the next page when it is
      full, and wraps to page zero before giving up; it additionally
      pre-allocates a run of consecutive free bits within the goal's
      longword, which is a performance trick with no on-disk consequence
      and is deliberately *not* copied here (a run handed out and not used
      would have to be given back or leaked, and one call marking exactly
      one block is what makes the ordering rule below statable). xdftool,
      observed rather than read, scans from the *bottom* every time: three
      files on a `DOS\1` ADF filled 866..902, and after deleting the
      middle one and writing another it filled the freed 873..879 and
      882..883 before touching anything higher — first fit with the hint
      pinned to zero. All of these are correct; the difference is locality,
      nothing on disk records which was used, and what the tests assert is
      only the three things that are not a choice: never hand out an
      allocated block, never hand one out twice, never hand out anything
      outside `reserved..block_count`.

      **The crash ordering is in the types, not only in the docs.**
      `allocate()` returns an `Allocation`, not a block number.
      `Allocation::block()` is the LBA to write the block's *own contents*
      at — always safe, because nothing reaches it — and
      `Allocator::reference()` is the LBA that may go into somebody else's
      pointer, which **refuses** (`AllocError::NotDurable`) until the
      bitmap page carrying the bit has been flushed. Between them sits
      `flush()`, which writes only the dirty pages (one dirty flag per
      page; a flag is cleared only after its write returns, so an
      interrupted flush leaves the rest queued) with the checksum in
      longword 0. Freeing cannot be enforced the same way — this code
      cannot see the caller's metadata write — so `free()` is documented
      as the second half of an unlink and refuses the one error it *can*
      see, a double free. `mark_bitmap_invalid`/`mark_bitmap_valid` are
      the session's bookends, the second stamping LNFS `NumBlocksUsed`
      and the flag last.

      A volume whose `bitmap_flag` is 0 is refused outright
      (`AllocError::BitmapInvalid`): those bits are whatever an
      interrupted update left, and rebuilding them is repair's job, not
      an allocator's guess.

      Tested by fill-and-free to exhaustion (every free block once, then
      all of them back), hint and wrap behaviour, double free and
      not-covered refusals, a 4000-step seeded interleave of
      alloc/free/flush cross-checked against a model `HashSet` *and*
      against the bits actually written, a write-counting sink proving a
      flush writes one block per dirty page and nothing on a second call,
      and a crash sweep that stops the medium after every prefix of a
      session's writes: the damage is always leak-shaped (`OrphanBlock`
      allowed) and `ReachableButFree` never appears.
- [x] **Create/delete/rename** in existing volumes: hash-chain
      insertion/removal under both fold tables, dircache invalidation
      (`DOS\4`/`5`: update or clear, never leave stale). `Mutator` in
      `src/mutate.rs` — a session holding a `Volume` and an `Allocator`,
      because reading a 2 GB volume's bitmap once per created file would
      dominate everything else, and holding nothing *but* those two,
      because a cached anything is a second copy of the volume to keep in
      step. Every public operation leaves the volume consistent, so there
      is no `finish()` to forget: that is affordable here and not in
      `Populator` precisely because nothing in this module ever lets the
      bitmap say "free" about a block something reaches, so the flag stays
      −1 throughout.

      **Block assembly moved to a shared module** (`src/build.rs`,
      crate-private): the header block's two name layouts, the `T_COMMENT`
      block, the OFS/FFS data block, the dircache block and its records.
      `Populator` was refactored onto it rather than copied from — a
      second implementation of "where does an LNFS comment go" is a second
      chance to put it at the classic offset, which is the mistake this
      crate exists to not make.

      Four decisions worth stating.

      **Deleting the target of a hard link is refused**
      (`MutateError::LinkedTo`), and the reason is that the two shipping
      implementations produce *different volumes*. AmigaOS promotes — its
      own documentation: "if the object a hard link points to is deleted,
      then the first hard link in the chain is altered so that it becomes
      the new file header block. The original file header block is then
      freed" — so the object's header block changes number, invalidating
      every pointer to it including its children's `parent` longwords.
      Linux's `affs` does the opposite and says why: "we can't remove the
      head of the link, as its blocknr is still used as ino, so we remove
      the block of the first link instead" (`affs_remove_link`), so it
      keeps the original block, copies the *link's* name into it,
      re-inserts it into the **link's** directory and frees the link — the
      file survives under another name in another directory. ADFlib
      refuses links outright, amitools has no link code at all, and AROS's
      `afs.handler` defines `BLK_ORIGINAL`/`BLK_LINKCHAIN` and never reads
      them, so its delete leaves the links dangling. Either shipping
      behaviour rewrites several blocks with no ordering that makes an
      interruption harmless. Refusing keeps the choice open and rules out
      the one outcome nobody wants; deleting the links first and then the
      target works today, and is what the refusal points a caller at.
      Deleting a *link* is implemented: it is spliced out of its target's
      −10 chain while still reachable from its directory, then unlinked,
      then freed.

      **Dircaches are regenerated, not patched.** The affected directory's
      whole chain is rebuilt from its hash chains after every create,
      delete, rename and metadata change, reusing blocks where the chain
      is no longer than before, allocating where it grew and freeing where
      it shrank. Surgical editing would be less I/O and one more place to
      leave the cache disagreeing with the chains; regeneration cannot,
      because the chains are the input. The chain is written *backwards*,
      last block first, so a `next` pointer never names a block that is
      not there yet. Every mutation test asserts zero `DircacheStale`
      findings afterwards.

      **The parent's date is stamped — when the caller supplies a clock.**
      The question was settled by reading four implementations, which do
      not agree. amitools' `xdftool` stamps the parent's longword −23 *and*
      the root's −10 on every create and delete (`ADFSDir._create_node`
      and `_delete` both end in `update_dir_mod_time()` +
      `volume.update_disk_time()`). Linux's `affs` stamps the parent
      (`affs_insert_hash`/`affs_remove_hash` both mark the directory inode
      dirty, which `affs_write_inode` writes to `change`/`root_change`,
      −23) but writes the root's −10 only from `affs_commit_super`, on
      sync and unmount. ADFlib stamps the parent in `adfCreateEntry` only
      when the entry becomes a slot's head and never in `adfRemoveEntry`.
      AROS stamps nothing on create or delete and every ancestor up to the
      root on a *write*. This crate follows amitools, the closest thing to
      a real-Amiga reference among them — but it is `no_std` and has no
      clock, so `Mutator::clock` supplies "now" and **until it is set no
      date is written at all**: a wrong date on the disk is worse than an
      old true one.

      **A rename unlinks before it relinks.** The entry is unreachable in
      between — a leak, recoverable — where the other order would put one
      block in two chains with one `hash_chain` longword to serve both.
      The new slot's head is read *after* the unlink, because for a
      same-slot rename (which is what a change of case is) the unlink is
      exactly what changed it. Renaming a directory into its own subtree
      is refused by walking `parent` pointers upward with a cycle guard,
      since the volume may already contain the loop.

      Tested by: creates into an existing tree on every variant ×
      {512, 4096} with a file that crosses an extension block; head/middle
      /tail chain splicing; the refusals (non-empty directory, duplicate,
      linked-to, own subtree, bad names, the root); case-only and Latin-1
      renames under both fold tables; LNFS renames crossing the merged
      field's capacity in both directions, with the comment moving
      inline↔overflow and the block count moving with it; `set_metadata`
      on every variant; dircache agreement after 30 creates, 25 deletes, a
      rename and a metadata change at two block sizes; **a create and its
      delete returning the volume to byte-identical block accounting**; a
      seeded random interleave of all five operations against a
      `BTreeMap` model on all eight variants × {512, 4096}, compared field
      by field and byte by byte after every batch and emptied back down to
      the blocks a fresh format had; a crash sweep (below); and a
      differential leg where xdftool lists a mutated volume, reads every
      byte out of it, writes into the bitmap this crate edited in place —
      and where the same operation sequence run through both
      implementations produces the same tree, the same file bytes and the
      same used-block count.
- [x] **File write/append/truncate**: FFS chains and OFS headers both.
      `Mutator::write_file`, `Mutator::append` and `Mutator::truncate` in
      `src/mutate.rs`, over one private `edit_file` — three ways of
      choosing an offset, a slice and a resulting length, so the block
      arithmetic and the OFS bookkeeping exist once.

      **The file header block is the single commit.** Every size change
      ends in exactly one write, carrying `byte_size`, `high_seq`, the
      whole data-pointer table and the extension pointer together; before
      it the file is entirely the old one, after it entirely the new one.
      That is forced rather than chosen: `file_chain` refuses a header
      whose length and extent disagree *in both directions*, so any order
      moving one before the other leaves a file that will not read, and
      `byte_size` lives in the header.

      Two consequences follow, and they are the wave's two real decisions.
      **The extension chain is rebuilt, never patched** — a `T_LIST` block
      is reachable only through the block before it back to the header, so
      editing one in place would commit a new block count from a block
      that is not the header. A size change therefore allocates a fresh
      chain, writes it last-block-first, points the header at it and frees
      the old blocks afterwards; it costs one block per ~35 KB of OFS file
      per size change, which is the price of the commit being one write,
      and is why every entry point takes a whole slice. (The same argument
      that made dircaches regenerate rather than patch, one chain over.)
      **A data block whose *recorded* length changes is replaced, not
      edited** — on OFS only, where the length is a field (longword 3): a
      grow makes the old last block full, a shrink makes some earlier
      block short, and copy-on-writing it keeps the header the only
      commit. FFS records no such length, so its boundary block is written
      in place and the bytes it gains past the old end are unreachable
      until the commit adds them.

      **Writing past the end zero-fills**, and the evidence says that is a
      choice rather than a default. AmigaDOS cannot even reach the state
      by seeking: "You cannot Seek() beyond the end of a file"
      (`dos.library/Seek`), and `ACTION_SEEK` "shall fail with the error
      code `ERROR_SEEK_ERROR`" past EOF, leaving the pointer unaltered —
      confirmed structurally in AROS's `afs.handler`, whose `seek()`
      initialises `ERROR_SEEK_ERROR` and never clears it when the chain
      runs out. The only route is `SetFileSize()`, whose autodoc says "if
      the file is extended, no values should be assumed for the new bytes"
      and whose packet specification says outright that "unlike other
      operating systems, AmigaDOS does not enforce zero-initialization of
      the extended region". Both permitted answers ship: AROS grows via
      `writeData` with a NULL buffer, which on FFS does not write the new
      block at all and leaves whatever the disk held; Linux's `affs`
      zeroes, through `cont_expand_zero` and
      `affs_extent_file_ofs`/`affs_getzeroblk`. This crate zeroes — not
      for tidiness but because the bytes a freshly allocated block holds
      are a *deleted file's*, and handing them back through a new file's
      length is an information leak the specification permits and nobody
      wants. Sparse is not on the table at all: the pointer table is dense
      by construction, data block *n* being the *n*th slot counted through
      the chain with 0 as the terminator, so the format has no hole to
      leave.

      Tested by: the write/append/truncate matrix on OFS, FFS, a dircache
      variant and a long-name variant at 512 and 4096 — appends crossing
      the extension boundary, overwrites spanning
      partial-first/full-middle/partial-last, truncates to 0, to mid-block,
      to an extension boundary and upward, and a write past the end whose
      gap is asserted to be zeroes on a volume pre-filled with `0xA5`;
      every step followed by a byte-identical readback against a `Vec<u8>`
      model and a clean `validate()`; a seeded random interleave of the
      three operations with offsets and lengths drawn *around* the two
      boundaries where the arithmetic differs; the crash sweeps below; and
      a differential leg where xdftool reads back every byte of a file
      this crate appended to, overwrote, truncated and grew, and where the
      same end state reached by a delete-and-rewrite there and an
      append-and-overwrite here yields the same tree, the same bytes and
      the same used-block count.
- [x] **Ranged reads** (`read_range(&chain, offset, buf)`): the FUSE-shaped
      gap in the read surface, and Copperline's too — a trackdisk-level
      consumer never wants whole files. `Volume::read_range` in
      `src/file.rs`, landed with wave 3 because both live in the chain
      machinery.

      It walks **only the blocks the range covers** — the first is
      `offset / payload_size`, arithmetic rather than a walk — and
      verifies the OFS headers of exactly those, since a block's expected
      sequence number falls out of its index and not out of having reached
      it from block 1. The verification is the *same* function the
      streaming read uses, because two copies would be two chances for one
      of them to stop checking the sequence number. Clamping is `read(2)`'s:
      `byte_size` is the authority for where the file stops (the last
      block has capacity past it and returning that would invent data), an
      offset at or past the end returns `Ok(0)` rather than an error, and
      a short count is how a partial fill reports itself. Tested against
      the whole-file read on both filesystems at both block sizes over
      twelve range shapes, with the block *reads* counted to prove the
      range really is a range, and with a corrupted OFS block that a range
      stopping short of it still reads past and a range touching it
      refuses by name.

      `FileChain` was split from the streaming read precisely so it could
      be collected once and reused; a pre-collected chain is also
      immutable data a concurrent consumer can hold outside its `Volume`
      lock, which keeps the *read* critical section at one block read.
      Read is the only path with that property: `lookup` and `readdir`
      walk hash chains, so their sections are chain-length long — fine in
      practice, and fine *because* the adapter's block cache absorbs the
      walk, which is the cache earning its place rather than an
      optimisation. Concurrency itself stays the consumer's: `&mut self`
      was deliberate, a FUSE adapter wraps the volume in a `Mutex` and
      brings the block cache its own medium warrants, since an LRU sized
      for a local image is wrong for a ZuluSCSI card over USB and that is
      exactly why the format crate cannot choose it.

      The mount-time story for such an adapter, recorded here because it
      falls straight out of surfaces the crate already has: `validate()`
      walks the whole volume — seconds on an image file, rather longer
      through a USB-attached card — so it is a mount *option*, defaulted
      by mode: always before exposing writes, opt-in for read-only. A
      damaged volume still mounts read-only with everything reachable
      (the walk records and continues by design, and read-only is when
      someone most wants the driver); refuse-or-repair gates only the
      write side. `bitmap_flag == 0` is the cheap early signal that a full
      walk is warranted before anyone pays for one. And a mount that came
      up read-only *because* validation found something must say so
      distinctly from one the user asked to be read-only: `EROFS` on the
      first write is technically correct and tells them nothing, while a
      mount-time log naming the findings — and the repair option that
      would clear them — turns "the driver refused" into "the driver
      protected the image". Which is the whole point of `Report` being
      typed findings rather than a boolean.
- [x] **Crash-shape discipline**: data blocks before metadata, chain
      pointers flipped last, bitmap updated in an order that at worst
      *leaks* blocks (validator-recoverable) rather than double-uses
      them. The format has no journal; ordering is all there is.

      Every write path in the crate is swept: `tests/volumes.rs` stops the
      medium after each
      successive write of an allocator session (wave 1), of a repair
      (wave 1) and of a mutation session that creates a directory, creates
      a file with an overflowing comment, renames and deletes (wave 2), on
      a plain, a dircache and a long-name variant. The damage is always
      leak-shaped: `OrphanBlock` is allowed, `ReachableButFree` never
      appears, every entry still reachable still reads, and — the one
      addition this wave needed — `DircacheStale` is allowed on the two
      variants that have caches and nowhere else, because a cache is
      advisory, a half-written one is what every dircache-unaware tool
      leaves behind, and `validate()` saying so is the mechanism by which
      it gets rebuilt.

      Wave 3 added the sweep the box was held open for — **file write,
      append and truncate** — and it comes out in two pieces, because the
      two halves of the operation have different shapes.

      An **overwrite that changes no length changes no metadata**: the
      block count, `byte_size`, the pointer tables and the bitmap are all
      untouched, so every prefix of it validates with *zero* findings and
      the file is the right length made of some old blocks and some new
      ones. That is asserted directly — each block compared against the
      old bytes *and* the new bytes and required to be one of them. It is
      also the one place this milestone's "old volume, new volume, or old
      volume plus leaks" invariant weakens, and it weakens to exactly
      **old bytes or new bytes, independently per block**. The weakening
      is inherent to overwriting in place rather than a shortcut: the
      alternative is copy-on-writing every touched block, which turns
      every overwrite into a reallocation and makes a file's blocks
      migrate across the volume for a guarantee the format cannot express
      anyway. Nothing structural is at risk in it, and `ReachableButFree`
      remains impossible.

      A **size change** is swept the same way the entry operations are, on
      a plain, a dircache and a long-name variant and on both filesystems:
      every prefix of a session that appends across an extension-block
      boundary, overwrites through the middle, truncates down and grows
      again. `OrphanBlock` is allowed and `DircacheStale` on the two
      variants that have caches; `ReachableButFree` never appears, no
      reachable block is corrupt, and — the assertion this wave is really
      about — *every file still reachable still reads every one of its
      bytes*, which is the header-block-as-single-commit rule saying that
      a file's length and its extent are never caught disagreeing.
- [x] **Differential mutation tests**: same operation sequence applied
      through this crate and through the guest's own filesystem on a
      copy; resulting volumes must agree (allowing documented
      don't-care fields — dates, allocation order).

      **Partly landed** — the oracle leg is done and the guest leg is
      not. `tests/differential.rs` runs the same sequence (makedir, two
      writes, a delete, a write) through xdftool and through `Mutator`,
      and the resulting trees agree entry for entry, byte for byte, and
      on the used-block count; separately, xdftool lists, reads and
      *writes into* a volume this crate mutated. Wave 3 added the same
      exercise for file *contents*: xdftool reads back every byte of a
      file this crate appended to across an extension-block boundary,
      overwrote through the middle, truncated and grew — the one check the
      model tests cannot make, since a writer and a reader that agreed on
      the same wrong chain arithmetic would satisfy both — and the same
      end state reached by a delete-and-rewrite there and an
      append-and-overwrite here produces the same tree, the same bytes and
      the same used-block count. The don't-cares are the
      two predicted ones and no others: allocation order (this crate
      scans forward from a hint, xdftool from the bottom of the bitmap)
      and dates (the caller's to supply here, the wall clock's there).
      **The guest leg is done.** A deterministic Copperline run boots
      Kickstart 3.1 + Workbench 3.1 with a writable copy of a volume
      this crate built in df1 (`write_protected = false` — inserted
      images default protected, and the guest says so in a requester),
      and an AmigaShell script performs makedir, copy, rename, a
      delete that frees an extension chain, and a fresh-file write
      through the ROM's own FFS. Two closures follow: this crate reads
      the guest-mutated image back with `validate()` clean — the ROM's
      allocator, renamer and freer produce a volume we certify — and
      `examples/guest-replay.rs` replays the identical five operations
      through `Mutator` on the pristine copy and compares the two
      volumes logically: tree, kinds, sizes and content hashes agree
      entry for entry, with dates and block placement the two
      documented don't-cares. Not in `cargo test` — it needs a ROM and
      an emulator — but reproducible from the two examples plus the
      scripted invocation, deterministically, which is what Copperline
      is for. The AROS `afs.handler` variant of the same exercise
      remains under the milestone-1 differential box with the amibake
      fixture pipeline.
- [x] **Validator repair**: the write-side half of `validate()`, doing
      what the ROM disk-validator does. `Volume::repair()` in
      `src/repair.rs`, and the first consumer of the allocator.

      The walk is *the same walk*: `validate()`'s tree pass was factored
      out as `walk_reachable()` and both call it, because the bitmap a
      repair writes is **defined** as the set that walk returns, and a
      second implementation of it would be a second answer. The bitmap is
      then rebuilt as the **union** of that walk and the old bits, the
      pages written fresh, the extension blocks and the root's pointers
      rewritten, and `bm_flag` stamped −1 in a write of its own, last —
      with `bm_flag = 0` written *first*, so every intermediate state is
      the honest "my bitmap is mid-update" one. Pages and extension blocks
      whose pointers name nothing usable (zero, out of range, a duplicate,
      or a block the tree itself is using) are replaced with freshly
      allocated ones; a page whose checksum does not balance is rebuilt
      from the walk alone and *said so* (`Action::PageUnreadable`), since
      that is the one place allocation cannot be preserved.

      **Repair only ever adds allocation and only ever removes
      reachability**, and both halves are asserted by the tests. The
      union is why: an orphan stays allocated (`Action::LeakKept`) rather
      than being freed the way the ROM validator frees it, because
      freeing a block the walk did not reach is only safe if the walk was
      complete — and a walk over a damaged volume is exactly where it is
      not, since one unreadable directory block hides its whole subtree
      and every file in it looks like a leak. The same reasoning is why an
      invalid bitmap is still *read*: its bits cannot be believed when
      they say "free", but a bit saying "allocated" is either true or a
      leak, and taking it at its word is conservative either way.
      Severing is opt-in (`RepairOptions { sever }`, default off) and
      narrow: only what the ordinary reader has *proved* it cannot
      follow — a header block that will not parse, a chain that revisits a
      block, a comment pointer whose block will not read as this entry's
      comment — cut by writing 0 into the slot or the previous entry's
      chain longword. The entries behind the cut stay on the disk and stay
      allocated: leaked, not freed. `repair()` returns a report of typed
      `Action`s mirroring the `Finding`s they answer.

      Tested against each damage separately — a cleared bit
      (`ReachableButFree` → allocated, and the volume back to byte-exactly
      what it was), a stale bit (`OrphanBlock` kept, invariant asserted),
      `bm_flag` zeroed, a page whose checksum is gone, a page pointer
      clobbered three ways, and a scribbled header with `sever` on and off
      — plus the whole matrix of eight variants × {512, 1024, 4096}, a
      crash sweep over the repair's own writes, and a differential leg
      where xdftool lists, reads every byte of, and *writes into* a volume
      this crate repaired. A prerequisite for resize, which must rebuild
      the bitmap rather than hand FFS an invalid flag the way AmiPart
      does.

## Milestone 4 — resize

- [x] **In-place grow/shrink of an existing volume**:
      `Volume::resize(&mut self, new_block_count) -> Result<ResizeReport,
      ResizeError<E>>` and a read-only `Volume::minimum_size(&mut self)`,
      in `src/resize.rs`. The filesystem half only — the RDB's `high_cyl`
      move is amiga-rdb's, composed by the consumer; the module doc states
      the ordering rule for each direction (grow: enlarge the partition
      first, then call `resize`; shrink: call `resize` first, then shrink
      the partition — reasoned from "a volume must never be told it is
      bigger than the medium actually is").

      The algorithm shipped in AmiPart's `ffsresize.c` (John Hertell, MIT
      — ported and cited, not copied) confirmed the core fact this crate's
      own `canonical_root_lba` already encoded: FFS recomputes the root's
      LBA from geometry on every mount, so any size change **moves the
      root** to the new midpoint. Moving it means copying the root's
      content to the new LBA and re-parenting its *direct* children (their
      `parent` longword names the root; deeper entries and hard-link
      `real_entry` pointers are untouched, since no child header moves) —
      plus two more pointers this crate found by reading its own layout
      rather than by reading AmiPart, which does not have them: the root's
      own `DOS\4`/`DOS\5` dircache chain (each block's own `parent` field)
      and the boot block's advisory root pointer.

      Three places this goes further than AmiPart, as planned: **every**
      block size 512..=32768 rather than 512/1024 only; the bitmap is
      rebuilt immediately by reusing `Volume::repair`'s reachability walk
      (`walk_reachable`, shared rather than duplicated) instead of
      stamping `bm_flag = 0` for FFS to fix on the next mount; and
      `DOS\4`/`DOS\5` dircache chains plus the LNFS `NumBlocksUsed`/
      `FileSystemType` fields are kept correct in both directions, where
      AmiPart does not touch them at all.

      **What the plan did not anticipate**, learned by building it: a
      shrink's *bitmap page and extension block* relocation falls out of
      `repair`'s own tolerant reader for free (a pointer past the new end
      simply fails its usability check and gets replaced) — but freeing
      what becomes obsolete does not, because `repair`'s one-direction
      rule (never remove allocation an incomplete walk might have missed)
      would otherwise keep the old root and every excess bitmap page as
      permanent leaks. `resize` patches those bits directly into the
      still-intact old bitmap before handing off to `repair`, which is
      the one piece of bitmap surgery this module does itself rather than
      delegating. And the new root's own target block can — on a volume
      packed solid from `reserved` upward, which is exactly how this
      crate's own allocator fills a volume — land on a block real user
      data occupies; this operation refuses that case
      (`ResizeError::RootTargetOccupied`) rather than relocating arbitrary
      data to make room, which is a real, named limitation:
      `minimum_size()` reports what this engine will actually accept, not
      the theoretical floor a data-relocating resize could reach, and the
      two can differ by roughly 2× on a volume with no free gap. Engineered
      directly (a volume filled from `reserved` upward with one big file,
      grown by the smallest possible step so the new midpoint lands inside
      it) rather than only hit incidentally, once a deterministic
      construction was worked out.

      Ordering: the very first write is a full copy of the (still valid)
      root at the new LBA with `bitmap_flag` forced to 0 — after that one
      write, any mount at the new geometry finds a structurally valid,
      honestly-untrustworthy root, the same state an interrupted ordinary
      mutation leaves. Every later write only improves on that floor, down
      to `bitmap_flag = -1` last, out of `repair`.

      `repair` alone only ever rebuilds the bitmap, and calling it after an
      interrupted `resize` can leave `ParentMismatch`/`DircacheStale`
      findings it has no opinion on. What finishes the job — found while
      writing the shrink crash sweep below, in response to review feedback
      asking whether the gap was inherent — is calling `resize` **again
      with the same target**: every write past the first is idempotent, so
      a retry redoes only what an earlier call left undone rather than
      being a no-op itself, and the "nothing to do" fast path now checks
      `bitmap_flag` as well as the size before deciding there is truly
      nothing to redo. Two things a retry still cannot recover, both
      bounded to leaks rather than anything dangerous and both pinned by
      the shrink crash sweep rather than left as prose: the *very first*
      root position in a sequence of retries, once a later call's notion of
      "the old root" has already moved past it (stays allocated as an
      `OrphanBlock`); and the root's own dircache pointer, if a crash lands
      after the first write (which carries it over unchanged) but before it
      is updated, leaving it unreadable through any bound a retry still has
      a use for — the pointer is cleared rather than the whole retry
      failing, `DircacheStale` reports it, and any later `Mutator`
      operation on the root regenerates it from the hash chains, which are
      never at risk. What was genuinely inherent (turning the documented
      gap into a solved problem, per the review's framing) was solved;
      what remained after that is two narrowly-scoped, safe-direction
      losses of bookkeeping, not of data — and now a test asserts exactly
      that shape rather than a wider "trust me".

      Tested in `tests/volumes.rs`: grow-then-shrink round trip on every
      variant × {512, 1024, 4096} with byte-identical files and clean
      `validate()`; the no-op case; crossing the 25-bitmap-pointer
      boundary into an extension block and back; shrink refusal naming
      the offending block with `minimum_size()` agreeing exactly at the
      boundary; relocating a multi-block root dircache chain out from
      under a shrink's cut; an 8-step seeded random grow/shrink sequence
      checked against the tree after every step; `RootTargetOccupied`
      engineered directly; and two crash sweeps, one per direction, each
      over every write prefix, asserting `reachable_but_free` is always
      zero once the bitmap claims validity, and — from the same crashed
      state — that `repair` alone leaves only the bitmap-fixed, findings
      it cannot address named and bounded, *and* that a same-target
      `resize` retry finishes the reparenting in full (checked down to
      every direct child's own parent longword) modulo the two named,
      pinned exceptions above. The shrink sweep is the one review asked
      for by name, since shrink is the direction that relocates metadata
      and issues explicit frees — exactly where a double allocation would
      come from if the ordering were wrong — and the grow sweep alone
      never exercises those paths.

      **Left undone**: an oracle leg in `tests/differential.rs` —
      `xdftool` only opens a fixed 1760-block ADF or an
      explicitly-geometried HDF and neither it nor `fstool` implements a
      resize to cross-check against, so the achievable differential value
      (an oracle re-reading a post-resize image) was judged not worth the
      harness work in the time available, and is recorded here rather than
      silently skipped; and a property test driving `resize` interleaved
      with `Mutator` operations (only pure resize sequences are
      seeded-random-tested today).

## In scope, not scheduled

- **Block layout policy, and compaction** — one subject, at two points
  in a volume's life. The premise: this crate's output is not only
  emulator images. It writes filesystems that land on **real Amiga
  hardware** — CF cards, real drives, real floppies — where a seek is a
  head stepping across a platter and costs milliseconds, not an offset
  into a host file costing nothing. Layout is therefore a durable
  property of every image shipped, not a tuning detail.

  The format itself is the evidence. The root sits at the *midpoint* of
  the volume — the whole reason `canonical_root_lba` exists and the
  reason a resize has to move it — so that a seek to the root is on
  average half a disk from anywhere. On a DD floppy (80 cylinders × 2
  heads × 11 sectors = 1760 blocks, so 22 blocks to a cylinder) block
  880 is dead centre, and a file laid contiguously inside one cylinder
  costs *zero* head steps to read where the same file scattered costs a
  step per block. A design that put its root in the middle was designed
  around seek.

  So, in the order they should be built:

  1. **Policy at creation.** `format` and `Populator` already produce
     contiguous files, but by accident of a forward-cursor allocator
     rather than by stated policy. Make it a policy with the reasoning
     attached and the allocator hint-driven: a file's data adjacent to
     its header, a directory's children near the directory, the
     metadata a boot touches clustered near the root. This is where
     most of the value is, because it costs nothing at build time and
     every image gets it.
  2. **Compaction of an existing volume** — the same policy applied
     retroactively, which is the defragmenter. Relocating a block is
     copy-then-flip (allocate, write content, update the one pointer
     that names it, free the old), so every intermediate state is the
     old volume plus a leak: *safer* than the in-place file overwrite
     this crate already ships, and covered by the M3 allocator's
     mark-then-use discipline and `repair()`. Two tiers, very different
     in cost: **data blocks only** touches just that file's own header
     and extension tables, and gets most of the benefit; **header
     blocks** additionally require re-pointing the parent's hash chain,
     the children's parent longwords, `real_entry`/`next_link` chains
     and the dircache record — doable (resize does the root-move
     version of exactly this) but where the bugs would live.
  3. **Which closes resize's real gap.** `resize()` today refuses a
     shrink when user data occupies the new root's target
     (`RootTargetOccupied`), so on a volume packed from `reserved`
     upward — this crate's own allocator's normal output — the
     achievable minimum can be about twice the theoretical floor.
     Relocation is precisely what turns that refusal into a move, and
     makes `minimum_size()` report the floor rather than what the
     current implementation will accept. Compaction is not merely a
     companion to shrink; it is what lets shrink reach its limit.

  **Passive reorganisation** is the third strategy, and the one worth
  building alongside the policy rather than after it. Aminet's
  `PFS2DefragTry` (Martin Steigerwald, 1998 — crediting Simon for the
  idea) defragments by copying each fragmented file out and back,
  letting the filesystem lay it down afresh. That works on PFS2/AFS
  because *their* allocators deliberately seek contiguous runs; it does
  **not** transfer to FFS, whose allocator is a volume-wide next-fit
  rover with no such intent, so a recopy just lands the file wherever
  the rover happens to be. But this crate *is* the writer when it is
  the one mutating, and it chooses placement rather than hoping — so it
  can do passively what PFS does natively: when a file is being
  rewritten anyway, place the new blocks as one run; when a mutation
  passes through a directory, prefer allocations that pull its entries
  toward it. No separate pass, no tool to run, just leaving a volume
  better than it was found. The cost is that it makes writes place
  blocks by policy rather than by cursor, which is the same machinery
  item 1 needs — which is why the two belong in one piece of work.

  **Survey before designing the policy**, the way the allocator and the
  LNFS layout were surveyed: ReOrg and the other commercial Amiga
  defragmenters had opinions tuned against real drives and real FFS
  read-ahead, and those opinions are worth more than first principles.
  One modern wrinkle for the notes: CF and SD-via-adapter have no seek
  cost but do have erase blocks, so contiguity still pays while
  cylinder-alignment reasoning does not transfer.

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
- [x] crates.io publish — **0.2.0 is out** (`v0.2.0`, tagged and
      released), carrying all three milestones rather than the
      read-only surface this box originally anticipated. Version 0.1
      is the primitives section above; 0.2 is read, create and mutate.
- [ ] `#![deny(missing_docs)]` and the API-surface pass that earns a
      1.0. Every public item *is* documented — the crate would very
      nearly pass the lint today — but turning it on is a promise
      about the surface, and the surface is worth one deliberate read
      first: `Populator` and `Mutator` grew from opposite ends and
      overlap (`Metadata` vs `MetaUpdate`, two ways to create a file);
      `Volume`'s inherent-impl surface is now spread over six modules;
      and the error types have multiplied (`Error`, `FormatError`,
      `AllocError`, `MutateError`, `PopulateError`) in ways that are
      right per-operation but worth checking read as one crate.

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
