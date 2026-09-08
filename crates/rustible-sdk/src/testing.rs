//! Docker integration harness: tier 3 of the testing strategy (vision 8).
//!
//! An op author writes one integration test per op and marks it with
//! `#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]`.
//! The body receives a `&mut Ctx` bound to the real `Local` backend and real
//! facts, and runs *inside* each image as root. The usual shape is
//! [`changed_then_ok`]: apply an op twice, assert `changed` then `ok`, then
//! look at the system.
//!
//! How it works, so the failure modes make sense:
//!
//! 1. The macro expands to a normal `#[test]` that calls [`run`].
//! 2. Without `RUSTIBLE_INTEGRATION=1` in the environment, or without a
//!    working `docker`, the test prints why and passes, so a plain
//!    `cargo test --workspace` stays green everywhere.
//! 3. With both, [`run`] cross-compiles **the test binary it is running in**
//!    for `<arch>-unknown-linux-musl` (`cargo test --no-run --release --target
//!    ... --test <this file>`, once per process), then for each image does
//!    `docker run --rm -v <binary>:/t:ro -e RUSTIBLE_INTEGRATION_IMAGE=<image>
//!    <image> /t --exact <test> --nocapture`.
//! 4. Inside the container the same `#[test]` sees `RUSTIBLE_INTEGRATION_IMAGE`,
//!    builds a `Ctx` over `System::local`, runs the body, and prints one JSON
//!    [`Report`] line that the outside parses. A body that returns `Err` or
//!    panics is reported with its message and fails the outer test.
//!
//! Static musl binaries need nothing from the image, which is why any stock
//! image works. `RUSTIBLE_INTEGRATION_IMAGES=a,b` restricts a run to a subset
//! of the attribute's images. Test file names must use underscores
//! (`tests/it_file_line.rs`): the file name is both the cargo `--test` target
//! and the crate name the harness reads at compile time.
//!
//! **Systemd images** (`systemd_images = [...]`, [`Image::Systemd`]) are for
//! the systemd ops, which need a live `systemd` as PID 1. Those run as
//! `docker run -d --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup
//! --tmpfs /run --tmpfs /run/lock <image> /sbin/init`, the harness waits for
//! `systemctl is-system-running` to settle, then `docker exec`s the test binary
//! and force-removes the container. The image must ship systemd; stock
//! `debian:12` does not. The documented, tested choice is the `jrei/systemd-*`
//! family: `jrei/systemd-debian:12` and `jrei/systemd-ubuntu:24.04`. Vision 8
//! reserves VMs for what Docker does badly; this variant covers unit
//! enable/start/stop, not reboots or kernel modules. Containers carry the
//! label `rustible.integration=1`, so a run killed half way leaves something
//! `docker ps -q --filter label=rustible.integration` can find.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::ctx::{Ctx, HostInfo};
use crate::error::Result;
use crate::event::{Collect, Compact, Event, EventSink, SharedSink, Status};
use crate::op::{Applied, Op};
use crate::system::System;

/// Set to `1` (any non-empty value) to run integration tests instead of skipping them.
pub const ENABLE_VAR: &str = "RUSTIBLE_INTEGRATION";
/// Set by the harness inside the container to the image name. Never set it by hand.
pub const IMAGE_VAR: &str = "RUSTIBLE_INTEGRATION_IMAGE";
/// Set by the harness on the container command line, never by hand: the
/// in-container branch is taken only when this is set AND the mounted test
/// binary is at `/t`, so a stray host export of the image variable cannot
/// run a test body on the host.
pub const INSIDE_VAR: &str = "RUSTIBLE_INTEGRATION_INSIDE";
/// Seconds a test body may run inside a container before `timeout` kills it.
const BODY_TIMEOUT_SECS: &str = "600";
/// Optional comma-separated subset of the attribute's images to run.
pub const IMAGES_FILTER_VAR: &str = "RUSTIBLE_INTEGRATION_IMAGES";

