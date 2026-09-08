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
