# M6: `shell::Command` extras, container tests for the merged ops

Branch `m6-shell-tests`. Governing sections: vision 6 (ops), 6.6/6.7 (the
worked example and "refuse rather than guess"), 8 (testing tiers), 12
(prediction). Brief: `docs/plan/M6.md`, the `shell` row plus the harness row.

Three jobs, in the order they turned out to matter: finish `shell::Command`,
put the merged ops that had only `Fake` tests in front of a real machine, and
resolve the two cross-op findings that the earlier PR reviews left open.

## Built

**`shell::Command`.** `sh(script)` (`/bin/sh -c`, the `ansible.builtin.shell`
mapping, next to `new(program)` for `command`), `env(k, v)`, `stdin(bytes)`,
`creates(path)`, `removes(path)`, `changed_when(closure)`, alongside the
`arg`/`args`/`cwd` that were already there. `always_changes()` is now honest:
it returns false as soon as any of `creates`, `removes` or `changed_when` is
set, so a step with an escape hatch is no longer forced through the
always-changed path.

`stdin` and `env` needed real support underneath, not just a builder field.
`Cmd`/`CmdSpec` carry them, `Backend::spawn` honours them, and the `Fake`
records them so a unit test can assert on both. `Local::spawn` writes stdin
from its own thread while the reader threads drain stdout and stderr: a child
that writes more than a pipe buffer before reading its input used to deadlock.
The unit test feeds `cat` four megabytes; the container test feeds `wc -c` two
and checks the byte count comes back.

`changed_when` needed something the `Op` trait did not have. An action can
only tell after running whether anything happened, and `check` must not run
the command, because a dry run that executes the command is not a dry run. So
`Op` gained `changed_by_apply(&self, &Output) -> bool`, default `true`, asked
by `Ctx::step` after a successful `apply`. When it answers false the step
finishes `Ok` with its diff intact and the note "ran, unchanged". Check mode
never consults it: a step with a `changed_when` reports `would change`,
because the honest answer before running is that it might.

**Container tests** for the ops the earlier waves merged with only `Fake`
coverage: `it_file_ops` (the whole file family, thirty-one steps),
`it_user_group`, `it_authorized_keys`, `it_shell_command`, and later
`it_user_busybox`. Each `TODO(M6 harness)` comment those satisfy is gone;
`grep -rn "TODO(M6 harness)"` over `crates/` now finds nothing.

## The two cross-op findings

**Check-mode chaining from a group a previous step only planned to create.**
The vision 6.6 shape is `group::Present::new("web")` then
`user::Present::new("app").groups(["web"])`. Under `--check` the group step
creates nothing, so the user step looked the group up, did not find it, and
aborted the dry run at exactly the point where the real run succeeds. A dry
run that fails where the real run works is worse than no dry run.

Resolved with a planned-resource note on `System`, shared by every clone
within one run: `note_would_create(kind, name, id)`, and `would_create(kind,
name)` / `would_create_id(kind, id)` / `would_create_id_by_name(kind, name)`
to ask. Outside check mode the questions answer `false`/`None`, so the
mechanism is inert in a real run. `group::Present::check` notes what it would
create; `user::Present` (for `groups` and for `gid`, by name or by gid) and
`user::Membership` accept a planned group and report `would change`.

This does not weaken "refuse rather than guess" (vision 6.7). Nothing is
created. A real run still refuses a group that is not on the machine, and the
container test asserts exactly that: the same step that succeeds in the dry
`Ctx` fails against the real one, with `/etc/group` unchanged afterwards. The
dry run trusts only what an earlier step in the same run said it would do,
which is the same thing the real run trusts that step for having done. A
planned group whose gid is unknown shows in the user's diff as `group=<name>`
and blocks prediction, per vision 12; with `group::Present::gid(4343)` the gid
is known, so `.gid(&planned)` predicts and the test asserts the predicted gid.

**The BusyBox default-shell prediction.** `user::Present` predicted `/bin/sh`
for a new account on BusyBox. That is a guess. BusyBox `adduser` takes the
shell from `$SHELL`, failing that from the invoking user's own passwd entry,
and the op can see neither through `sys` (under `sudo` or the M5 elevated
helper they differ from the playbook's anyway).

Resolved by not predicting it, and not naming it in the diff either, unless
the playbook passed `.shell()` explicitly. The previous rule only withheld the
prediction for `system(true)` accounts, which was the same guess with a
narrower blast radius.

`it_user_busybox` on `alpine:3.20` proves it rather than asserting it: the
same `adduser -D`, on one image in one run, produces `/bin/sh` with no
`$SHELL` in its environment and `/bin/ash` under `SHELL=/bin/ash`. Whichever
one the op had hard-coded would have been wrong half the time. The rejected
alternative was to pin `SHELL=/bin/sh` in the `adduser` environment to make
the old prediction true, which would have silently given accounts a different
shell from a hand-run `adduser` on the same box.

## What the container tests caught that the `Fake` tests did not

**A real bug, and the reason this branch has a second commit.** The vision 6.1
shape is a group step followed by a user step. Written with the *same name*
for both, which is the natural thing to write, it failed on every real image:

```
groupadd rustible-x && useradd rustible-x
  -> useradd: group rustible-x exists - if you want to add this user
     to that group, use -g
addgroup rustible-x && adduser -D rustible-x
  -> adduser: group name 'rustible-x' is in use
```

Both tools try to create a private group named after the account and refuse
when one already exists. Every `Fake` test passed, because the `Fake` returns
whatever exit status the test told it to and nobody thought to tell it about a
failure they did not know existed. It took `useradd` itself to find this.

`user::Present` without an explicit `gid` now passes a group named after the
new account as its primary group: `useradd -g <gid>`, BusyBox `adduser -G
<name>`. That also makes the gid known ahead of time, so the step predicts
where it previously could not. In check mode a *planned* same-named group is
accepted the same way, through the mechanism above. An explicit `gid` still
wins. Ansible passes `-N` here instead, which suppresses the private group and
lands the account in the default group (`users`, gid 100) that nobody asked
for.

**A flaky gate that would have been a red CI on somebody else's PR.** Running
the whole container suite for this branch, `it_systemd_image` failed once on
`jrei/systemd-ubuntu:24.04` with "systemd did not reach `running` in 60s (last
state: `offline`)", then passed three times in a row. Protocol section 5 says
one failure is a bug, not a flake, so it got read rather than re-run:
`wait_for_systemd` treated `offline` as a terminal state, but `systemctl
is-system-running` answers `offline` whenever it cannot reach the manager,
which includes the first fraction of a second of the container's life. The
timeout, not the state, is what should decide. `offline` is now polled through;
`stopping` and `maintenance` stay terminal, and a genuinely dead container is
still caught immediately by the `container_running` probe. The only cost is
that pointing a `SystemdImage` test at a non-systemd image now takes the full
sixty seconds to be diagnosed instead of failing fast.

