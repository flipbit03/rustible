# M6 report: `ssh::authorized_keys::{Present, Absent}`

**Branch:** `m6-ssh-authorized-keys`. **Run:** unattended run 1, 2026-09-08
(resumed once after a rate-limit interruption). **Brief:** the
`ssh::authorized_keys` row of `docs/plan/M6.md` plus the wave-one template in
`docs/06_BUILD_PLAN.md` section 4.
**Done-when log:** `docs/plan/logs/M6-authorized-keys-done.txt`.

## What was built

1. **`crates/rustible-std/src/ssh/mod.rs`**: the `ssh` module, holding
   `authorized_keys` only for now; `pub mod ssh;` added to the std crate root.
2. **`PublicKey { options, key_type, key, comment }`** parsed by the pure
   `parse_line`, which handles the optional leading options field the way
   sshd does (the first token is options when it is not a key type; quoted
   whitespace and backslash escapes inside quotes do not end the token), the
   `sk-` and `-cert-v01@openssh.com` key types, and returns `None` for blank
   lines, `#` comments, and anything that is not a key. `to_line` renders the
   canonical form; `same_key` is the identity: `(key_type, key)` only.
3. **Pure planning**: `plan_present(text, keys, exclusive)` and
   `plan_absent(text, keys)` return a `Planned` (new text when something
   changes, plus `added`, `removed`, `already_present`, `not_present`).
   Untouched lines are written back byte for byte, key lines included, so an
   existing line carrying a requested key with a different comment or
   options field is counted present and left alone. New keys are appended in
   canonical form. Requested keys are de-duplicated by identity. A file that
   lacked a trailing newline gets one when rewritten (the `file::Line`
   normalization) and is not rewritten just for that.
4. **`Present::for_user_name(name)` / `Present::in_file(path)`** return a
   `PresentBuilder`; `.exclusive(bool)` is available on the builder and on the
   finished op (vision 6.1 writes `.keys(keys).exclusive(true)`, the brief
   writes `.exclusive(true).keys(..)`; both read well); `.keys(iter)` is the
   finishing call. The user form reads `/etc/passwd` through
   `sys.read_to_string` for uid, gid, and home, and fails naming the user when
   absent. `check` refuses a target that is a directory, a missing parent in
   the file form, and a missing home directory in the user form, all with
   messages naming the op that creates the prerequisite (vision 6.7).
   `check` reads the file (missing counts as empty), plans, and returns
   `Plan::change_predicting(Diff::text(..), KeysReport)`; `apply` takes the
   `after` text from the diff and the report from `change.predicted`, writes
   with `sys.write_atomic`, and when creating: `~/.ssh` with `mkdir_all`,
   `set_mode(0o700)`, `set_owner(uid, gid)`, and the file with
   `set_mode(0o600)` and `set_owner`. Existing files and directories keep
   their attributes.
5. **`Absent::for_user_name(name)` / `Absent::in_file(path)`** with `.keys`
   finishing. A missing file is `Satisfied` with every key in `not_present`;
   nothing is ever created and no attributes change.
6. **`KeysReport { path, added, removed, already_present, not_present }`**,
   the output of both ops. `Present` fills `added`, `already_present`, and
   `removed` (only with `exclusive`); `Absent` fills `removed` and
   `not_present`.
7. Rustdoc on every public item; the module, `Present`, and `Absent` name
   `ansible.posix.authorized_key` and the `state:` each translates.

Usage:

```rust
let keys = ["ssh-ed25519 AAAAC3...XYZ cadu@x86", "ssh-ed25519 AAAAC3...ABC cadu@arm"];
let r = ctx.step("Install authorized keys", authorized_keys::Present::for_user_name("cadu").exclusive(true).keys(keys))?;
ctx.log(format!("+{} -{} keys in {}", r.added.len(), r.removed.len(), r.path.display()));
```

