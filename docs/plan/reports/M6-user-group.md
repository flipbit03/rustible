# M6 report: `user::{Present, Absent, Existing, Membership}` and `group::{Present, Absent}`

**Branch:** `m6-user-group`. **Run:** unattended run 1, 2026-09-08 (subagent).
**Brief:** the `user` and `group` rows of `docs/plan/M6.md` plus the wave-one
template in `docs/06_BUILD_PLAN.md` section 4 and the lead's op brief.
**Done-when log:** `docs/plan/logs/M6-user-group-done.txt`.

## What was built

1. **`crates/rustible-std/src/group.rs`** (`pub mod group;` in the crate root).
   `Group { name, gid, members }` is the output of `Present` and the input of
   `user::Membership::in_group`. Pure parsers: `parse_group` (skips lines with
   fewer than four fields, a non-numeric gid, or an empty name; no trailing
   newline needed), `group_entry`, `group_by_gid`, `groups_of(text, user)` (the
   sorted supplementary group list), and `lookup_group`, which turns a line
   that names the group but does not parse into an error instead of "missing".
   Shared helpers used by both modules: `Tools::of(sys)` (shadow-utils
   everywhere, BusyBox on `Distro::Alpine`), `require_root`, `run_tool` (adds
   "running `groupadd` (shadow-utils)" context so a missing binary on an
   `Other` distro reads as such), `validate_name`, `validate_field`.
2. **`group::Present::new("docker").gid(u32).system(bool)`**: `groupadd [-r]
   [-g GID] name` / `addgroup [-S] [-g GID] name`; an existing group with a
   different gid gets `groupmod -g` (Alpine fails clearly). A gid already used
   by another group fails at `check`. `Diff::Attrs` with `exists` and `gid`.
   Output `Group`, re-read from `/etc/group` after the command.
3. **`group::Absent::new("docker")`**: `groupdel` / `delgroup`; fails at
   `check` naming the user when the group is someone's primary group. Output
   `Removed { name, gid: Option<u32> }`.
4. **`crates/rustible-std/src/user.rs`** (`pub mod user;`).
   `Account { name, uid, gid, home, shell, groups }` (groups = sorted
   supplementary groups from `/etc/group`). Pure: `PasswdEntry`,
   `parse_passwd`, `passwd_entry`, `passwd_by_uid`, `lookup_user`,
   `account_of(entry, group_text)`; `Desired` + `plan_modify(current,
   current_groups, want) -> Delta` computes the attribute diff of an existing
   account (uid, gid, home, shell, comment, groups with `append`), and
   `Delta::from_changes` reads a `Diff::Attrs` back so `apply` executes what
   `check` produced. `GroupId` (`From<u32>`, `From<&str>`, `From<String>`,
   `From<&Group>`) is what `.gid(..)` takes.
5. **`user::Present::new("cadu")`** with `.uid`, `.gid`, `.home`, `.shell`,
   `.create_home` (default true), `.system`, `.groups([..])`, `.append`
   (default true), `.comment`. `check` requires root, validates names and
   fields, reads both files, refuses missing groups and a missing primary
   group (vision 6.7) and a uid taken by another user, then plans a create
   (`useradd [-r] [-u] [-g] [-G a,b] [-d] [-s] [-c] -m|-M name`, or `adduser
   -D [-S] [-u] [-G group] [-h] [-s] [-g gecos] [-H] name` plus one `addgroup
   user g` per group on Alpine) or a modify (one `usermod` with `-u -g -d -s
   -c` and `-aG added` or `-G exact`; `addgroup`/`delgroup` on Alpine, where
   attribute changes fail clearly). `apply` re-reads the files and returns the
   real `Account`. Default shell for the diff and prediction comes from
   `SHELL=` in `/etc/default/useradd` when present, else `/bin/sh`.
6. **`user::Absent::new("cadu").remove_home(bool)`**: `userdel [-r]` /
   `deluser [--remove-home]`. Output `Removed { name, home: Option<PathBuf> }`
   with `home` set to the directory this step deleted.
7. **`user::Existing::named("cadu")`**: read-only, no root needed,
   `Satisfied(Account)` or an error naming the user; `apply` is unreachable and
   says so.
8. **`user::Membership::of(&account).in_group(&group)`** (also `of_name`,
   `in_group_named`): user and group must exist; satisfied when the group
   lists the user or is the primary group; `usermod -aG group user` /
   `addgroup user group`. Output `Member { user, group }`.
9. Rustdoc on every public item; each op names `ansible.builtin.user` /
   `ansible.builtin.group` and the `state:` it translates. No new
   dependencies.

Usage:

```rust
let account = ctx.step("Ensure rustible user exists",
    user::Present::new("rustible").shell("/bin/bash").groups(["docker"]).create_home(true))?;
ctx.step("Ensure ~/.ssh exists", file::Directory::at(account.home.join(".ssh")).mode(0o700))?;
for name in ["docker", "adm"] {
    let grp = ctx.step(format!("Ensure group {name} exists"), group::Present::new(name))?;
    ctx.step(format!("Add rustible to {name}"), user::Membership::of(&account).in_group(&grp))?;
}
ctx.step("Remove old deploy user", user::Absent::new("deploy").remove_home(true))?;
```

