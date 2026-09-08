# M6 report: `apt::{Absent, Latest}`, `apt::Present::update_cache(Duration)`, `hostname::Is`, `sysctl::Present`

**Branch:** `m6-small-ops`. **Run:** unattended run 1, 2026-09-08, subagent.
**Brief:** the `apt`, `hostname` and `sysctl` rows of `docs/plan/M6.md` plus
the wave-one template in `docs/06_BUILD_PLAN.md` section 4; governing vision
sections 6.2, 6.3, 6.7, 6.8, 7, 8, 12, 13.
**Done-when log:** `docs/plan/logs/M6-small-ops-done.txt`.
**Tests:** 67 in the three modules (63 new; the four pre-existing
`apt::Present` tests stay, one adjusted to assert that no `stat` or
`apt-get update` runs without `update_cache`).

## What was built

### `crates/rustible-std/src/apt.rs` (extended)

1. **Pure parsers**, all public and string-tested: `parse_dpkg_status`
   (`${Status}\t${Version}` into `DpkgStatus::{Unknown, Installed,
   ConfigFiles, Other}`), `parse_policy` (`Installed:` and `Candidate:` lines
   of `apt-cache policy`, `(none)` and empty output both mapping to `None`),
   `parse_stat_mtime`.
2. **`Present::update_cache(Duration)`** replaces `update_cache(bool)`.
   `apply` (only reached when something is missing) runs `apt-get update`
   when the lists are older than the given max age; `Duration::ZERO` always
   updates. The age is the mtime of `/var/lib/apt/lists`, falling back to
   `/var/cache/apt/pkgcache.bin`, read with `stat -c %Y` through `sys.cmd`;
   when neither can be read the cache counts as stale.
3. **`Absent::new([..]).purge(bool).autoremove(bool)`** → `RemoveReport {
   removed: Vec<Package>, not_present: Vec<String> }`. `check` asks
   `dpkg-query` per name: `install ok installed` is present; `deinstall ok
   config-files` is present only with `purge` (so a plain `Absent` after a
   plain remove is `ok`, a purging one is `changed`); any other dpkg state
   fails the step naming it. `apply` runs `apt-get remove -y <names>` (or
   `purge -y`), then `apt-get autoremove -y` (`--purge` when purging) when
   asked and something was removed. Versions are known from dpkg, so the
   prediction is exact and `apply` returns it.
