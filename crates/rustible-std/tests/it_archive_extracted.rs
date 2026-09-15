//! Docker integration test for `archive::Extracted` (vision 8, tier 3).
//! Runs with `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_archive_extracted`.
//!
//! The four fixtures under `fixtures/archive/` are the same tree encoded
//! as tar, tar.gz, tar.xz and tar.zst; each is written into the container
//! and extracted with the pure-Rust decoders, no `tar` binary involved.

use rustible::prelude::*;
use rustible::sdk::testing::changed_then_ok;
use rustible_std::archive::{Extracted, Format};

const FIXTURES: [(&str, Format, &[u8]); 4] = [
    (
        "hello.tar",
        Format::Tar,
        include_bytes!("../fixtures/archive/hello.tar"),
    ),
    (
        "hello.tar.gz",
        Format::TarGz,
        include_bytes!("../fixtures/archive/hello.tar.gz"),
    ),
    (
        "hello.tar.xz",
        Format::TarXz,
        include_bytes!("../fixtures/archive/hello.tar.xz"),
    ),
    (
        "hello.tar.zst",
        Format::TarZst,
        include_bytes!("../fixtures/archive/hello.tar.zst"),
    ),
];

/// A one-member ustar archive holding `data` at `name` with `mode`, built
/// by hand so the test needs no tar encoder. Used for the setuid case: the
/// checked-in fixtures cannot carry a setuid bit through every tool that
/// touches them.
fn one_member_tar(name: &str, mode: u32, data: &[u8]) -> Vec<u8> {
    let mut h = [0u8; 512];
    let put = |h: &mut [u8; 512], at: usize, v: &[u8]| h[at..at + v.len()].copy_from_slice(v);
    put(&mut h, 0, name.as_bytes());
    put(&mut h, 100, format!("{mode:07o}\0").as_bytes());
    put(&mut h, 108, b"0000000\0"); // uid
    put(&mut h, 116, b"0000000\0"); // gid
    put(&mut h, 124, format!("{:011o}\0", data.len()).as_bytes());
    put(&mut h, 136, b"00000000000\0"); // mtime
    h[148..156].fill(b' '); // checksum field is spaces while summing
    h[156] = b'0'; // regular file
    put(&mut h, 257, b"ustar\0");
    put(&mut h, 263, b"00");
    let sum: u32 = h.iter().map(|b| *b as u32).sum();
    put(&mut h, 148, format!("{sum:06o}\0 ").as_bytes());

    let mut out = h.to_vec();
    out.extend_from_slice(data);
    out.extend(std::iter::repeat_n(0u8, (512 - data.len() % 512) % 512));
    out.extend(std::iter::repeat_n(0u8, 1024));
    out
}

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn extracted_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
    let work = "/opt/rustible-test";
    ctx.sys().mkdir_all(work)?;

    for (name, format, bytes) in FIXTURES {
        let src = format!("{work}/{name}");
        let dest = format!("{work}/{}", name.replace('.', "_"));
        ctx.sys().write_atomic(&src, bytes)?;
        ctx.sys().mkdir_all(&dest)?;

        // With the `creates` marker: extracted once, then `ok`.
        let (first, second) = changed_then_ok(ctx, &format!("extract {name}"), || {
            Extracted::from_path(&src)
                .to(&dest)
                .creates("hello/README.txt")
        })?;
        assert_eq!(first.format, Some(format));
        assert_eq!(
            (first.files, first.dirs, first.symlinks, first.bytes),
            (2, 3, 1, 38),
            "{name}"
        );
        assert!(first.extracted && !second.extracted);

        let sys = ctx.sys();
        assert_eq!(
            sys.read_to_string(format!("{dest}/hello/README.txt"))?,
            "hello from rustible\n"
        );
        assert_eq!(
            sys.stat(format!("{dest}/hello/README.txt"))?.unwrap().mode,
            0o644
        );
        assert_eq!(
            sys.read_to_string(format!("{dest}/hello/bin/run"))?,
            "#!/bin/sh\necho hi\n"
        );
        assert_eq!(
            sys.stat(format!("{dest}/hello/bin/run"))?.unwrap().mode,
            0o755
        );
        assert_eq!(
            sys.stat(format!("{dest}/hello/empty"))?.unwrap().kind,
            rustible::sdk::backend::FileKind::Dir
        );
        assert_eq!(
            sys.read_link(format!("{dest}/hello/link"))?,
            std::path::PathBuf::from("README.txt")
        );
        // The symlink resolves to the real file.
        assert_eq!(
            sys.read_to_string(format!("{dest}/hello/link"))?,
            "hello from rustible\n"
        );

        // The script is executable for real.
        let out = ctx.sys().cmd(format!("{dest}/hello/bin/run")).run()?;
        assert_eq!(out.stdout_str(), "hi\n");
    }

    // Without a marker every run changes (and re-extraction is harmless).
    let src = format!("{work}/hello.tar.gz");
    let dest = format!("{work}/stripped");
    ctx.sys().mkdir_all(&dest)?;
    let op = || {
        Extracted::from_path(&src)
            .to(&dest)
            .strip_components(1)
            .owner(65534, 65534)
    };
    assert!(op().always_changes());
    let a = ctx.step("extract stripped", op())?;
    let b = ctx.step("extract stripped again", op())?;
    assert!(a.changed && b.changed);
    assert_eq!(a.skipped, 1, "`hello/` itself has nothing left");
    let run = ctx.sys().stat(format!("{dest}/bin/run"))?.unwrap();
    assert_eq!((run.mode, run.uid, run.gid), (0o755, 65534, 65534));
    assert!(!ctx.sys().exists(format!("{dest}/hello"))?);

    // A setuid member keeps its bit when `.owner` is also set. On Linux
    // `chown(2)` clears setuid and setgid on anything that is not a
    // directory, so this only holds if the owner is applied before the
    // mode. With the two swapped the file below comes out 0o755 and the
    // step still reports success, which is the whole danger.
    let suid_src = format!("{work}/suid.tar");
    let suid_dest = format!("{work}/suid");
    ctx.sys().write_atomic(
        &suid_src,
        &one_member_tar("helper", 0o4755, b"#!/bin/sh\ntrue\n"),
    )?;
    ctx.sys().mkdir_all(&suid_dest)?;
    let r = ctx.step(
        "extract a setuid member with an owner",
        Extracted::from_path(&suid_src)
            .to(&suid_dest)
            .owner(65534, 65534),
    )?;
    assert!(r.changed);
    let helper = ctx.sys().stat(format!("{suid_dest}/helper"))?.unwrap();
    assert_eq!(
        (helper.mode, helper.uid, helper.gid),
        (0o4755, 65534, 65534),
        "chown must not have cleared the setuid bit"
    );

    // A refusal: the destination must exist (vision 6.7).
    let err = ctx
        .step(
            "extract into a missing directory",
            Extracted::from_path(&src).to("/opt/nope"),
        )
        .unwrap_err()
        .chain();
    assert!(
        err.contains("/opt/nope does not exist; create it first with file::Directory"),
        "{err}"
    );

    ctx.sys().remove_all(work)?;
    Ok(())
}
