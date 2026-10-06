//! Homebrew on a mac, the way `vagrant.rs` drives apt on a Debian guest.
//!
//! Deliberately **not** escalated: Homebrew refuses to run as root, so this
//! is the one playbook here whose ops need the login user. Run it twice; the
//! second run reports `ok`.
//!
//!     rustible playbook run macbrew
//!     rustible playbook run macbrew --var present=false
//!
//! A second formula is named by an alias, which has no rack of its own:
//! `brew::Present` must find it through `<prefix>/opt/<alias>` on the second
//! run, and `brew::Absent` must remove it by its rack's name.
//!
//! The removal first gives the formula a second version, a copy of its keg,
//! which `brew::Absent` must take too, and pins it, which `brew::Absent` must
//! refuse until it is unpinned. A removal run with both installed reports
//! five `changed` and one `recovered`; the one after it, `ok`.
//!
//! Each install run also moves a 50 MiB file through `ctx.as_root()`, which
//! from this unescalated playbook is the sudo helper: it is generated as
//! root under a scratch directory, copied there with `file::Copy` (the
//! second run's `check` reads both sides back through the helper to report
//! `ok`), and fetched to the workspace's ignored `out/macbrew/`, where CI
//! compares it with what was generated and removes it (#86). The mac's own
//! `tar` then makes a tarball of it, plain (over 48 MiB) and gzipped, and
//! each is extracted as root, so the archive's reads and the member's
//! writes alternate on the helper chunk by chunk (#88). A removal run
//! removes the scratch directory.

use std::path::{Component, Path, PathBuf};

use rustible::prelude::*;
use rustible::sdk::backend::FileKind;
use rustible_std::{archive, brew, file, shell};

/// The version the copied keg is filed under. Any directory in a rack is a
/// version to Homebrew; this one cannot be mistaken for a real release.
const COPY: &str = "0.0.0-rustible";

/// Where the 50 MiB file and its copy live, root's, on this mac.
const SCRATCH: &str = "/private/tmp/rustible-macbrew";

/// Its size and the line it repeats, which CI regenerates to compare the
/// fetched copy with.
const LARGE_BYTES: u64 = 50 * 1024 * 1024;
const LARGE_LINE: &str = "rustible streams this line";

#[rustible::vars]
struct Vars {
    /// A small curses game, chosen because it is quick to build and has no
    /// service to leave running.
    #[default = "ninvaders"]
    package: String,
    /// A formula named by an alias: `6tunnel` is `sixtunnel`'s, recorded in
    /// its receipt, so brew links `opt/6tunnel` to the keg. Chosen for a
    /// bottle of about 20 KB with no dependencies and no service.
    #[default = "6tunnel"]
    alias: String,
    /// False removes it again, which is how the `Absent` path is exercised.
    #[default = true]
    present: bool,
}

#[rustible::playbook(hosts = "mac", vars = Vars)]
fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> {
    let f = ctx.facts();
    ensure!(
        f.has_pm(&Pm::Brew),
        "this playbook needs Homebrew; {} has {:?}",
        f.hostname,
        f.package_managers
    );
    ensure!(
        !f.is_root,
        "run this playbook unescalated: Homebrew refuses to run as root"
    );
    ctx.log(format!(
        "{} is {:?} {} on {:?}, {} cpus, {} MB, pm {:?}, init {:?}",
        f.hostname, f.distro, f.distro_version, f.arch, f.cpus, f.memory_mb, f.package_managers, f.init
    ));

    if vars.present {
        let out = ctx.step(
            format!("{} present", vars.package),
            brew::Present::new([vars.package.as_str()]),
        )?;
        if !ctx.check_mode() {
            ctx.log(format!(
                "installed {:?}, already there {:?}",
                out.installed, out.already_present
            ));
        }
        let out = ctx.step(
            format!("{} present, by its alias", vars.alias),
            brew::Present::new([vars.alias.as_str()]),
        )?;
        if !ctx.check_mode() {
            ctx.log(format!(
                "installed {:?}, already there {:?}",
                out.installed, out.already_present
            ));
        }
        large_file_as_root(ctx)?;
    } else {
        if !ctx.check_mode() {
            second_version_and_pin(ctx, &vars.package)?;
        }
        let out = ctx.step(
            format!("{} absent", vars.package),
            brew::Absent::new([vars.package.as_str()]),
        )?;
        if !ctx.check_mode() {
            ctx.log(format!("removed {:?}", out.removed));
        }
        let out = ctx.step(
            format!("{} absent, by its alias", vars.alias),
            brew::Absent::new([vars.alias.as_str()]),
        )?;
        if !ctx.check_mode() {
            ctx.log(format!("removed {:?}", out.removed));
        }
        ctx.as_root().step(
            format!("{SCRATCH} removed"),
            file::Absent::at(SCRATCH).recursive(true),
        )?;
    }
    Ok(())
}

