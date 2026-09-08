# M6 report: Docker integration harness

**Branch:** `m6-harness`. **Run:** unattended run 1, 2026-09-08 (interrupted by a
rate limit and resumed once). **Brief:** `docs/plan/M6.md`, "Docker harness".
**Done-when log:** `docs/plan/logs/M6-harness-done.txt`.

## What was built, per brief item

1. **`rustible_sdk::testing`** (`crates/rustible-sdk/src/testing.rs`, tier 3
   of vision 8). `Spec` (what the macro hands over), `Image` (`Plain` /
   `Systemd`), `run` (the generated test's entry point), `run_images` (drive
   docker for a list of images, no skip, no panic), `changed_then_ok` (apply
   an op twice, assert `changed` then `ok`, return both results), and the
   `Report` / `StepReport` / `ImageResult` types the two sides of the harness
   exchange. Six unit tests cover the pure parts (libtest path derivation,
   cargo artifact selection, report parsing, pass/fail verdict).
2. **`#[rustible::integration_test(images = [...], systemd_images = [...])]`**
   (`crates/rustible-macros/src/lib.rs`, re-exported from `rustible`). Expands
   the annotated `fn name(ctx: &mut Ctx) -> Result<()>` to a plain `#[test]`
   that calls `testing::run` with a `Spec` filled from `stringify!`,
   `module_path!`, `env!("CARGO_CRATE_NAME")` and `env!("CARGO_MANIFEST_DIR")`.
   Rejects a missing/empty image list and a wrong signature at compile time;
   four trybuild cases (two pass, two fail) in `crates/rustible-macros/tests/ui/`.
