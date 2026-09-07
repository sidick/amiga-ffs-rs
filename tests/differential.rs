//! Differential tests: volumes built by an *independent* implementation,
//! read back through this crate.
//!
//! The oracle is `xdftool` from amitools — GPL-2, so it is **run, never
//! copied**. Nothing in this file transcribes a line of it; it is invoked
//! as a subprocess, it lays down the bytes, and this crate has to agree
//! with what a completely separate codebase decided those bytes mean.
//! That is the only kind of test that can catch a mistake this crate and
//! its own synthetic builder in `volumes.rs` would make together — and
//! the failure this crate exists to not have (every name empty on a
//! `DOS\7` volume) is exactly that kind of mistake: self-consistent,
//! and wrong.
//!
//! # Skipping, not failing
//!
//! `xdftool` is not a build dependency. When it is absent every test here
//! prints why and returns green: a contributor without amitools installed
//! must not see red for a tool they were never asked to have. Set
//! `AMIGA_FFS_XDFTOOL` to point at a specific one, or leave it and the
//! suite looks for `xdftool` on `PATH` and then for
//! `python3 -m amitools.tools.xdftool`.
//!
//! Fixtures are generated into a temporary directory at test time and
//! deleted afterwards. Nothing is checked in: a committed ADF is a
//! binary blob nobody can review, and the whole point is that the bytes
//! come from the other implementation *now*, not from whatever version
//! produced them once.
//!
//! # What is covered, and what is not
//!
//! All eight variants `DOS\0`–`DOS\7` are generated and read: both
//! filesystems, both fold tables, dircaches and long names. Long names
//! (>30 bytes) are exercised on `DOS\6`/`DOS\7` only, because xdftool
//! correctly refuses them elsewhere — which is itself asserted, since a
//! writer that accepted one would be writing a volume no AmigaDOS could
//! read.
//!
//! Not covered: **comments**. `xdftool`'s `comment` command fails with a
//! `TypeError` before it writes anything (amitools 0.7.x), so there is no
//! way to make it produce a volume with a comment on it. Comments are
//! covered against synthetic volumes in `volumes.rs` instead, in both
//! layouts including the `T_COMMENT` overflow block.
//!
//! Also not covered: block sizes other than 512. `xdftool`'s ADF and HDF
//! images are 512-byte-blocked, and the RDB-partitioned images where
//! other block sizes live are `rdbtool`'s territory and this crate's
//! sibling's problem.

use std::path::{Path, PathBuf};
use std::process::Command;

use amiga_ffs::*;

// ---------------------------------------------------------------------------
// Finding the oracle
// ---------------------------------------------------------------------------

/// How to invoke xdftool, if it can be invoked at all.
fn xdftool() -> Option<Vec<String>> {
    if let Ok(explicit) = std::env::var("AMIGA_FFS_XDFTOOL") {
        return Some(vec![explicit]);
    }
    let candidates = [
        vec!["xdftool".to_string()],
        vec![
            "python3".to_string(),
            "-m".to_string(),
            "amitools.tools.xdftool".to_string(),
        ],
    ];
    candidates.into_iter().find(|argv| {
        Command::new(&argv[0])
            .args(&argv[1..])
            .arg("--help")
            .output()
            .map_or(false, |o| o.status.success())
    })
}

/// Run xdftool, returning its stdout, or `None` with the reason printed.
/// A failed *command* is a hard failure — the tool is present, so it
/// working is now this test's business.
fn run(argv: &[String], args: &[&str]) -> String {
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .args(args)
        .output()
        .expect("xdftool was found a moment ago");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "xdftool {args:?} failed:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // xdftool exits 0 on some filesystem errors and reports them on
    // stdout, so the text has to be checked too.
    assert!(
        !stdout.contains("FSError") && !stdout.contains("Traceback"),
        "xdftool {args:?} reported an error:\n{stdout}"
    );
    stdout
}

/// A scratch directory that removes itself. No `tempfile` dependency for
/// a crate whose whole point is having none.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "amiga-ffs-diff-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).expect("scratch dir");
        Self(p)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// A file-backed BlockSource
// ---------------------------------------------------------------------------