/// 50 MiB through the sudo helper, both ways (decision 30 on #82): one
/// frame of the helper carries less, so the copy, its comparison, the fetch
/// and the extractions all cross it in chunks. Generated under a temporary
/// name, so a generation cut short is not mistaken for a finished one by
/// `creates`.
fn large_file_as_root(ctx: &mut Ctx) -> Result<()> {
    let large = format!("{SCRATCH}/large");
    let copy = format!("{SCRATCH}/large-copy");
    let mut root = ctx.as_root();
    root.step(
        "50 MiB file, as root",
        shell::Command::sh(format!(
            "mkdir -p {SCRATCH} && yes '{LARGE_LINE}' | head -c {LARGE_BYTES} > {large}.part \
             && mv {large}.part {large}"
        ))
        .creates(&large),
    )?;
    // A dry run on a mac that never had it has no source to compare.
    if root.check_mode() && !root.sys().exists(&large)? {
        return Ok(());
    }
    root.step(
        "50 MiB copy, as root",
        file::Copy::from_local_path(&large).to(&copy),
    )?;
    if !root.check_mode() {
        root.fetch(&copy, "out/macbrew/")?;
    }
    // The file as a tarball member, made by the mac's own `tar` (bsdtar;
    // `COPYFILE_DISABLE` keeps AppleDouble `._` members out): plain, over
    // 48 MiB, and gzipped. `creates` makes the second run `ok`.
    for (tarball, z) in [("large.tar", ""), ("large.tar.gz", "z")] {
        let tarball = format!("{SCRATCH}/{tarball}");
        root.step(
            format!("tarball {tarball}, as root"),
            shell::Command::sh(format!(
                "mkdir -p {SCRATCH}/tree && ln -f {large} {SCRATCH}/tree/large \
                 && COPYFILE_DISABLE=1 tar -C {SCRATCH} -c{z}f {tarball}.part tree \
                 && mv {tarball}.part {tarball}"
            ))
            .creates(&tarball),
        )?;
        if root.check_mode() && !root.sys().exists(&tarball)? {
            continue;
        }
        let dest = format!("{tarball}.d");
        root.step(format!("{dest}, as root"), file::Directory::at(&dest))?;
        root.step(
            format!("50 MiB member of {tarball}, as root"),
            archive::Extracted::from_path(&tarball)
                .to(&dest)
                .creates("tree/large"),
        )?;
        if !root.check_mode() {
            root.sys()
                .cmd("cmp")
                .args([large.clone(), format!("{dest}/tree/large")])
                .run()
                .with_context(|| format!("the member extracted from {tarball} is not the 50 MiB file"))?;
        }
    }
    Ok(())
}

