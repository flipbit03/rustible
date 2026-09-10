//! The global `--inventory` flag, which overrides the workspace's own
//! `inventory` setting. It exists so a run can target machines that are not
//! in the committed inventory: `dev/vagrant/Vagrantfile` writes one for the
//! Vagrant guests, whose addresses and key paths belong to one developer's
//! disk and are therefore not committed.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn rustible(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rustible"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("running rustible")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A workspace whose own `hosts.kdl` names one host, plus a second inventory
/// file beside it naming a different one.
fn workspace(dir: &Path) {
    fs::write(dir.join("rustible.toml"), "").unwrap();
    fs::write(
        dir.join("hosts.kdl"),
        "host \"committed\" addr=\"10.0.0.1\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("other.kdl"),
        "host \"elsewhere\" addr=\"10.0.0.2\"\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("playbooks")).unwrap();
}

#[test]
fn without_the_flag_the_workspace_inventory_is_used() {
    let tmp = tempfile::tempdir().unwrap();
    workspace(tmp.path());
    let out = rustible(tmp.path(), &["inventory", "show", "committed"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("10.0.0.1"), "{}", stdout(&out));
}

#[test]
fn the_flag_replaces_the_workspace_inventory() {
    let tmp = tempfile::tempdir().unwrap();
    workspace(tmp.path());
    let out = rustible(
        tmp.path(),
        &["--inventory", "other.kdl", "inventory", "show", "elsewhere"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("10.0.0.2"), "{}", stdout(&out));

    // And the committed inventory's host is then genuinely not visible,
    // rather than the two being merged.
    let out = rustible(
        tmp.path(),
        &["--inventory", "other.kdl", "inventory", "show", "committed"],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("committed"), "{}", stderr(&out));
}

#[test]
fn the_subcommands_own_file_flag_wins_over_the_global_one() {
    let tmp = tempfile::tempdir().unwrap();
    workspace(tmp.path());
    let out = rustible(
        tmp.path(),
        &[
            "--inventory",
            "other.kdl",
            "inventory",
            "show",
            "--file",
            "hosts.kdl",
            "committed",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("10.0.0.1"), "{}", stdout(&out));
}

#[test]
fn a_missing_inventory_file_is_reported_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    workspace(tmp.path());
    let out = rustible(
        tmp.path(),
        &["--inventory", "nope.kdl", "inventory", "check"],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("nope.kdl"), "{}", stderr(&out));
}

/// Every subcommand that does not resolve hosts refuses the flag rather than
/// accepting and ignoring it. `--inventory` is global so clap offers it
/// everywhere; a flag that silently does nothing is how a run against the
/// wrong machines gets reported as a success.
#[test]
fn subcommands_that_read_no_inventory_refuse_the_flag() {
    let tmp = tempfile::tempdir().unwrap();
    workspace(tmp.path());
    for cmd in [
        vec!["playbook", "list"],
        vec!["playbook", "create", "playbooks/x.rs"],
        vec!["toolchain", "check"],
    ] {
        let mut args = vec!["--inventory", "other.kdl"];
        args.extend_from_slice(&cmd);
        let out = rustible(tmp.path(), &args);
        let label = cmd.join(" ");
        assert_eq!(
            out.status.code(),
            Some(3),
            "`{label}` should have refused --inventory: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("--inventory"),
            "`{label}`: {}",
            stderr(&out)
        );
    }
}

#[test]
fn init_refuses_the_flag_rather_than_ignoring_it() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("new");
    let out = rustible(
        tmp.path(),
        &["--inventory", "other.kdl", "init", target.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(stderr(&out).contains("--inventory"), "{}", stderr(&out));
    assert!(!target.exists(), "init must not have created anything");
}