const REPORT_PREFIX: &str = "RUSTIBLE_INTEGRATION_REPORT ";
/// Label on every container the harness starts.
const CONTAINER_LABEL: &str = "rustible.integration=1";
/// How long a systemd image may take to reach `running` or `degraded`.
const SYSTEMD_BOOT_TIMEOUT: Duration = Duration::from_secs(60);

/// An image a test runs in. Built by the `#[rustible::integration_test]`
/// expansion from `images = [...]` (`Plain`) and `systemd_images = [...]`
/// (`Systemd`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Image {
    /// A stock image; the test binary is the container's only process.
    Plain(&'static str),
    /// An image with systemd installed, booted with `/sbin/init` as PID 1
    /// before the test runs (needs `--privileged`). Known to work:
    /// `jrei/systemd-debian:12`, `jrei/systemd-ubuntu:24.04`.
    Systemd(&'static str),
}

impl Image {
    /// The docker image name, as passed to `docker run`.
    pub fn name(&self) -> &'static str {
        match self {
            Image::Plain(n) | Image::Systemd(n) => n,
        }
    }

    /// Whether this image boots systemd as PID 1 (`systemd_images`).
    pub fn is_systemd(&self) -> bool {
        matches!(self, Image::Systemd(_))
    }
}

impl std::fmt::Display for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What the `#[rustible::integration_test]` expansion hands to [`run`].
/// All fields come from `stringify!`, `module_path!`, and `env!` at the
/// definition site.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// The test function's name.
    pub name: &'static str,
    /// `module_path!()` at the definition site; with `crate_name` it gives the
    /// libtest path for `--exact`.
    pub module_path: &'static str,
    /// `env!("CARGO_CRATE_NAME")`: the test target to build and re-run.
    pub crate_name: &'static str,
    /// `env!("CARGO_MANIFEST_DIR")`: the package the test target belongs to.
    pub manifest_dir: &'static str,
    /// Images to run in, e.g. `Image::Plain("debian:12")`.
    pub images: &'static [Image],
}

/// One finished step inside the container, as reported to the outside.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StepReport {
    /// The name the test body passed to `ctx.step`, which is how an
    /// assertion in the outer test finds the step it cares about.
    pub name: String,
    /// `Ok`, `Changed`, `Failed`; `WouldChange` never appears, since the
    /// harness runs the body with check mode off.
    pub status: Status,
    /// `Diff::short`, not the full rendering: enough to see what moved, and
    /// small enough to keep the report on one line.
    pub diff: Option<String>,
    /// Wall time of the step inside the container, which includes the
    /// processes it spawned.
    pub elapsed_ms: u64,
}

/// What one container run prints as its last line, JSON-encoded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Report {
    /// [`Spec::name`], so a report can be traced back to a test function
    /// even when several ran against the same image.
    pub test: String,
    /// The image the container was started from, taken from
    /// [`IMAGE_VAR`] inside the container rather than from the outside.
    pub image: String,
    /// `Distro VERSION_ID` as the binary saw it, so a wrong image is obvious.
    pub distro: String,
    /// One entry per finished step, in the order the body ran them.
    /// Steps skipped with `ctx.skip` do not appear.
    pub steps: Vec<StepReport>,
    /// Commands the body ran, through `sys.cmd` or through ops.
    pub commands: u32,
    /// Wall time of the body alone, measured inside the container, so it
    /// excludes the image pull and the container's own startup.
    pub elapsed_ms: u64,
    /// `None` on success; the error chain or panic message otherwise.
    pub error: Option<String>,
}

/// The outcome of one image, printed by [`run`] and returned to callers of
/// [`run_images`].
#[derive(Debug, Clone)]
pub struct ImageResult {
    /// The image as named in the attribute, so a failure message says which
    /// of several images went wrong.
    pub image: String,
    /// Wall time of `docker run`, image pull included on the first use.
    pub elapsed: Duration,
    /// Parsed from the container's output when present.
    pub report: Option<Report>,
    /// Docker's exit status; 0 means the test passed inside.
    pub status: i32,
    /// Everything the container printed, for the failure message.
    pub output: String,
}