4. **`Latest::new([..]).update_cache(Duration).install_recommends(bool)`** →
   `UpgradeReport { upgraded: Vec<(Package, String)>, installed:
   Vec<Package>, current: Vec<Package> }`. `check` compares the installed
   version from `dpkg-query` with the candidate from `apt-cache policy`; a
   name with no candidate fails the step ("unknown name, or the lists need
   `apt-get update`"). `apply` refreshes the cache per `update_cache`, then
   `apt-get install -y [--no-install-recommends] <missing>` and `apt-get
   install -y --only-upgrade <outdated>` as two commands, and reports the
   versions dpkg has afterwards. Predicts with the candidate versions.
5. **`require_apt_root`**: every apt op refuses on a non-apt host
   (`facts.package_manager`) and without root, before running anything.
   `check` runs only `dpkg-query`, `apt-cache policy` and `stat`.
6. Module docs name `ansible.builtin.apt` and each `state`, and spell out
   what `update_cache` does and does not do in `check` (see Decisions).

### `crates/rustible-std/src/hostname.rs` (new)

`Is::new(name)` → `HostnameReport { previous, current }`. `validate_hostname`
(pure, public) accepts RFC 1123 labels of at most 63 characters joined by
dots, 253 in all, letters, digits and hyphens, no leading or trailing hyphen;
`check` fails on anything else. `check` reads `/etc/hostname` and
`/proc/sys/kernel/hostname` through `sys` (a missing file reads as
`absent`; an unreadable `/proc` falls back to `facts.hostname`) and is
satisfied when both equal the name; the diff lists whichever differ.
`apply` runs `hostnamectl set-hostname <name>` when `facts.init ==
Init::Systemd`, else writes `/etc/hostname` atomically and runs `hostname
<name>`. Needs root. `previous` is the kernel hostname before the step.
Rustdoc names `ansible.builtin.hostname`.

### `crates/rustible-std/src/sysctl.rs` (new)

`Present::new(key, value).file(path).apply_now(bool)` → `SysctlReport { key,
value, previous_live: Option<String>, file: PathBuf }`. Pure, public:
`validate_key` (`[A-Za-z0-9_./-]`), `proc_path` (dots to slashes under
`/proc/sys`), `normalize` (token-wise comparison), `parse_line` (comments,
blanks, `key=value` and `key = value`, sysctl's leading `-` marker),
`value_in` (last line wins, as sysctl reads it), and `plan_sysctl_line`
(rewrite the first line for the key in place, drop later duplicates, append
when missing, `None` when exactly one matching line already carries the
value). `check` reads the drop-in (default `/etc/sysctl.d/99-rustible.conf`;
missing file is fine, missing directory is refused) and the live value from
`/proc/sys/<key>`; changed when either differs. With `apply_now` (default
on) a key the kernel lacks is refused; with `.apply_now(false)` it is
persisted only and `previous_live` is `None`. `apply` writes the file
atomically when it differs and runs `sysctl -w key=value` when the live
value differs. Needs root; `check` runs no commands at all. Rustdoc names
`ansible.posix.sysctl`.

`crates/rustible-std/src/lib.rs` gains `pub mod hostname; pub mod sysctl;`.

## Usage

```rust
use std::time::Duration;
use rustible_std::{apt, hostname, sysctl};

ctx.step("Install nginx and curl",
    apt::Present::new(["nginx", "curl"]).update_cache(Duration::from_secs(3600)))?;
let gone = ctx.step("Remove apache2",
    apt::Absent::new(["apache2", "sendmail"]).purge(true).autoremove(true))?;
ctx.log(format!("removed {} package(s)", gone.removed.len()));
let up = ctx.step("Keep openssl current",
    apt::Latest::new(["openssl"]).update_cache(Duration::ZERO))?;
for (pkg, from) in &up.upgraded { ctx.log(format!("{}: {from} -> {}", pkg.name, pkg.version)); }

let name = ctx.step("Hostname", hostname::Is::new("HOME-GAMES"))?;
if name.changed { ctx.step("avahi restarted", systemd::Restart::new("avahi-daemon"))?; }

ctx.step("IP forwarding on", sysctl::Present::new("net.ipv4.ip_forward", "1"))?;
ctx.step("Persist only (container)",
    sysctl::Present::new("vm.swappiness", "10").file("/etc/sysctl.d/10-vm.conf").apply_now(false))?;
```

## Verified

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace`: clean (log).
- `cargo test -p rustible-std apt`: 31 tests. Pure: dpkg status variants
  (installed, config-files, empty, other, no tab), policy with both
  versions, `(none)` installed, unknown package (empty and `(none)`
  candidate), stat mtime. `Present`: the four existing tests; stale lists
  run `apt-get update` before `install` with the exact `stat` argv; fresh
  lists skip it; `Duration::ZERO` updates without `stat`; fallback to
  `pkgcache.bin`, and unknown age updates; non-root refusal runs nothing.
  `Absent`: satisfied when not installed; change with exact `apt-get remove
  -y apache2` and the mixed report; purge with `purge -y` and `autoremove -y
  --purge` in order; autoremove without purge; config-files present only
  when purging (rendered diff); half-configured refused; wrong pm and
  non-root refusals run nothing; apply without prediction refused; check
  mode through `ctx.step` predicts and runs only `dpkg-query`. `Latest`:
  satisfied at candidate; upgrade with exact `--only-upgrade` argv and the
  `(Package, from)` prediction; missing package installed with
  `--no-install-recommends`; mixed install, upgrade and current in two
  commands with `install_recommends(true)`; unknown package refused; wrong
  pm and non-root; `update` runs before `install` and not in `check`; check
  mode runs only `dpkg-query` and `apt-cache`.
- `cargo test -p rustible-std hostname`: 15 tests. Pure: valid names
  (uppercase, FQDN, digits, 63-char label) and invalid ones (empty,
  underscore, space, leading and trailing hyphen, empty label, 64-char
  label, over 253). Fake: satisfied; file-only and kernel-only changes with
  the rendered diff and `previous`; missing `/etc/hostname` as `absent`;
  facts fallback; systemd apply with exact `hostnamectl` argv and the file
  left to it; non-systemd apply writing the file and running `hostname`;
  creating a missing `/etc/hostname`; invalid name and non-root refusals;
  apply without prediction refused; check mode through `ctx.step` predicts,
  writes nothing, runs nothing; changed then ok across two steps.
- `cargo test -p rustible-std sysctl`: 21 tests. Pure: key validation,
  proc path, line parsing, append, replace in place, satisfied regardless
  of spacing and tabs, duplicate collapsing (and `value_in` last-wins),
  no prefix or comment match. Fake: satisfied; file-only change writes
  without `sysctl`; live-only change runs exact `sysctl -w`; both; persist
  only; missing `/proc` key refused unless persist-only; custom file;
  missing directory refused; token-wise multi-value comparison; bad key,
  newline value and non-root refusals; apply without prediction refused;
  check mode through `ctx.step` predicts, writes nothing, runs nothing;
  changed then ok across two steps.
- Container tests: not added. Branch `m6-harness` (PR #5) has not merged
  into `main` at the time of this PR, so there is no
  `#[rustible::integration_test]` to use. `TODO(M6 harness)` comments in
  `apt.rs` (install `sl`, `Absent` purge, changed then ok) and `sysctl.rs`
  (`.apply_now(false)`, assert the drop-in) name the tests to add. Not
  faked.
- Nothing was run against a real host: every apt, hostname and sysctl op
  needs root, and the protocol forbids sudo on this VM.

## Deviations

- The apt lists' age comes from `stat -c %Y` via `sys.cmd`, not `sys.stat`:
  the SDK's `Stat` has no mtime and the SDK is out of scope for this op.
- `hostname::Is::check` reads `/proc/sys/kernel/hostname` through `sys`
  rather than `sys.facts().hostname` (facts only as a fallback), so a second
  step in one run is `ok` after `apply`.
- `sysctl::Present` uses a local planner, not `file::plan_line`, because
  `key=value` and `key = value` must compare equal.
- `apt::Latest` also has `.install_recommends(bool)` (mirrors `Present`).
- No `update_cache_always()`; `update_cache(Duration::ZERO)` is the spelling.
- No container tests (see Verified).

## Decisions

Recorded in `docs/plan/DECISIONS.md` under `[M6-so]`:

- `update_cache(bool)` → `update_cache(Duration)`; no `update_cache_always()`.
- Lists' age via `stat -c %Y` through `sys.cmd` (no `Stat.mtime`; m6-file-ops
  is editing `backend/mod.rs`).
- `apt::Latest` refreshes in `apply` only (vision 6.8), so stale lists can
  make it report `ok`. **Proposed amendment for Cadu:** let `Latest::check`
  run `apt-get update` when the lists are older than `max_age`, as Ansible
  does in check mode; a command is not a file mutation under 7.3.
- All apt ops, `Present` included, refuse without root.
- `apt::Absent` fails on half-configured or unpacked packages instead of
  guessing.
- `apt::Latest::install_recommends(bool)`, default off.
- `hostname::Is` reads the live kernel name through `sys`; validation in
  `check`; dotted FQDNs and uppercase accepted.
- `sysctl::Present` has its own planner; refuses a missing drop-in
  directory and, when applying live, a missing `/proc/sys` key; `apply`
  re-plans from the current file (`Diff::Attrs`, not `Diff::Text`).

## Self-review

Run by the lead on the PR.