## Verified

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace`: clean, after rebasing on `origin/main`
  at 406768b (authorized_keys merged; `lib.rs` module list resolved).
- `cargo test -p rustible-std user` (39) and `cargo test -p rustible-std group`
  (30) in the log; 55 tests live in `user.rs` (35) and `group.rs` (20), the
  rest of the matches are other modules' names.
- Pure tests: passwd and group parsing with malformed lines and no trailing
  newline; lookups by name, gid, uid; `groups_of` sorted; malformed line for
  the requested name is an error; `plan_modify` for nothing asked, shell plus
  appended groups, exact groups (add and remove, and clearing all), ids, home
  and comment; `Delta` round trip through its changes and rejection of a
  foreign diff.
- Fake-backend tests per op. `user::Present`: satisfied; create with exact
  `useradd` argv, prediction equal to the re-read account, changed then ok;
  create without uid not predicted; private-gid rule (taken gid, existing
  group of the same name, explicit `.gid` restoring prediction); `.gid` by
  name, id and `&Group`, `-r`, `-M` and `-d`; modify with `usermod -s -aG`;
  exact groups with `-G`; ids, home and comment in one `usermod`; missing
  group, missing primary group by name and by gid, taken uid; Alpine create
  (`adduser -D ... -H` plus `addgroup` per group) and Alpine system user
  without shell not predicted; Alpine membership via `addgroup`/`delgroup`
  and attribute change refused; not root; invalid names and fields;
  malformed passwd line refused by `Present`, `Absent`, `Existing`; missing
  binary names the tool family; default shell from `/etc/default/useradd`.
  `user::Absent`: satisfied; `userdel -r` with home reported and plain
  `userdel`; Alpine `deluser --remove-home`; not root. `user::Existing`: lookup
  without root, missing user error, through `ctx.step` never changed.
  `user::Membership`: satisfied for a member and for the primary group;
  `usermod -aG`; Alpine `addgroup`; missing user, missing group, not root;
  through `ctx.step` changed then ok. `group::Present`: satisfied; `groupadd
  -r -g` with prediction and re-read, changed then ok; no gid not predicted;
  `groupmod -g`; taken gid; Alpine `addgroup -S -g`; Alpine gid change
  refused; not root; missing binary; bad names. `group::Absent`: satisfied;
  `groupdel`; Alpine `delgroup`; primary group refused; not root. Check mode
  through `ctx.step` for both modules: predictions available where promised,
  no command run, files unchanged (mutation guard covered by driving `check`
  under `ctx.step`).
- Container test: not added. Branch `m6-harness` has not merged into `main`
  at the time of this PR. `TODO(M6 harness)` comments in `user.rs` and
  `group.rs` name the tests to add (debian:12 and ubuntu:24.04: create user
  changed then ok with home present; create group; membership; absent with
  home removed). Not faked.

## Deviations

- The Fake cannot run `useradd`, so tests that assert the re-read output
  plant the tool's effect on `/etc/passwd` and `/etc/group` (via
  `Backend::write` on the fake) between `check` and `apply`. This is why
  `apply` executes the diff rather than inspecting again (Decisions).
- `Account` carries `groups` as the sorted list, not file order, so a
  prediction and a re-read compare equal.
- `Desired::default()` has `append: true` (manual `Default`) so the default
  asks for nothing.
- `ssh::authorized_keys::{Present, Absent}::for_user(&Account)` added on the
  lead's instruction after PR #3 merged: one-line wrappers over
  `for_account(&account.home, account.uid, account.gid)`, with a Fake test
  chaining `user::Existing` into `Present::for_user` and `Absent::for_user`
  through `ctx.step`.

## Decisions

See the `[M6-ug]` entries in `docs/plan/DECISIONS.md`. In short:

- Predict only when honest: existing account or group always; new one only
  with a known uid (gid), and for a user a known primary gid (explicit or the
  private-group rule when gid == uid is free); otherwise no prediction.
- `apply` executes the `Diff::Attrs` from `check` and re-reads the files for
  the real output; no second inspection.
- `append` defaults to `true` (keeping memberships is the safe default).
- Alpine: attribute changes on existing users and gid changes on existing
  groups fail at `check` (no `usermod`/`groupmod` in BusyBox); membership
  works.
- `.gid()` accepts a number, a name, or `&Group`; the group must exist.
- `group::Absent` refuses a primary group at `check`.
- `Membership` counts the primary group as membership and never removes.
- `/etc/passwd` and `/etc/group` are read directly (no NSS); malformed lines
  for the requested name are errors.
- `for_user(&Account)` on authorized_keys added at the lead's request.

## Self-review

Run by the lead on the PR.