/// While `package` is installed with one version: copy its keg to a second
/// version, so the removal that follows must take both, and pin it, to see
/// `brew::Absent` refuse it, then unpin it. With nothing installed, which is
/// the second removal run, it does nothing at all; nor with the copy already
/// there, which is a removal that left it behind, so `brew::Absent` itself
/// reports `changed` and says so.
fn second_version_and_pin(ctx: &mut Ctx, package: &str) -> Result<()> {
    let Some(rack) = rack(ctx, package)? else {
        return Ok(());
    };
    let copy = rack.join(COPY);
    if ctx.sys().stat(&copy)?.is_some() {
        return Ok(());
    }
    let mut keg = None;
    for version in ctx.sys().read_dir(&rack)? {
        if is_dir(ctx, &version)? {
            keg = Some(version);
            break;
        }
    }
    let Some(keg) = keg else {
        bail!("{} has no version directory to copy", rack.display());
    };
    ctx.step(
        format!("{package} gets a second version"),
        shell::Command::new("/bin/cp")
            .arg("-R")
            .arg(keg.display().to_string())
            .arg(copy.display().to_string()),
    )?;
    let bin = brew_bin(ctx)?;
    ctx.step(
        format!("{package} pinned"),
        shell::Command::new(bin).args(["pin", "--formula", package]),
    )?;
    let refused = ctx.step(
        format!("{package} absent, while pinned"),
        brew::Absent::new([package]),
    );
    // Unpinned whatever that step did, so a wrong refusal leaves no pin
    // behind; its `Result` waits until the refusal is judged, so that a
    // removal that went through anyway is what gets reported, rather than
    // `brew unpin` failing on a formula no longer installed.
    let unpinned = ctx.step(
        format!("{package} unpinned"),
        shell::Command::new(bin).args(["unpin", "--formula", package]),
    );
    match refused {
        Ok(_) => bail!("brew::Absent removed `{package}` while it was pinned"),
        Err(e) => {
            let chain = e.chain();
            ensure!(
                chain.contains(&format!("`{package}` is pinned (`brew pin`)")),
                "brew::Absent should refuse a pinned `{package}` naming the pin; it said: {chain}"
            );
            ctx.log(format!("brew::Absent refused the pinned {package}"));
        }
    }
    unpinned?;
    Ok(())
}

/// The `brew` on this mac, at the two places Homebrew installs it there.
fn brew_bin(ctx: &Ctx) -> Result<&'static str> {
    for bin in ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"] {
        if ctx.sys().exists(bin)? {
            return Ok(bin);
        }
    }
    bail!("no `brew` at /opt/homebrew/bin or /usr/local/bin");
}

/// `package`'s rack, if it is a directory, in the Cellar `brew::Absent`
/// reads: `<repository>/Cellar` when that is a directory, the repository
/// being two directories up from the target of `brew` when `brew` is a
/// symlink (`/usr/local/bin/brew -> ../Homebrew/bin/brew`), and otherwise
/// `<prefix>/Cellar`, the prefix two directories up from `brew` itself.
fn rack(ctx: &Ctx, package: &str) -> Result<Option<PathBuf>> {
    let bin = Path::new(brew_bin(ctx)?);
    let up2 = |p: &Path| p.parent().and_then(Path::parent).map(Path::to_path_buf);
    let mut cellars = vec![];
    if ctx.sys().stat(bin)?.is_some_and(|s| s.kind == FileKind::Symlink) {
        let target = bin.parent().unwrap_or(Path::new("/")).join(ctx.sys().read_link(bin)?);
        let mut resolved = PathBuf::new();
        for c in target.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                other => resolved.push(other),
            }
        }
        cellars.extend(up2(&resolved).map(|repository| repository.join("Cellar")));
    }
    cellars.extend(up2(bin).map(|prefix| prefix.join("Cellar")));
    for cellar in cellars {
        if is_dir(ctx, &cellar)? {
            let rack = cellar.join(package);
            return Ok(is_dir(ctx, &rack)?.then_some(rack));
        }
    }
    Ok(None)
}

/// A directory, or a symlink to one; never `.DS_Store` or another file.
fn is_dir(ctx: &Ctx, p: &Path) -> Result<bool> {
    Ok(ctx
        .sys()
        .stat_follow(p)?
        .is_some_and(|s| s.kind == FileKind::Dir))
}
