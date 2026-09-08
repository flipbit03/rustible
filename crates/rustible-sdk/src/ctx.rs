//! What `main` receives.

use std::cell::{Cell, OnceCell, RefCell};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::channel::Channel;
use crate::error::{Context as _, Error, Result};
use crate::event::{Event, Level, Status, Summary};
use crate::facts::Facts;
use crate::op::{Applied, Op, Plan};
use crate::protocol::MAX_FRAME_PAYLOAD;
use crate::secret::Secret;
use crate::stream::{Chunk, chunks, write_chunks};
use crate::system::{Identity, Phase, System};

/// The inventory's view of the host this process is configuring: its name,
/// the groups it inherits from, and the connection and escalation parameters
/// resolved for it (vision doc 10.1).
///
/// The orchestrator resolves all of this and sends it in the `Start` frame.
/// A binary run by hand builds it with [`HostInfo::local`] instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    /// The host's name in the inventory file, which need not resolve in DNS.
    /// The report labels this host with it, and [`Ctx::fetch`] uses it as the
    /// per-host directory so two hosts do not overwrite each other.
    pub name: String,
    /// Every group the host belongs to, nearest first and transitively
    /// closed, so a playbook can branch on membership without reading the
    /// inventory. Empty for a host that is in no group.
    pub groups: Vec<String>,
    /// The inventory's privileged account for this host (default `root`);
    /// what `escalate = true` launches as and `as_escalated()` switches to.
    #[serde(default = "default_escalate_user")]
    pub escalate_user: String,
    /// `"ssh"` or `"local"`.
    #[serde(default = "default_connection")]
    pub connection: String,
    /// The inventory's `escalate` parameter: `"sudo"`, `"doas"`, or
    /// `"none"`. How `as_user` reaches other identities (vision doc 11.3).
    #[serde(default = "default_escalate_method")]
    pub escalate_method: String,
}

fn default_escalate_user() -> String {
    "root".into()
}

fn default_escalate_method() -> String {
    "sudo".into()
}

fn default_connection() -> String {
    "local".into()
}

impl HostInfo {
    /// The host a plain local run configures: named `local`, in no group,
    /// connection `local`, escalating to `root` through `sudo`. Used by
    /// `--check-vars` and by tests; a run driven by an orchestrator gets its
    /// `HostInfo` from the `Start` frame instead.
    pub fn local() -> Self {
        HostInfo {
            name: "local".into(),
            groups: vec![],
            escalate_user: default_escalate_user(),
            connection: default_connection(),
            escalate_method: default_escalate_method(),
        }
    }
}

/// State every `Ctx` of a run shares: one step sequence and summary across
/// `section` and `as_user` clones (vision doc 11.1), the channel, and the
/// directory streamed files land in, removed when the run's last `Ctx` drops.
pub(crate) struct Shared {
    pub(crate) step_counter: Cell<u32>,
    pub(crate) summary: RefCell<Summary>,
    channel: Arc<Channel>,
    run_id: String,
    tempdir: OnceCell<RunDir>,
}