impl ImageResult {
    /// The container ran the test and its report says every step succeeded.
    pub fn passed(&self) -> bool {
        self.status == 0 && self.report.as_ref().is_some_and(|r| r.error.is_none())
    }
}

/// The body of an integration test. Generated by the macro; the author writes
/// `fn name(ctx: &mut Ctx) -> Result<()>`.
pub type Body = fn(&mut Ctx) -> Result<()>;

/// What [`changed_then_ok`] returns: the first (changed) and second (ok) results.
pub type Twice<T> = (Applied<T>, Applied<T>);

/// Entry point of the generated `#[test]`. Skips, runs inside the container,
/// or drives docker for every image and panics with every failure at the end.
pub fn run(spec: &Spec, body: Body) {
    if std::env::var_os(INSIDE_VAR).is_some() && Path::new("/t").exists() {
        let image = std::env::var(IMAGE_VAR).unwrap_or_else(|_| "?".into());
        inside(spec, &image, body);
        return;
    }
    if std::env::var_os(IMAGE_VAR).is_some() {
        eprintln!(
            "integration: {IMAGE_VAR} is set but this is not a harness container; ignoring it              (the harness sets {INSIDE_VAR} and mounts the binary at /t)"
        );
    }
    let images = selected_images(spec.images);
    match gate(&images) {
        Gate::Run => {}
        Gate::Skip(reason) => {
            println!(
                "integration test `{}` skipped: {reason} (images: {})",
                spec.name,
                spec.images
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return;
        }
        // Explicitly enabled and nothing can run: that is a failure, so a
        // CI leg with a broken docker or a mistyped image filter goes red.
        Gate::Fail(reason) => panic!("integration test `{}`: {reason}", spec.name),
    }
    let results = run_images(spec, &images);
    let mut failures = vec![];
    for r in &results {
        let steps = r
            .report
            .as_ref()
            .map(|rep| {
                rep.steps
                    .iter()
                    .map(|s| format!("{}: {}", s.name, status_word(s.status)))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let verdict = if r.passed() { "ok" } else { "FAILED" };
        // The container's step and command lines (the pretty renderer), so
        // `--nocapture` reads like a playbook run.
        for line in r
            .output
            .lines()
            .filter(|l| l.starts_with('[') || l.trim_start().starts_with('|'))
        {
            println!("  {line}");
        }
        println!(
            "[{}] {verdict} in {:.1}s  {steps}",
            r.image,
            r.elapsed.as_secs_f64()
        );
        if !r.passed() {
            let why = match &r.report {
                Some(rep) => rep.error.clone().unwrap_or_default(),
                None => format!("no report line found; docker exit status {}", r.status),
            };
            failures.push(format!(
                "[{}] {why}\n--- container output ---\n{}",
                r.image,
                r.output.trim_end()
            ));
        }
    }
    if !failures.is_empty() {
        panic!(
            "integration test `{}` failed in {} of {} image(s):\n{}",
            spec.name,
            failures.len(),
            results.len(),
            failures.join("\n\n")
        );
    }
}

/// Apply the op built by `op` twice under `name`: the first step must report
/// `changed`, the second `ok`. Returns both results so the test can inspect
/// the op's output. This is the assertion vision 8 calls typical.
pub fn changed_then_ok<O: Op>(
    ctx: &mut Ctx,
    name: &str,
    op: impl Fn() -> O,
) -> Result<Twice<O::Output>> {
    let first = ctx.step(name, op())?;
    crate::ensure!(
        first.changed,
        "step `{name}`: expected `changed` on the first apply, got `ok`"
    );
    let second = ctx.step(format!("{name} (again)"), op())?;
    crate::ensure!(
        !second.changed,
        "step `{name}`: expected `ok` on the second apply, got `changed` ({})",
        second.diff.as_ref().map(|d| d.short()).unwrap_or_default()
    );
    Ok((first, second))
}

/// Build the current test binary for musl (once per process) and run `spec`
/// in each of `images`. Does not skip and does not panic on test failure;
/// [`run`] does both.
pub fn run_images(spec: &Spec, images: &[Image]) -> Vec<ImageResult> {
    let bin = match test_binary(spec) {
        Ok(p) => p,
        Err(e) => panic!("integration test `{}`: {e}", spec.name),
    };
    let path = test_path(spec.module_path, spec.crate_name, spec.name);
    images
        .iter()
        .map(|image| match image {
            Image::Plain(name) => run_in_image(&bin, name, &path),
            Image::Systemd(name) => run_in_systemd_image(&bin, name, &path),
        })
        .collect()
}

// ---- the container side ----

/// Fan out to several sinks: the compact printer for humans reading the
/// container log, and a collector for the structured report.
struct Tee(Vec<SharedSink>);

impl EventSink for Tee {
    fn emit(&self, event: Event) {
        for s in &self.0 {
            s.emit(event.clone());
        }
    }
}

fn inside(spec: &Spec, image: &str, body: Body) {
    let collect = Arc::new(Collect::default());
    let sink: SharedSink = Arc::new(Tee(vec![
        collect.clone(),
        Arc::new(Compact::new(std::io::stdout(), 2)),
    ]));
    let sys = System::local(false, sink);
    let facts = sys.facts().clone();
    let mut ctx = Ctx::new(
        sys,
        HostInfo {
            name: image.to_string(),
            ..HostInfo::local()
        },
    );
    // libtest prints `test <name> ... ` without a newline before the body runs.
    println!();
    let t0 = Instant::now();
    let outcome = catch_unwind(AssertUnwindSafe(|| body(&mut ctx)));
    let error = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e.chain()),
        Err(payload) => Some(format!("panicked: {}", panic_message(&payload))),
    };
    let events = collect.events();
    let report = Report {
        test: spec.name.to_string(),
        image: image.to_string(),
        distro: format!("{:?} {}", facts.distro, facts.distro_version)
            .trim()
            .to_string(),
        steps: events
            .iter()
            .filter_map(|e| match e {
                Event::StepFinished {
                    name,
                    status,
                    diff,
                    elapsed_ms,
                    ..
                } => Some(StepReport {
                    name: name.clone(),
                    status: *status,
                    diff: diff.as_ref().map(|d| d.short()),
                    elapsed_ms: *elapsed_ms,
                }),
                _ => None,
            })
            .collect(),
        commands: events
            .iter()
            .filter(|e| matches!(e, Event::CmdRan { .. }))
            .count() as u32,
        elapsed_ms: t0.elapsed().as_millis() as u64,
        error: error.clone(),
    };
    println!(
        "{REPORT_PREFIX}{}",
        serde_json::to_string(&report).expect("report is serializable")
    );
    if let Some(e) = error {
        panic!("integration test `{}` failed in {image}: {e}", spec.name);
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

// ---- the host side ----

fn selected_images(all: &[Image]) -> Vec<Image> {
    match std::env::var(IMAGES_FILTER_VAR) {
        Ok(filter) if !filter.trim().is_empty() => {
            let wanted: Vec<&str> = filter.split(',').map(str::trim).collect();
            all.iter()
                .filter(|i| wanted.contains(&i.name()))
                .copied()
                .collect()
        }
        _ => all.to_vec(),
    }
}

enum Gate {
    Run,
    Skip(String),
    Fail(String),
}

fn gate(images: &[Image]) -> Gate {
    if std::env::var(ENABLE_VAR).map_or(true, |v| v.is_empty()) {
        return Gate::Skip(format!("set {ENABLE_VAR}=1 to run it in docker"));
    }
    if images.is_empty() {
        return Gate::Fail(format!(
            "{ENABLE_VAR} is set but {IMAGES_FILTER_VAR} selects none of this test's images"
        ));
    }
    if !docker_available() {
        return Gate::Fail(format!(
            "{ENABLE_VAR} is set but docker is not available (`docker info` failed)"
        ));
    }
    Gate::Run
}

fn docker_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("docker")
            .arg("info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// The musl build of the test binary this process is running from. Built once
/// per process; every integration test in the file shares it.
fn test_binary(spec: &Spec) -> std::result::Result<PathBuf, String> {
    static BUILT: OnceLock<std::result::Result<PathBuf, String>> = OnceLock::new();
    BUILT.get_or_init(|| build_test_binary(spec)).clone()
}

fn musl_triple() -> String {
    format!("{}-unknown-linux-musl", std::env::consts::ARCH)
}

fn build_test_binary(spec: &Spec) -> std::result::Result<PathBuf, String> {
    let triple = musl_triple();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let manifest = PathBuf::from(spec.manifest_dir).join("Cargo.toml");
    let t0 = Instant::now();
    eprintln!(
        "integration: building test target `{}` for {triple} (release)",
        spec.crate_name
    );
    let out = Command::new(&cargo)
        .args(["test", "--no-run", "--release", "--target", &triple])
        .arg("--manifest-path")
        .arg(&manifest)
        .args(["--test", spec.crate_name, "--message-format=json"])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not run `{cargo}`: {e}"))?;
    // Mirror cargo's stderr for the reader, since it was captured.
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("no test target named") {
            return Err(format!(
                "the harness needs the test in a top-level `tests/{}.rs` file with an underscore \
                 name (cargo test target `{}`); it cannot run from `src/` or a hyphenated file",
                spec.crate_name, spec.crate_name
            ));
        }
        return Err(format!(
            "`cargo test --no-run --target {triple}` failed ({}); if the target is missing, \
             run `rustup target add {triple}`",
            out.status
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let path = test_artifact(&stdout, spec.crate_name).ok_or_else(|| {
        format!(
            "cargo produced no test executable named `{}` for {triple}",
            spec.crate_name
        )
    })?;
    eprintln!(
        "integration: built {} in {:.1}s",
        path.display(),
        t0.elapsed().as_secs_f64()
    );
    Ok(path)
}

/// Find the `executable` of the `compiler-artifact` message for the test
/// target `crate_name` in cargo's `--message-format=json` output.
fn test_artifact(cargo_json: &str, crate_name: &str) -> Option<PathBuf> {
    cargo_json
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-artifact")
        .filter(|m| {
            let target = &m["target"];
            let kind_is_test = target["kind"]
                .as_array()
                .is_some_and(|k| k.iter().any(|v| v == "test"));
            let name = target["name"].as_str().unwrap_or_default();
            kind_is_test && name == crate_name
        })
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
}

/// The libtest path of a test: `module_path!()` minus the crate name, plus the
/// function name. `--exact` needs it verbatim.
fn test_path(module_path: &str, crate_name: &str, name: &str) -> String {
    let rest = module_path
        .strip_prefix(crate_name)
        .unwrap_or(module_path)
        .trim_start_matches("::");
    if rest.is_empty() {
        name.to_string()
    } else {
        format!("{rest}::{name}")
    }
}

/// The command that runs exactly one test inside the container with its
/// output visible, under coreutils `timeout` so a hung body cannot block CI.
fn test_args(test_path: &str) -> [&str; 7] {
    [
        "timeout",
        BODY_TIMEOUT_SECS,
        "/t",
        "--exact",
        test_path,
        "--nocapture",
        "--test-threads=1",
    ]
}

fn run_in_image(bin: &Path, image: &str, test_path: &str) -> ImageResult {
    let t0 = Instant::now();
    let mount = format!("{}:/t:ro", bin.display());
    let out = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--label",
            CONTAINER_LABEL,
            "-v",
            &mount,
            "-e",
        ])
        .arg(format!("{IMAGE_VAR}={image}"))
        .args(["-e", &format!("{INSIDE_VAR}=1")])
        .arg(image)
        .args(test_args(test_path))
        .stdin(Stdio::null())
        .output();
    finish(
        image,
        t0,
        out.map_err(|e| format!("could not run docker: {e}")),
    )
}

/// Boot `image` with systemd as PID 1, wait for it to settle, run the test
/// with `docker exec`, and remove the container whatever happened.
fn run_in_systemd_image(bin: &Path, image: &str, test_path: &str) -> ImageResult {
    let t0 = Instant::now();
    let mount = format!("{}:/t:ro", bin.display());
    let started = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--label",
            CONTAINER_LABEL,
            "--privileged",
            "--cgroupns=host",
            "-v",
            "/sys/fs/cgroup:/sys/fs/cgroup:rw",
            "--tmpfs",
            "/run",
            "--tmpfs",
            "/run/lock",
            "-v",
            &mount,
            image,
            "/sbin/init",
        ])
        .stdin(Stdio::null())
        .output();
    /// Removes the container when dropped, so a panic on the host side (or
    /// an early return) never leaves a privileged systemd container running.
    struct Container(String);
    impl Drop for Container {
        fn drop(&mut self) {
            let _ = Command::new("docker")
                .args(["rm", "-f", &self.0])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let id = match started {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(o) => {
            let why = format!(
                "docker run {image} /sbin/init failed ({}):\n{}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim_end()
            );
            return finish(image, t0, Err(why));
        }
        Err(e) => return finish(image, t0, Err(format!("could not run docker: {e}"))),
    };
    let guard = Container(id.clone());
    let out = match wait_for_systemd(&id) {
        Ok(()) => Command::new("docker")
            .args(["exec", "-e"])
            .arg(format!("{IMAGE_VAR}={image}"))
            .args(["-e", &format!("{INSIDE_VAR}=1")])
            .arg(&id)
            .args(test_args(test_path))
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("could not run docker exec: {e}")),
        Err(why) => Err(why),
    };
    drop(guard);
    finish(image, t0, out)
}

fn container_running(id: &str) -> bool {
    Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", id])
        .stdin(Stdio::null())
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

/// Poll `systemctl is-system-running` until systemd reports a final state.
/// `degraded` (some unit failed to start) is accepted: the test decides what
/// it needs. Anything else after [`SYSTEMD_BOOT_TIMEOUT`] is an error naming
/// the last state seen.
///
/// `offline` is *not* terminal: `systemctl` answers `offline` whenever it
/// cannot reach the manager, which includes the first fraction of a second
/// of the container's life, before pid 1 is listening on its private bus.
/// A container that is genuinely dead is caught by [`container_running`]
/// instead, so polling through `offline` costs nothing and removes a race
/// that failed roughly one run in ten on `jrei/systemd-ubuntu:24.04`.
fn wait_for_systemd(id: &str) -> std::result::Result<(), String> {
    let t0 = Instant::now();
    let mut last = String::from("(not started)");
    while t0.elapsed() < SYSTEMD_BOOT_TIMEOUT {
        let out = Command::new("docker")
            .args(["exec", id, "systemctl", "is-system-running"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("could not run docker exec: {e}"))?;
        last = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // Early in boot `systemctl` may answer non-zero with nothing on
        // stdout; a dead container answers the same way. Ask docker which.
        if !out.status.success() && last.is_empty() && !container_running(id) {
            return Err(format!(
                "container {id} is not running (docker exec: {})",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        match last.as_str() {
            "running" | "degraded" => return Ok(()),
            // Terminal states that will not improve. `offline` is absent
            // on purpose: it is also what a not-yet-listening manager says.
            "stopping" | "maintenance" => break,
            _ => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    Err(format!(
        "systemd did not reach `running` in {}s (last state: `{last}`)",
        SYSTEMD_BOOT_TIMEOUT.as_secs()
    ))
}

/// Turn the outcome of the test process (or the reason it never ran) into an
/// [`ImageResult`], folding stderr into the output for the failure message.
fn finish(
    image: &str,
    t0: Instant,
    out: std::result::Result<std::process::Output, String>,
) -> ImageResult {
    let elapsed = t0.elapsed();
    match out {
        Ok(out) => {
            let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.trim().is_empty() {
                output.push_str("\n--- stderr ---\n");
                output.push_str(&stderr);
            }
            let report = parse_report(&output);
            ImageResult {
                image: image.to_string(),
                elapsed,
                report,
                status: out.status.code().unwrap_or(-1),
                output,
            }
        }
        Err(why) => ImageResult {
            image: image.to_string(),
            elapsed,
            report: None,
            status: -1,
            output: why,
        },
    }
}

/// The last report line in a container's output, if any.
fn parse_report(output: &str) -> Option<Report> {
    output
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(REPORT_PREFIX))
        .and_then(|json| serde_json::from_str(json).ok())
}

fn status_word(s: Status) -> &'static str {
    match s {
        Status::Ok => "ok",
        Status::Changed => "changed",
        Status::WouldChange => "would change",
        Status::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_at_crate_root_is_the_name() {
        assert_eq!(test_path("it_file_line", "it_file_line", "line"), "line");
    }

    #[test]
    fn test_path_inside_a_module_keeps_the_module() {
        assert_eq!(
            test_path("it_file_line::nested::deep", "it_file_line", "line"),
            "nested::deep::line"
        );
    }

    #[test]
    fn test_artifact_picks_the_test_target_by_name() {
        let json = r#"
{"reason":"compiler-artifact","target":{"name":"rustible-std","kind":["lib"]},"executable":null}
{"reason":"compiler-artifact","target":{"name":"other","kind":["test"]},"executable":"/x/other-1"}
{"reason":"compiler-artifact","target":{"name":"it_file_line","kind":["test"]},"executable":"/x/it_file_line-2"}
{"reason":"build-finished","success":true}
"#;
        assert_eq!(
            test_artifact(json, "it_file_line"),
            Some(PathBuf::from("/x/it_file_line-2"))
        );
        assert_eq!(test_artifact(json, "missing"), None);
    }

    #[test]
    fn test_artifact_matches_the_exact_target_name_only() {
        // Hyphenated targets are refused earlier with a clear message, so
        // the artifact lookup is exact and never guesses.
        let json = r#"{"reason":"compiler-artifact","target":{"name":"it_x","kind":["test"]},"executable":"/x/it_x"}"#;
        assert_eq!(test_artifact(json, "it_x"), Some(PathBuf::from("/x/it_x")));
        let json = r#"{"reason":"compiler-artifact","target":{"name":"it-x","kind":["test"]},"executable":"/x/it_x"}"#;
        assert_eq!(test_artifact(json, "it_x"), None);
    }

    #[test]
    fn parse_report_reads_the_last_report_line() {
        let report = Report {
            test: "t".into(),
            image: "debian:12".into(),
            distro: "Debian 12".into(),
            steps: vec![StepReport {
                name: "s".into(),
                status: Status::Changed,
                diff: Some("x".into()),
                elapsed_ms: 3,
            }],
            commands: 2,
            elapsed_ms: 10,
            error: None,
        };
        let output = format!(
            "[debian:12]  s ... changed\n{REPORT_PREFIX}{}\ntest t ... ok\n",
            serde_json::to_string(&report).unwrap()
        );
        assert_eq!(parse_report(&output), Some(report));
        assert_eq!(parse_report("nothing here"), None);
    }

    #[test]
    fn image_result_passes_only_with_a_clean_report() {
        let ok = Report {
            test: "t".into(),
            image: "i".into(),
            distro: String::new(),
            steps: vec![],
            commands: 0,
            elapsed_ms: 0,
            error: None,
        };
        let mk = |status, report: Option<Report>| ImageResult {
            image: "i".into(),
            elapsed: Duration::ZERO,
            report,
            status,
            output: String::new(),
        };
        assert!(mk(0, Some(ok.clone())).passed());
        assert!(!mk(101, Some(ok.clone())).passed());
        assert!(!mk(0, None).passed());
        let failed = Report {
            error: Some("boom".into()),
            ..ok
        };
        assert!(!mk(0, Some(failed)).passed());
    }
}