/// The whole image in memory. An ADF is 880 KB; streaming it would buy
/// nothing and cost a `Seek` impl.
struct ImageDisk {
    bs: usize,
    data: Vec<u8>,
}

impl ImageDisk {
    fn open(path: &Path) -> Self {
        Self {
            bs: 512,
            data: std::fs::read(path).expect("image file"),
        }
    }
}

#[derive(Debug)]
struct ImageError(String);

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ImageError {}

impl BlockSource for ImageDisk {
    type Error = ImageError;

    fn block_size(&self) -> usize {
        self.bs
    }

    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ImageError> {
        let off = lba as usize * self.bs;
        if off + self.bs > self.data.len() {
            return Err(ImageError(format!("lba {lba} past end of image")));
        }
        buf.copy_from_slice(&self.data[off..off + self.bs]);
        Ok(())
    }

    fn block_count(&self) -> Option<u64> {
        Some(self.data.len() as u64 / self.bs as u64)
    }
}

// ---------------------------------------------------------------------------
// The fixture tree
// ---------------------------------------------------------------------------

/// A byte pattern with no short period, so a chain read out of order or
/// off by a block cannot round-trip.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + i / 251) % 251) as u8).collect()
}

/// A file the fixture contains: its Amiga path and its bytes.
fn fixture_files(long_names: bool) -> Vec<(String, Vec<u8>)> {
    let mut files = vec![
        (
            "S/Startup-Sequence".to_string(),
            b"Echo \"Hello\"\n".to_vec(),
        ),
        ("Devs/system-configuration".to_string(), pattern(232)),
        // 40000 bytes is 79 FFS data blocks at 512 -- more than the 72 a
        // header's table holds, so this file *must* cross an extension
        // block. On OFS it is 82 blocks, and crosses one too.
        ("big.dat".to_string(), pattern(40_000)),
        ("Devs/Keymaps/gb".to_string(), pattern(1000)),
        ("empty".to_string(), Vec::new()),
    ];
    if long_names {
        // 50 bytes: past the classic 30-byte limit, well inside LNFS's
        // 107. The name that a classic-offset reader returns as empty.
        files.push((
            "a-file-name-of-fifty-characters-for-the-lnfs-tests".to_string(),
            pattern(777),
        ));
    }
    files
}

const FIXTURE_DIRS: [&str; 3] = ["Devs", "Devs/Keymaps", "S"];