This is main's code (the harness), not this branch's. It is fixed here because
this branch is what surfaced it and leaving it would make the harness job red
at random on unrelated pull requests.

**Smaller things.** `useradd`'s private group goes away with the account
(`USERGROUPS_ENAB`), so `group::Absent` for a same-named group afterwards is
correctly a no-op, and the test asserts that rather than assuming it. BusyBox
has no `usermod` at all, so modifying an existing account there fails with a
message that names the reason; the Alpine test pins that error instead of
trusting the `Fake`'s idea of it.

## Deviations from the brief

The brief's container test says `Present::new("rustible-test")` for both the
group and the user. That is literally the case that cannot work, per the
finding above, so `it_user_group` uses `rustible-grp` and `rustible-usr` and
then tests the same-named case deliberately, with the assertions the fixed
behaviour deserves.

The Alpine coverage is a separate binary, `it_user_busybox`, not an
`alpine:3.20` leg on `it_user_group`. BusyBox is a different toolset rather
than a variation on the same one, and the Debian test's assertions about
`/bin/bash`, `USERGROUPS_ENAB` and `usermod` have no Alpine equivalent to
branch to. Two files read better than one file of `if busybox`.

## Verified

All four gates and the whole container suite, twice: on the merge of `main`
that brought the M3 CLI and the M7 CI workflows, and again on the second merge
that brought the `rustible-github` collection. That second merge conflicted in
`DECISIONS.md`, where both sides had appended a block; both blocks are kept.
Output in `docs/plan/logs/M6-shell-tests-done.txt`.

| gate | result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace` | all pass, 281 of them in `rustible-std` alone |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --lib` | clean |

Container suite as CI runs it, `RUSTIBLE_INTEGRATION=1 cargo test -p
rustible-std --tests`: eleven test binaries, five images, 38s wall.

| test | images | time |
|---|---|---|
| `it_shell_command` | debian:12, ubuntu:24.04 | 0.65s |
| `it_file_ops` | debian:12, ubuntu:24.04 | 0.72s |
| `it_user_group` | debian:12, ubuntu:24.04 | 1.35s |
| `it_user_busybox` | alpine:3.20 | 1.19s |
| `it_authorized_keys` | debian:12, ubuntu:24.04 | 0.77s |
| `it_file_line` | debian:12, ubuntu:24.04 | 0.66s |
| `it_apt_present` | debian:12, ubuntu:24.04 | 11.56s |
| `it_apt_absent` | debian:12, ubuntu:24.04 | 14.78s |
| `it_sysctl_present` | debian:12, ubuntu:24.04 | 0.66s |
| `it_systemd_image` | jrei/systemd-{debian:12,ubuntu:24.04} | 1.76s |
| `it_systemd` | jrei/systemd-{debian:12,ubuntu:24.04} | 2.31s |

## Not verified

- **ARM.** Everything above ran on the x86 VM only. The images are
  multi-arch, so the tests should run on `cadu-cogram-vm-arm` unchanged, but
  nobody has run them there.
- **`shell::Command` over SSH.** The stdin-from-a-thread fix is in
  `Local::spawn`. The remote transport has its own path, and no container test
  reaches it; the M3 remote run does not feed stdin to anything.
- **The `offline` fix under a genuinely broken image.** The new behaviour on a
  non-systemd image (a full sixty-second timeout, then an error naming the
  last state) is reasoned about, not exercised. Writing a test for it means
  paying sixty seconds per run, which is not worth it.

## Decisions

Eleven `[M6-sh]` entries in `docs/plan/DECISIONS.md`, each with a Reverse clause:
`changed_by_apply`, the `changed_when` closure representation, `sh()` and
`always_changes()`, `stdin`/`env` support, the planned-resource mechanism, the
BusyBox shell prediction, the distinct test names, the same-named-group fix,
`would_create_id_by_name`, the `wait_for_systemd` `offline` fix, and the
separate Alpine binary.
