//! Homebrew on a mac, the way `vagrant.rs` drives apt on a Debian guest.
//!
//! Deliberately **not** escalated: Homebrew refuses to run as root, so this
//! is the one playbook here whose ops need the login user. Run it twice; the
//! second run reports `ok`.
//!
//!     rustible playbook run macbrew
//!     rustible playbook run macbrew --var present=false
//!
//! The removal first gives the formula a second version, a copy of its keg,
//! which `brew::Absent` must take too, and pins it, which `brew::Absent` must
//! refuse until it is unpinned. A removal run with the formula installed
//! reports four `changed` and one `recovered`; the one after it, `ok`.

use std::path::Path;

use rustible::prelude::*;
use rustible_std::{brew, shell};

/// The version the copied keg is filed under. Any directory in a rack is a
/// version to Homebrew; this one cannot be mistaken for a real release.
const COPY: &str = "0.0.0-rustible";

#[rustible::vars]
struct Vars {
    /// A small curses game, chosen because it is quick to build and has no
    /// service to leave running.
    #[default = "ninvaders"]
    package: String,
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
    }
    Ok(())
}

/// While `package` is installed: copy its keg to a second version, so the
/// removal that follows must take both, and pin it, to see `brew::Absent`
/// refuse it, then unpin it. With nothing installed, which is the second
/// removal run, it does nothing at all.
fn second_version_and_pin(ctx: &mut Ctx, package: &str) -> Result<()> {
    let mut bin = None;
    for candidate in ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"] {
        if ctx.sys().exists(candidate)? {
            bin = Some(candidate);
            break;
        }
    }
    let Some(bin) = bin else {
        bail!("no `brew` at /opt/homebrew/bin or /usr/local/bin");
    };
    // The rack is `<prefix>/Cellar/<name>`, the prefix two directories up
    // from `brew`, which is where `brew::Absent` reads it too.
    let prefix = Path::new(bin).parent().and_then(Path::parent);
    let rack = prefix.unwrap_or(Path::new("/")).join("Cellar").join(package);
    if !ctx.sys().exists(&rack)? {
        return Ok(());
    }
    let Some(keg) = ctx
        .sys()
        .read_dir(&rack)?
        .into_iter()
        .find(|v| !v.ends_with(COPY))
    else {
        bail!("{} has no version directory to copy", rack.display());
    };
    let copy = rack.join(COPY);
    ctx.step(
        format!("{package} gets a second version"),
        shell::Command::new("/bin/cp")
            .arg("-R")
            .arg(keg.display().to_string())
            .arg(copy.display().to_string())
            .creates(&copy),
    )?;
    ctx.step(
        format!("{package} pinned"),
        shell::Command::new(bin).args(["pin", package]),
    )?;
    let refused = ctx.step(
        format!("{package} absent, while pinned"),
        brew::Absent::new([package]),
    );
    // Unpinned whatever that step did, so a failure below leaves no pin
    // behind on the machine.
    ctx.step(
        format!("{package} unpinned"),
        shell::Command::new(bin).args(["unpin", package]),
    )?;
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
    Ok(())
}