/// The run's temp directory, removed when the run's last `Ctx` drops.
///
/// Named `.rustible-<run id>` rather than randomly, so that an orchestrator
/// that has to kill this process can remove it too: SIGKILL runs no
/// destructor, and a random name is known only here.
struct RunDir(PathBuf);

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Shared {
    fn tempdir(&self) -> Result<&Path> {
        if let Some(d) = self.tempdir.get() {
            return Ok(&d.0);
        }
        let path = std::env::temp_dir().join(crate::stream::run_dir_name(&self.run_id));
        // `create_dir`, not `create_dir_all`: it fails on anything already
        // at that path instead of following it. The name is derived from
        // the run id, which is predictable enough to squat in a
        // world-writable /tmp, and this run's files are not for sharing.
        std::fs::create_dir(&path)
            .with_context(|| format!("creating the run's temp directory {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("securing {}", path.display()))?;
        }
        Ok(&self.tempdir.get_or_init(|| RunDir(path)).0)
    }
}

/// The handle a playbook body is given: one host, one run.
///
/// Everything a playbook does goes through it. [`Ctx::step`] is the only
/// verb; the rest is context ([`Ctx::host`], [`Ctx::facts`],
/// [`Ctx::check_mode`]), reporting ([`Ctx::log`], [`Ctx::warn`],
/// [`Ctx::skip`], [`Ctx::section`]), and moving files between the
/// orchestrator's workspace and this host ([`Ctx::local_file`],
/// [`Ctx::local_secret`], [`Ctx::fetch`]).
///
/// [`Ctx::section`], [`Ctx::as_user`] and their friends hand out further
/// `Ctx` values, and every one of them shares a single step counter and a
/// single summary with this one (vision doc 11.1). However deeply a playbook
/// nests, the report stays one numbered sequence and the counts at the end
/// add up.
pub struct Ctx {
    sys: System,
    host: HostInfo,
    shared: Rc<Shared>,
    depth: u8,
}

impl Ctx {
    /// A context with no orchestrator behind it: steps work, `local_file`
    /// and friends fail. Tests and ad-hoc use.
    pub fn new(sys: System, host: HostInfo) -> Self {
        Self::with_channel(sys, host, Channel::detached())
    }

    /// A run with no id from an orchestrator: the temp directory is named
    /// from the process id, which is unique among live runs on this host.
    pub fn with_channel(sys: System, host: HostInfo, channel: Arc<Channel>) -> Self {
        Self::for_run(sys, host, channel, format!("pid{}", std::process::id()))
    }

    /// The orchestrator-driven form. `run_id` comes from `Start` and names
    /// the run's temp directory, so the orchestrator can remove it after
    /// killing a binary that ignored `Cancel`.
    pub fn for_run(
        sys: System,
        host: HostInfo,
        channel: Arc<Channel>,
        run_id: impl Into<String>,
    ) -> Self {
        Ctx {
            sys,
            host,
            shared: Rc::new(Shared {
                step_counter: Cell::new(0),
                summary: RefCell::new(Summary::default()),
                channel,
                run_id: run_id.into(),
                tempdir: OnceCell::new(),
            }),
            depth: 0,
        }
    }

    // ---- tier 1 ----

    /// The one verb. Reconcile an op, report it, return its typed output.
    ///
    /// Calls [`Op::check`]. A satisfied op finishes the step `ok` and
    /// [`Op::apply`] is never reached. A change finishes `would change` in
    /// check mode; otherwise `apply` runs and the step finishes `changed`,
    /// or `ok` when the op's [`Op::changed_by_apply`] reports that running
    /// it altered nothing. Whichever way it goes, one `StepStarted` and one
    /// `StepFinished` event are emitted and exactly one counter in the run
    /// summary moves.
    ///
    /// `name` is what the report shows and what error messages quote. It is
    /// not an identifier: nothing dedupes on it and repeats are fine.
    ///
    /// Errors when `check` or `apply` fails, with `` `step <name>` `` added
    /// as the outermost context layer, and when the run has been cancelled.
    /// Cancellation is tested before `check` and again after it, so a
    /// `Cancel` frame stops the run between steps and never interrupts an
    /// `apply` half way through (vision doc 5.5, 16.10). A failed step does
    /// not by itself end the playbook; the `?` in the playbook body does.
    ///
    /// The returned [`Applied`] derefs to the op's output and *panics* on
    /// deref when there is none, which happens for a step that would change
    /// in check mode unless the op predicted its output. Reach for
    /// [`Applied::is_available`] or [`Applied::output`] to handle that
    /// instead of panicking.
    pub fn step<O: Op>(&mut self, name: impl Into<String>, op: O) -> Result<Applied<O::Output>> {
        let name = name.into();
        // A cancelled run stops between steps: nothing is interrupted
        // mid-apply, and no further step starts (vision doc 5.5, 16.10).
        self.shared
            .channel
            .check_cancelled()
            .with_context(|| format!("step `{name}` not started"))?;
        let id = self.next_id();
        let identity = self.sys.identity().label();
        let sink = self.sys.sink().clone();
        sink.emit(Event::StepStarted {
            id,
            depth: self.depth,
            name: name.clone(),
            identity: identity.clone(),
        });
        let t0 = Instant::now();

        let finish = |status: Status, diff: Option<crate::Diff>, note: Option<String>| {
            sink.emit(Event::StepFinished {
                id,
                depth: self.depth,
                name: name.clone(),
                identity: identity.clone(),
                status,
                diff: diff.clone(),
                note,
                elapsed_ms: t0.elapsed().as_millis() as u64,
            });
        };

        self.sys.set_phase(Phase::Checking);
        let plan = op.check(&self.sys);
        self.sys.set_phase(Phase::Idle);

        let result = match plan {
            Err(e) => {
                finish(Status::Failed, None, Some(e.chain()));
                self.bump(|s| s.failed += 1);
                return Err(e.context(format!("step `{name}`")));
            }
            Ok(Plan::Satisfied(out)) => {
                finish(Status::Ok, None, None);
                self.bump(|s| s.ok += 1);
                Applied::new(name, Some(out), false, false, None, t0.elapsed())
            }
            Ok(Plan::Change(change)) if self.sys.check_mode() => {
                let note = if op.always_changes() {
                    Some("action".into())
                } else {
                    None
                };
                finish(Status::WouldChange, Some(change.diff.clone()), note);
                self.bump(|s| s.would_change += 1);
                let predicted = change.predicted.is_some();
                Applied::new(
                    name,
                    change.predicted,
                    true,
                    predicted,
                    Some(change.diff),
                    t0.elapsed(),
                )
            }
            Ok(Plan::Change(change)) => {
                let diff = change.diff.clone();
                if let Err(e) = self.shared.channel.check_cancelled() {
                    finish(Status::Failed, Some(diff), Some(e.chain()));
                    self.bump(|s| s.failed += 1);
                    return Err(e.context(format!("step `{name}` not applied")));
                }
                self.sys.set_phase(Phase::Applying);
                let applied = op.apply(&self.sys, change);
                self.sys.set_phase(Phase::Idle);
                match applied {
                    Err(e) => {
                        finish(Status::Failed, Some(diff), Some(e.chain()));
                        self.bump(|s| s.failed += 1);
                        return Err(e.context(format!("step `{name}`")));
                    }
                    Ok(out) if !op.changed_by_apply(&out) => {
                        // The op ran and decided nothing changed (a command
                        // with `changed_when`). Reported `ok`; the diff stays
                        // so verbose output shows what ran.
                        finish(
                            Status::Ok,
                            Some(diff.clone()),
                            Some("ran, unchanged".into()),
                        );
                        self.bump(|s| s.ok += 1);
                        Applied::new(name, Some(out), false, false, Some(diff), t0.elapsed())
                    }
                    Ok(out) => {
                        let note = if op.always_changes() {
                            Some("action".into())
                        } else {
                            None
                        };
                        finish(Status::Changed, Some(diff.clone()), note);
                        self.bump(|s| s.changed += 1);
                        Applied::new(name, Some(out), true, false, Some(diff), t0.elapsed())
                    }
                }
            }
        };
        Ok(result)
    }

    /// The inventory's view of this host, including the `escalate_user` and
    /// `escalate_method` that [`Ctx::as_escalated`] follows.
    pub fn host(&self) -> &HostInfo {
        &self.host
    }

    /// What was gathered from the machine when the run started. Read once,
    /// not re-read per step, so an op that changes the system does not change
    /// the facts under a later branch.
    pub fn facts(&self) -> &Facts {
        self.sys.facts()
    }

    /// True under `--check`. Ops seldom need it, since the `check`/`apply`
    /// split already keeps them honest; a playbook needs it when its own
    /// control flow would otherwise act on an output no step produced.
    pub fn check_mode(&self) -> bool {
        self.sys.check_mode()
    }

    /// A line in the report at any verbosity, for something the user should
    /// see that is not a step. Use [`Ctx::debug`] for detail worth `-v` only.
    pub fn log(&self, msg: impl Into<String>) {
        self.sys.sink().emit(Event::Log {
            level: Level::Info,
            msg: msg.into(),
        });
    }

    /// A `WARNING:` line, and one more on the run's warning count that the
    /// summary prints at the end. Nothing fails; this is how a playbook says
    /// something is off without giving up on the host.
    pub fn warn(&self, msg: impl Into<String>) {
        self.bump(|s| s.warnings += 1);
        self.sys.warn(msg);
    }

    /// A line shown only at `-v` and above. The SDK uses it for byte counts
    /// and temp paths, which are noise at the default verbosity. Unlike
    /// [`Ctx::warn`], it touches no counter.
    pub fn debug(&self, msg: impl Into<String>) {
        self.sys.debug(msg);
    }

    /// Direct access to the machine. Reads are fine; mutations should be steps.
    pub fn sys(&self) -> &System {
        &self.sys
    }

    /// A file from the workspace (path relative to its root), streamed over
    /// the channel into the run's temp directory on this host. The path is
    /// removed when the run ends. Anything outside the workspace is denied
    /// by the orchestrator (vision doc 5.6).
    pub fn local_file(&mut self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let requested = path_str(path.as_ref());
        let name = Path::new(&requested)
            .file_name()
            .ok_or_else(|| Error::msg(format!("`{requested}` has no file name")))?
            .to_owned();
        let n = self.shared.channel.next_req();
        let dir = self.shared.tempdir()?.join(n.to_string());
        std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let dest = dir.join(name);
        let mut file =
            std::fs::File::create(&dest).with_context(|| format!("creating {}", dest.display()))?;
        let mut received = 0u64;
        self.shared.channel.stream_file(&requested, &mut |chunk| {
            received = write_chunks(&mut file, &chunk, received)
                .with_context(|| format!("writing {}", dest.display()))?;
            Ok(())
        })?;
        file.flush()
            .with_context(|| format!("writing {}", dest.display()))?;
        self.debug(format!(
            "{requested}: {received} bytes at {}",
            dest.display()
        ));
        Ok(dest)
    }

    /// A workspace file streamed into memory only: never on this host's
    /// disk, zeroized when the `Secret` drops (vision doc 5.6, 11).
    pub fn local_secret(&mut self, path: impl AsRef<Path>) -> Result<Secret> {
        let requested = path_str(path.as_ref());
        let mut secret = Secret::new(Vec::new());
        let mut received = 0u64;
        self.shared.channel.stream_file(&requested, &mut |chunk| {
            if chunk.offset != received {
                return Err(Error::msg(format!(
                    "`{requested}`: chunk at offset {} after {received} bytes",
                    chunk.offset
                )));
            }
            received += chunk.bytes.len() as u64;
            secret.push(&chunk.bytes);
            Ok(())
        })?;
        self.debug(format!("{requested}: {received} secret bytes in memory"));
        Ok(secret)
    }

    // ---- tier 2 ----

    /// Record a step that was deliberately not run.
    pub fn skip(&mut self, name: impl Into<String>, reason: impl Into<String>) {
        let id = self.next_id();
        self.bump(|s| s.skipped += 1);
        self.sys.sink().emit(Event::StepSkipped {
            id,
            depth: self.depth,
            name: name.into(),
            reason: reason.into(),
        });
    }

    /// Group steps under a heading in the output. Output only.
    pub fn section<T>(
        &mut self,
        name: impl Into<String>,
        f: impl FnOnce(&mut Ctx) -> Result<T>,
    ) -> Result<T> {
        let name = name.into();
        let sink = self.sys.sink().clone();
        sink.emit(Event::SectionStarted {
            depth: self.depth,
            name: name.clone(),
        });
        let mut inner = Ctx {
            sys: self.sys.clone(),
            host: self.host.clone(),
            shared: self.shared.clone(),
            depth: self.depth + 1,
        };
        let r = f(&mut inner);
        sink.emit(Event::SectionFinished {
            depth: self.depth,
            name,
        });
        r
    }

    /// A `Ctx` whose ops run as another user. Same host, same counters.
    pub fn as_user(&self, name: &str) -> Ctx {
        Ctx {
            sys: self.sys.as_user(name),
            host: self.host.clone(),
            shared: self.shared.clone(),
            depth: self.depth,
        }
    }

    /// Literally root. Never follows the inventory (vision 11.3).
    pub fn as_root(&self) -> Ctx {
        self.as_user("root")
    }

    /// The inventory's privileged account for this host (`escalate_user`).
    pub fn as_escalated(&self) -> Ctx {
        let user = self.host.escalate_user.clone();
        self.as_user(&user)
    }

    /// Reverse transfer: send a file from this host to the workspace on the
    /// orchestrator. `local_dest` is relative to the workspace root; a
    /// trailing `/` means a directory, and the file lands at
    /// `<dest>/<host name>/<file name>` so several hosts do not collide
    /// (Ansible's `fetch` layout). The read goes through `sys`, so an
    /// escalated `Ctx` fetches what its identity can read.
    pub fn fetch(&mut self, remote: impl AsRef<Path>, local_dest: impl AsRef<Path>) -> Result<()> {
        let remote = remote.as_ref();
        let dest = path_str(local_dest.as_ref());
        let dest = if dest.ends_with('/') {
            let name = remote
                .file_name()
                .ok_or_else(|| Error::msg(format!("`{}` has no file name", remote.display())))?;
            format!("{dest}{}/{}", self.host.name, name.to_string_lossy())
        } else {
            dest
        };
        // `sys.read` puts the whole file in memory on this host, and an
        // escalated read also puts it in one helper frame, base64-inflated
        // by 4/3 against the frame ceiling. Refuse first, with the numbers
        // and the reason: without this the escalated case failed deep in
        // the framing with "frame of N bytes exceeds limit", naming neither
        // the file nor the helper.
        if let Some(st) = self.sys.stat_follow(remote)?
            && matches!(self.sys.identity(), Identity::User(_))
            && st.size > MAX_FRAME_PAYLOAD as u64
        {
            return Err(Error::msg(format!(
                "fetching {}: {} bytes is more than an escalated read can carry \
                 ({} bytes, the helper's frame limit); a file this large has to be \
                 fetched without `as_user`/`as_root`, or copied to a readable path first",
                remote.display(),
                st.size,
                MAX_FRAME_PAYLOAD
            )));
        }
        let bytes = self
            .sys
            .read(remote)
            .with_context(|| format!("fetching {}", remote.display()))?;
        let req = self.shared.channel.next_req();
        for chunk in chunks(bytes.as_slice()) {
            let chunk: Chunk = chunk.with_context(|| format!("reading {}", remote.display()))?;
            self.shared.channel.send_fetch(req, &dest, &chunk)?;
        }
        self.debug(format!(
            "fetched {} ({} bytes) to {dest}",
            remote.display(),
            bytes.len()
        ));
        Ok(())
    }

    // ---- internals ----

    fn next_id(&self) -> u32 {
        let id = self.shared.step_counter.get() + 1;
        self.shared.step_counter.set(id);
        id
    }

    fn bump(&self, f: impl FnOnce(&mut Summary)) {
        f(&mut self.shared.summary.borrow_mut());
    }

    pub(crate) fn summary(&self) -> Summary {
        self.shared.summary.borrow().clone()
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Fake;
    use crate::event::Collect;
    use crate::protocol::{Down, Up, UpLink};
    use std::sync::atomic::{AtomicU32, Ordering};

    struct NoUp;
    impl UpLink for NoUp {
        fn send(&self, _: &Up) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Counts check and apply calls; optionally cancels the run from inside
    /// `check` to model a Cancel frame arriving mid-step.
    struct Probe {
        checks: Arc<AtomicU32>,
        applies: Arc<AtomicU32>,
        cancel_in_check: Option<Arc<Channel>>,
    }

    impl Op for Probe {
        type Output = ();
        fn check(&self, _: &System) -> Result<Plan<()>> {
            self.checks.fetch_add(1, Ordering::SeqCst);
            if let Some(ch) = &self.cancel_in_check {
                ch.cancel("cancelled by the orchestrator");
            }
            Ok(Plan::change(crate::Diff::summary("do it")))
        }
        fn apply(&self, _: &System, _: crate::Change<()>) -> Result<()> {
            self.applies.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn ctx_with_channel() -> (Ctx, Arc<Channel>, crate::channel::Feeder, Arc<Collect>) {
        let sink = Arc::new(Collect::default());
        let sys = System::fake(Arc::new(Fake::new()), sink.clone());
        let (channel, feeder) = Channel::remote(Arc::new(NoUp));
        (
            Ctx::with_channel(sys, HostInfo::local(), channel.clone()),
            channel,
            feeder,
            sink,
        )
    }

    #[test]
    fn cancelled_run_starts_no_further_step() {
        let (mut ctx, _channel, feeder, sink) = ctx_with_channel();
        let checks = Arc::new(AtomicU32::new(0));
        let applies = Arc::new(AtomicU32::new(0));
        let probe = || Probe {
            checks: checks.clone(),
            applies: applies.clone(),
            cancel_in_check: None,
        };
        ctx.step("first", probe()).unwrap();
        feeder.feed(Down::Cancel);
        let err = ctx.step("second", probe()).unwrap_err().chain();
        assert!(
            err.contains("step `second` not started") && err.contains("cancelled"),
            "{err}"
        );
        assert_eq!(
            (
                checks.load(Ordering::SeqCst),
                applies.load(Ordering::SeqCst)
            ),
            (1, 1)
        );
        // The refused step never started, so no StepStarted for it.
        let started: Vec<String> = sink
            .events()
            .into_iter()
            .filter_map(|e| match e {
                Event::StepStarted { name, .. } => Some(name),
                _ => None,
            })
            .collect();
        assert_eq!(started, ["first"]);
    }

    #[test]
    fn cancel_during_check_skips_apply_and_fails_the_step() {
        let (mut ctx, channel, _feeder, sink) = ctx_with_channel();
        let checks = Arc::new(AtomicU32::new(0));
        let applies = Arc::new(AtomicU32::new(0));
        let err = ctx
            .step(
                "slow",
                Probe {
                    checks: checks.clone(),
                    applies: applies.clone(),
                    cancel_in_check: Some(channel),
                },
            )
            .unwrap_err()
            .chain();
        assert!(
            err.contains("not applied") && err.contains("cancelled"),
            "{err}"
        );
        assert_eq!(
            (
                checks.load(Ordering::SeqCst),
                applies.load(Ordering::SeqCst)
            ),
            (1, 0)
        );
        assert!(sink.events().iter().any(|e| matches!(
            e,
            Event::StepFinished { status: Status::Failed, note: Some(n), .. } if n.contains("cancelled")
        )));
        assert_eq!(ctx.summary().failed, 1);
    }

    #[test]
    fn local_file_lands_in_a_temp_dir_and_secret_stays_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("files")).unwrap();
        std::fs::write(dir.path().join("files/big"), vec![7u8; 3000]).unwrap();
        std::fs::write(dir.path().join("files/token"), b"s3cr3t\n").unwrap();
        let sink = Arc::new(Collect::default());
        let sys = System::fake(Arc::new(Fake::new()), sink);
        let (channel, _feeder) =
            Channel::local(crate::stream::WorkspaceFiles::new(dir.path()).unwrap());
        let mut ctx = Ctx::with_channel(sys, HostInfo::local(), channel);

        let p = ctx.local_file("files/big").unwrap();
        assert!(p.starts_with(std::env::temp_dir()));
        assert_eq!(std::fs::read(&p).unwrap(), vec![7u8; 3000]);
        let run_dir = p.parent().unwrap().parent().unwrap().to_path_buf();

        let secret = ctx.local_secret("files/token").unwrap();
        assert_eq!(secret.as_str().unwrap(), "s3cr3t");
        // Nothing but the streamed file is under the run's temp dir.
        let mut names = vec![];
        for e in walkdir(&run_dir) {
            names.push(e.file_name().unwrap().to_string_lossy().into_owned());
        }
        assert_eq!(names, ["big"]);
        assert!(
            ctx.local_file("../etc/passwd")
                .unwrap_err()
                .chain()
                .contains("denied")
        );

        drop(ctx);
        assert!(!run_dir.exists(), "temp dir removed when the run ends");
    }

    fn walkdir(p: &Path) -> Vec<PathBuf> {
        let mut out = vec![];
        for e in std::fs::read_dir(p).unwrap() {
            let e = e.unwrap().path();
            if e.is_dir() {
                out.extend(walkdir(&e));
            } else {
                out.push(e);
            }
        }
        out
    }

    #[test]
    fn fetch_reads_through_sys_and_lands_under_host_name() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Collect::default());
        let fake = Arc::new(Fake::new().with_file("/etc/hostname", "box1\n"));
        let sys = System::fake(fake, sink);
        let (channel, _feeder) =
            Channel::local(crate::stream::WorkspaceFiles::new(dir.path()).unwrap());
        let mut ctx = Ctx::with_channel(sys, HostInfo::local(), channel);
        ctx.fetch("/etc/hostname", "out/").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("out/local/hostname")).unwrap(),
            b"box1\n"
        );
        ctx.fetch("/etc/hostname", "exact.txt").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("exact.txt")).unwrap(),
            b"box1\n"
        );
        assert!(
            ctx.fetch("/etc/hostname", "../x")
                .unwrap_err()
                .chain()
                .contains("denied")
        );
        assert!(ctx.fetch("/missing", "out/").is_err());
    }
}