## Verified

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace`: clean.
- `cargo test -p rustible-std ssh`: 33 tests, output in
  `docs/plan/logs/M6-authorized-keys-done.txt`. Pure tests: plain key,
  no comment and extra spaces, options field with quoted spaces and escapes,
  cert and `sk-` types, non-keys, identity ignoring comment and options;
  `plan_present` on an empty file, satisfied, matching by identity with a
  different comment (line kept as is), missing trailing newline (rewritten
  only when something else changes), comments and blanks and options and
  unparseable lines preserved in place, duplicate keys in the request,
  exclusive removing strangers and keeping comments, exclusive satisfied when
  exact, non-exclusive leaving strangers; `plan_absent` removing matching
  lines only (an options-prefixed line included), a key that is not there, an
  empty file, every duplicate line of a key; `/etc/passwd` lookup.
- Fake-backend tests: user form resolving `/etc/passwd`, creating `.ssh`
  (0700, uid 1000, gid 1001) and the file (0600, same owner) asserted via
  `fake.file(..)`, check creating nothing, second check `Satisfied` with the
  keys in `already_present`; an existing 0644 file and 0755 `.ssh` keep their
  attributes; exclusive through `ctx.step` changed then ok; check mode
  predicting and writing nothing; the create path through `ctx.step` under
  the mutation guard; `in_file` form writing the given path with no owner
  change and no commands; `in_file` refusing a missing parent; user form
  refusing a missing home; unknown user error from both ops; invalid key
  line error; `Absent` removing one of three and satisfied afterwards;
  `Absent` on a missing file satisfied and creating nothing.
- Container test: not added. Branch `m6-harness` has not merged into `main`
  at the time of this PR, so there is no `#[rustible::integration_test]` to
  use. A `TODO(M6 harness)` comment in `authorized_keys.rs` names the test to
  add (user form twice on `debian:12` and `ubuntu:24.04`, asserting modes and
  ownership on the real filesystem). Not faked.

## Deviations

- `plan_present` and `plan_absent` return a `Planned` struct rather than the
  brief's `Option<(String, KeysReport-ish)>`: the satisfied case still needs
  `already_present` / `not_present` for the `Satisfied` output, so the text
  is the only optional part. `Planned::into_report(path)` turns it into the
  `KeysReport`.
- No container test (see Verified).

## Decisions

- [M6] 2026-09-08 The user form refuses when the user's home directory does
  not exist instead of `mkdir_all`-ing it root-owned on the way to `~/.ssh`:
  vision 6.7 (one resource per op; prerequisites fail with a clear message);
  creating `~/.ssh` itself is kept because the brief and Ansible's
  `manage_dir` default both ask for it. Reverse: drop the home check in
  `check_parent`.
- [M6] 2026-09-08 `exclusive(bool)` exists on both `PresentBuilder` and
  `Present`: vision 6.1 shows `.keys(keys).exclusive(true)` and the brief
  shows `.exclusive(true).keys(..)`. Reverse: delete one of the two methods.
- [M6] 2026-09-08 A requested line that does not parse as a key fails the
  step at `check` ("not a public key line: ...") rather than being appended
  verbatim: a typo in a playbook should not land in `authorized_keys`.
  Reverse: append unparseable lines as `Entry::Other`.
- [M6] 2026-09-08 Key identity is `(key_type, key)`; options and comment are
  ignored for matching and an existing line is never rewritten to match the
  requested options or comment. Ansible does the same for the comment and
  rewrites options; rewriting is a follow-up if my_infra needs it. Reverse:
  compare options in `same_key` or rewrite the raw line on mismatch.
- [M6] 2026-09-08 `Absent` does not create the file or `.ssh` and does not
  check the parent; a missing file is simply satisfied.

## Self-review

Run by the lead on the PR.

## Self-review (lead, PR #3)

`code-review` at effort high: seven consolidated findings, two confirmed by
test, the rest by code reading. Applied on the branch:

1. A requested key with an embedded newline wrote two file lines and broke
   changed-then-ok under `exclusive`. Requested lines containing any control
   character are now refused at `check`. Test added.
2. `check_parent` refused a symlinked `~/.ssh` because `System::stat` is
   `lstat`. The SDK gained `Backend::stat_follow`/`System::stat_follow`
   (`Local` uses `std::fs::metadata`, `Fake` aliases `stat`), and the parent
   check follows links. A symlinked `authorized_keys` file is refused with a
   message pointing at `in_file(real path)`, because an atomic rewrite would
   replace the link with a regular file.
3. The op created `~/.ssh` inside an op whose resource is the file, against
   vision 6.7 (decided) and the 6.1 example, which ensures the directory in a
   separate `file::Directory` step. Now a missing `~/.ssh` fails naming
   `file::Directory`; the create branch is gone. Tests updated.
4. The vision's `for_user(&Account)` shape was absent. Added
   `for_account(home, uid, gid)` on both ops, a no-lookup form; when the
   `user` ops merge, `for_user(&Account)` becomes a one-line wrapper (noted
   for the m6-user-group merge).
5. CRLF files lost their carriage returns on rewrite. The line terminator is
   detected and preserved. Test added.
6. A malformed `/etc/passwd` entry for the user was reported as "does not
   exist". `passwd_entry` now distinguishes malformed from missing. Test added.
7. `/etc/passwd` is read directly rather than through NSS, so LDAP/SSSD/homed
   accounts are refused. Recorded as a known limitation (the vision plans the
   user ops around `/etc/passwd`); `for_account` is the escape hatch.