3. **`SystemdImage` variant.** `Image::Systemd`, written as
   `systemd_images = ["jrei/systemd-debian:12", "jrei/systemd-ubuntu:24.04"]`.
   Those are the documented images (module docs and the enum's rustdoc): the
   `jrei/systemd-*` family is Debian/Ubuntu with systemd installed and
   `/sbin/init` as the entrypoint. The harness boots the container detached
   with `--privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw
   --tmpfs /run --tmpfs /run/lock`, polls `systemctl is-system-running` until
   `running` or `degraded` (60 s cap), `docker exec`s the test binary, and
   `docker rm -f`s the container whatever happened. Every container carries
   the label `rustible.integration=1` so a run killed half way is findable.
4. **Three harness tests in `crates/rustible-std/tests/`**, one per shape:
   `it_file_line.rs` (`file::Line`: add, then replace with `matching` and
   `backup`), `it_apt_present.rs` (`apt::Present` with `update_cache`, checks
   the resolved version and the installed binary), `it_systemd_image.rs`
   (systemd is PID 1, `systemctl is-active systemd-journald` through
   `shell::Command`). The first two run on `debian:12` and `ubuntu:24.04`, the
   third on both systemd images.

## Design chosen and why

The test binary runs itself inside the container. `run` looks at
`RUSTIBLE_INTEGRATION_IMAGE`: absent, it is the host side, and it
cross-compiles the very test target it is executing in
(`cargo test --no-run --release --target <arch>-unknown-linux-musl --test
<file> --message-format=json`, once per process, artifact path taken from the
JSON), then for each image runs `docker run --rm -v <bin>:/t:ro <image> /t
--exact <path> --nocapture`. Present, it is the container side: it builds a
`Ctx` over `System::local` with real facts, runs the body under
`catch_unwind`, tees events to the `Pretty` renderer (so `--nocapture` reads
like a playbook run, prefixed with the image name) and to a `Collect` sink,
and prints one `RUSTIBLE_INTEGRATION_REPORT {json}` line with the steps,
statuses, short diffs, command count, distro seen, and the error chain or
panic message. The host parses that line; a missing line, a non-zero docker
exit, or a report with an error fails the outer test, and all image failures
are reported together at the end.

Why this shape: it is the vision 8.3 sentence made literal (static musl
binary dropped into any stock image, no setup), there is no second entry
point (`cargo test` is the only command), and an op author's test body is
ordinary op code against a real `Local` backend. The alternative, a
`build.rs` or an outer wrapper that builds first and mounts a directory of
binaries, would need a second command and a registry of test names.

Gating: `RUSTIBLE_INTEGRATION=1` and a working `docker info` are both
required; otherwise the test prints the reason and passes. The brief's
"runs in CI when docker is available, skipped otherwise" is met by CI
setting the variable on runners that have docker (no CI workflow exists in
the repo yet; that is M7's). `RUSTIBLE_INTEGRATION_IMAGES=a,b` narrows a run
to a subset of the attribute's images. Fallbacks the brief allowed and were
not needed: none; both stock images and both systemd images work on this
host without any of the "VM for systemd" path from vision 8.4.

## How an op author adds a test

Three lines, plus `use rustible::prelude::*; use
rustible::sdk::testing::changed_then_ok;` at the top of a new
`crates/rustible-std/tests/it_<module>_<state>.rs` (underscores: the file
name is the cargo test target and the crate name the harness rebuilds):

```rust
#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn present_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
    changed_then_ok(ctx, "install sl", || apt::Present::new(["sl"]))?; Ok(())
}
```

`changed_then_ok` returns both `Applied` results for further assertions, and
`ctx.sys()` is the real system for looking at the outcome. Systemd ops use
`systemd_images = ["jrei/systemd-debian:12", "jrei/systemd-ubuntu:24.04"]`
instead of (or next to) `images`. Run one file with
`RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_apt_present --
--nocapture`.

## Timings per image

From the done-when log (wall time of the container run; the musl build of
each test target is 0.3 s warm, 1.8 s after a change to `rustible-std`):

| test | image | time |
|---|---|---|
| `it_file_line` | `debian:12` | 0.9 s |
| `it_file_line` | `ubuntu:24.04` | 0.3 s |
| `it_apt_present` | `debian:12` | 2.3 s (`apt-get update` 1.1 s, install 0.7 s) |
| `it_apt_present` | `ubuntu:24.04` | 9.3 s (`apt-get update` 3.5 s, install 3.4 s) |
| `it_systemd_image` | `jrei/systemd-debian:12` | 0.6 s (boot to `running` included) |
| `it_systemd_image` | `jrei/systemd-ubuntu:24.04` | 0.6 s |

Images were already pulled; a first pull adds a few seconds per image.

## Verified

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace` (no docker leg: the three harness tests
  skip and pass), all exit 0 after merging `main` (PR #2, M4).
- `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test <file> --
  --nocapture` for each of the three files: 6 of 6 image runs pass, output in
  the log. Repeated three times for `it_systemd_image` and `it_file_line`
  (docs/07 rule 5): 6 of 6 green, no flake.
- `docker ps -aq --filter label=rustible.integration` is empty after the
  runs: no leaked containers.
- Hard limits: no sudo, no host but this VM, docker as this user;
  `--privileged` is only used for the systemd images and only inside docker.
  Test paths are under `/etc/rustible-test` inside throwaway containers, so
  nothing to clean on the host.

## Deviations

- **Opt-in variable** instead of auto-running whenever docker exists (see
  Decisions). A developer running `cargo test --workspace` on a laptop with
  docker should not be surprised by image pulls and a musl build.
- **Systemd images are third-party** (`jrei/systemd-*`) rather than the
  brief's stock `debian:12` / `ubuntu:24.04`, which ship no systemd. Recorded
  in Decisions with the reversal (own Dockerfile).
- **The systemd smoke test exercises the harness, not a systemd op**: the
  systemd ops live on their own M6 branch. That branch should switch its
  harness test to `systemd_images` and may delete `it_systemd_image.rs` once
  a real op covers the same ground.
- **No CI workflow file** was added: none exists on `main`, and M7 owns the
  workflow. The harness's skip logic is what makes the brief's "skipped
  otherwise" true today.

## Decisions

Added to `docs/plan/DECISIONS.md` under docs/07 rule 2.3, each with its
reversal:

- Opt-in via `RUSTIBLE_INTEGRATION=1` plus `docker info`, skip otherwise.
- The running test rebuilds its own target for musl (no wrapper); test file
  names must be crate names.
- `rustible-std` takes `rustible` as a dev-dependency (dev-only cycle, same
  as `rustible-macros`) so its tests write the attribute as an author would.
- `SystemdImage` is `Image::Systemd`, spelled `systemd_images = [...]`;
  `--privileged` with the host cgroup; images `jrei/systemd-debian:12` and
  `jrei/systemd-ubuntu:24.04`.

## Self-review

Self-review: run by the lead on PR #5, see below.

## Self-review (lead, PR #5)

`code-review` at effort high: ten consolidated findings, five confirmed.
Applied on the branch:

1. The in-container branch was selected by `RUSTIBLE_INTEGRATION_IMAGE` alone,
   so a stray host export ran the test body on the host. The harness now
   sets `RUSTIBLE_INTEGRATION_INSIDE` and the branch requires it plus the
   mounted binary at `/t`; a bare image variable on the host is ignored with
   a note.
2. `rustible-std`'s dev-dependency on `rustible` carried a version, which
   would make a publish-time cycle. Path-only now, like `rustible-macros`.
3. The author's attributes (`#[ignore]`, `#[should_panic]`, `#[cfg]`, docs)
   landed on the inner fn. They move to the generated outer `#[test]`.
4. `wait_for_systemd` ignored `docker exec`'s exit status and spun 60 s on a
   dead container. A non-zero exit with empty stdout fails at once with the
   stderr.
5. A panic on the host side leaked a privileged systemd container. A `Drop`
   guard removes it; the body runs under coreutils `timeout 600` inside the
   container (plain and systemd runs) so a hung body cannot block CI.
6. With `RUSTIBLE_INTEGRATION=1` explicitly set, a missing docker or an image
   filter selecting nothing turned tests into passing skips. Both now fail;
   the silent skip remains only for the unset-variable case.
7. `--test <CARGO_CRATE_NAME>` only matches top-level, underscore-named test
   files; cargo's "no test target named" is now translated into a message
   saying so, and the dead hyphen-normalizing lookup is gone.

Dismissed, with reasons:
- Replacing the harness's `Report`/`StepReport` with a replay of `event::Event`
  through `JsonLines`: a real simplification, but a refactor of working code
  with no behaviour change; deferred, recorded in DECISIONS.md for M7.
- Dropping `--cgroupns=host` and the host cgroup bind mount: the jrei images
  declare `VOLUME /sys/fs/cgroup` and a finder reproduced that they fail to
  boot under the default private cgroup namespace; the flags are what makes
  them work. A purpose-built systemd image without the volume would allow the
  safer form; recorded in DECISIONS.md.
- Module path `rustible_sdk::testing` instead of the brief's
  `testing::integration`: kept, recorded as a deviation here.

Verified after the fixes: `cargo test --workspace` green without docker;
`RUSTIBLE_INTEGRATION=1` runs of `it_file_line` (debian 0.7 s, ubuntu 0.3 s)
and `it_systemd_image` pass with the new marker, timeout, and guard.