/// Build one variant's image with xdftool and hand back its path.
fn build_image(argv: &[String], scratch: &Scratch, variant: Variant, label: &str) -> PathBuf {
    let name = format!("dos{}.adf", variant.dostype() & 0xFF);
    let image = scratch.path(&name);
    let image_s = image.to_str().unwrap().to_string();
    let dostype = format!("DOS{}", variant.dostype() & 0xFF);

    let mut args: Vec<String> = vec!["-f".into(), image_s, "format".into(), label.into(), dostype];
    for dir in FIXTURE_DIRS {
        args.push("+".into());
        args.push("makedir".into());
        args.push(dir.into());
    }
    for (path, bytes) in fixture_files(variant.has_long_names()) {
        let host = scratch.path(&path.replace('/', "_"));
        std::fs::write(&host, &bytes).expect("write host file");
        args.push("+".into());
        args.push("write".into());
        args.push(host.to_str().unwrap().to_string());
        args.push(path);
    }
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run(argv, &refs);
    image
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// Print the reason and skip. Returns the tool when it is there.
macro_rules! oracle {
    () => {
        match xdftool() {
            Some(t) => t,
            None => {
                eprintln!(
                    "SKIP: xdftool not found. Install amitools (`pipx install amitools`) or set \
                     AMIGA_FFS_XDFTOOL to run the differential suite."
                );
                return;
            }
        }
    };
}

/// Every entry the crate finds under `dir`, as `(path, kind, size)`,
/// recursively, sorted — the shape a tree comparison needs.
fn walk(vol: &mut Volume<ImageDisk>, dir: u64, prefix: &str) -> Vec<(String, EntryKind, u32)> {
    let mut out = Vec::new();
    for entry in vol.read_dir(dir).expect("read_dir") {
        let name = String::from_utf8(entry.name.clone()).expect("latin-1 name is ascii here");
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if entry.kind.is_directory() {
            out.extend(walk(vol, entry.lba, &path));
        }
        out.push((path, entry.kind, entry.byte_size));
    }
    out.sort();
    out
}

#[test]
fn every_variant_xdftool_can_write_reads_back_identically() {
    let argv = oracle!();
    let scratch = Scratch::new("tree");

    for byte in 0u32..=7 {
        let variant = Variant::from_dostype(0x444F_5300 | byte).unwrap();
        let label = format!("Vol{byte}");
        let image = build_image(&argv, &scratch, variant, &label);

        let mut vol = Volume::open(ImageDisk::open(&image), None).unwrap();
        assert_eq!(vol.variant(), variant, "dostype from the boot block");
        assert_eq!(vol.root().name, label.as_bytes(), "{variant:?}");
        assert_eq!(vol.block_size(), 512);

        let root = vol.root_lba();
        let found = walk(&mut vol, root, "");

        let mut want: Vec<(String, EntryKind, u32)> = FIXTURE_DIRS
            .iter()
            .map(|d| (d.to_string(), EntryKind::Directory, 0))
            .collect();
        for (path, bytes) in fixture_files(variant.has_long_names()) {
            want.push((path, EntryKind::File, bytes.len() as u32));
        }
        want.sort();
        assert_eq!(found, want, "{variant:?} tree");

        // Not one empty name, on any variant. The regression this crate
        // exists to not have, asserted against bytes it did not write.
        for (path, _, _) in &found {
            assert!(!path.is_empty());
            assert!(
                !path.ends_with('/'),
                "{variant:?}: empty component in {path}"
            );
        }

        // Every byte of every file, through the chain the other
        // implementation laid down -- including one that crosses an
        // extension block, on FFS and OFS alike.
        for (path, bytes) in fixture_files(variant.has_long_names()) {
            let entry = vol
                .lookup_path(root, path.as_bytes())
                .unwrap()
                .unwrap_or_else(|| panic!("{variant:?}: {path} not found"));
            assert_eq!(
                vol.read_file(entry.lba).unwrap(),
                bytes,
                "{variant:?}: contents of {path}"
            );
        }

        // Lookup under the volume's own fold table finds what xdftool
        // wrote, whatever case it is asked in.
        assert!(vol.lookup_path(root, b"devs/KEYMAPS/gb").unwrap().is_some());
    }
}

#[test]
fn images_from_another_implementation_validate_clean() {
    let argv = oracle!();
    let scratch = Scratch::new("validate");

    for byte in 0u32..=7 {
        let variant = Variant::from_dostype(0x444F_5300 | byte).unwrap();
        let image = build_image(&argv, &scratch, variant, "Checked");
        let mut vol = Volume::open(ImageDisk::open(&image), None).unwrap();
        let report = vol.validate();

        // The strongest single statement this suite makes: an
        // independent implementation's idea of a correct volume passes
        // every check this crate's validator applies -- checksums, hash
        // slots, parent pointers, own keys, dircache agreement, and the
        // bitmap in both directions.
        assert!(
            report.is_clean(),
            "{variant:?}: {:#?}",
            report
                .findings
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
        );
        let s = report.summary;
        assert_eq!(s.directories, 1 + FIXTURE_DIRS.len() as u64, "{variant:?}");
        assert_eq!(
            s.files,
            fixture_files(variant.has_long_names()).len() as u64,
            "{variant:?}"
        );
        assert_eq!(s.reachable, s.allocated, "{variant:?}");
        assert_eq!(s.orphans, 0);
        assert_eq!(s.reachable_but_free, 0);
        // 40000 bytes needs more than one table's worth of pointers on
        // every variant, so an extension block is always there.
        assert!(s.extension_blocks >= 1, "{variant:?}");
        assert_eq!(
            s.dircache_blocks > 0,
            variant.has_dircache(),
            "{variant:?} dircache blocks"
        );
        assert_eq!(s.bitmap_blocks, 1, "one page covers a 1760-block floppy");
    }
}

#[test]
fn the_bitmap_agrees_with_the_writers_own_accounting() {
    let argv = oracle!();
    let scratch = Scratch::new("bitmap");
    let image = build_image(&argv, &scratch, Variant::FfsIntl, "Counted");

    // xdftool's own `info` prints the block counts it believes.
    let info = run(&argv, &[image.to_str().unwrap(), "info"]);
    let used: u64 = info
        .split_whitespace()
        .skip_while(|w| *w != "used:")
        .nth(1)
        .expect("xdftool info prints a used count")
        .parse()
        .expect("a number");

    let mut vol = Volume::open(ImageDisk::open(&image), None).unwrap();
    let bm = vol.read_bitmap().unwrap();
    assert!(bm.valid());
    // xdftool counts the two boot blocks as used; the bitmap has no bit
    // for them, which is the whole reason `is_allocated` returns `None`
    // there rather than `Some(true)`.
    assert_eq!(bm.allocated_count() + 2, used, "\n{info}");
    assert_eq!(bm.allocated_count() + bm.free_count(), 1758);
    assert!(bm.covers_whole_volume());
    assert!(!bm.covers(0) && !bm.covers(1));

    // And every block the walk reaches is one the bitmap marks used.
    let report = vol.validate();
    assert!(report.is_clean(), "{:?}", report.findings);
    assert_eq!(report.summary.allocated + 2, used);
}

#[test]
fn a_dircache_written_by_another_implementation_agrees_with_its_own_chains() {
    let argv = oracle!();
    let scratch = Scratch::new("dircache");

    for variant in [Variant::OfsIntlDircache, Variant::FfsIntlDircache] {
        let image = build_image(&argv, &scratch, variant, "Cached");
        let mut vol = Volume::open(ImageDisk::open(&image), None).unwrap();
        let root = vol.root_lba();

        let cache = vol.read_dircache(root).unwrap();
        assert!(!cache.blocks.is_empty(), "{variant:?} has a dircache");
        let mut cached: Vec<String> = cache
            .records
            .iter()
            .map(|r| String::from_utf8(r.name.clone()).unwrap())
            .collect();
        cached.sort();

        let mut listed: Vec<String> = vol
            .read_dir(root)
            .unwrap()
            .into_iter()
            .map(|e| String::from_utf8(e.name).unwrap())
            .collect();
        listed.sort();
        assert_eq!(cached, listed, "{variant:?}: cache vs chains");

        // Sizes agree too -- and the record layout's word alignment is
        // what makes the second and later records land where they do, so
        // getting this list right at all is the padding rule confirmed
        // against a writer that is not this crate.
        for record in &cache.records {
            let entry = vol.entry_at(record.entry as u64).unwrap();
            assert_eq!(record.size, entry.byte_size, "{:?}", record.name);
            assert_eq!(record.name, entry.name);
        }

        // The subdirectories have their own caches, chained off their own
        // longword -2.
        let devs = vol.lookup(root, b"Devs").unwrap().unwrap();
        assert!(vol.dircache_head(devs.lba).unwrap() != 0);
        assert_eq!(vol.read_dircache(devs.lba).unwrap().records.len(), 2);
    }
}

#[test]
fn xdftool_refuses_a_long_name_on_a_variant_that_cannot_store_it() {
    // Not a test of this crate: a test that the *oracle* enforces the
    // same 30-byte limit, which is what makes its `DOS\6`/`DOS\7` images
    // evidence about long names rather than evidence about nothing.
    let argv = oracle!();
    let scratch = Scratch::new("toolong");
    let image = scratch.path("classic.adf");
    let host = scratch.path("payload");
    std::fs::write(&host, b"x").unwrap();
    let long = "a-file-name-of-fifty-characters-for-the-lnfs-tests";
    assert!(long.len() > MAX_NAME_CLASSIC);

    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .args([
            "-f",
            image.to_str().unwrap(),
            "format",
            "Classic",
            "DOS3",
            "+",
            "write",
            host.to_str().unwrap(),
            long,
        ])
        .output()
        .expect("run xdftool");
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("Invalid File Name"),
        "xdftool accepted a {}-byte name on DOS\\3:\n{text}",
        long.len()
    );

    // This crate refuses to even ask for it, one layer earlier.
    let mut vol = Volume::open(ImageDisk::open(&image), None).unwrap();
    let root = vol.root_lba();
    assert!(matches!(
        vol.lookup(root, long.as_bytes()),
        Err(Error::NameTooLong { max: 30, .. })
    ));
}
