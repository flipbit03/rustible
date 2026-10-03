//! What `main` receives.

use std::cell::{Cell, OnceCell, RefCell};
use std::io::Write;
use std::ops::Deref;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::channel::Channel;
use crate::error::{Context as _, Error, OutputUnavailable, Result, StepFailed};
use crate::event::{Event, Level, Status, Summary, block_prefix};
use crate::facts::Facts;
use crate::op::{Applied, Intent, Op, Plan};
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
/// `block` and `as_user` clones (vision doc 11.1), the channel, and the
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
/// [`Ctx::check_mode`]), grouping ([`Ctx::block`]), reporting
/// ([`Ctx::log`], [`Ctx::warn`], [`Ctx::skip`]), and moving files between
/// the orchestrator's workspace and this host ([`Ctx::local_file`],
/// [`Ctx::local_secret`], [`Ctx::fetch`]).
///
/// [`Ctx::block`], [`Ctx::as_user`] and their friends hand out further
/// `Ctx` values, and every one of them shares a single step counter and a
/// single summary with this one (vision doc 11.1). However deeply a playbook
/// nests, the report stays one numbered sequence and the counts at the end
/// add up. Each also carries the path of blocks it is inside, so a step
/// reports its `[outer][inner]` prefix and marks every one of them changed.
pub struct Ctx {
    sys: System,
    host: HostInfo,
    shared: Rc<Shared>,
    /// The enclosing blocks, outermost first. Shared with the `Ctx` that
    /// made each one, which reads its frame's `changed` when the closure
    /// returns.
    blocks: Rc<[Rc<BlockFrame>]>,
}

/// One enclosing [`Ctx::block`], as the steps inside it see it.
struct BlockFrame {
    name: String,
    /// Set by any step inside, however deeply nested, that finishes
    /// `changed` or `would change`.
    changed: Cell<bool>,
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
            blocks: Rc::new([]),
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
    /// The returned [`Applied`] derefs to the op's output. A step that would
    /// change in check mode has none (vision doc 12); reading it, through
    /// `Deref` or [`Applied::output`], ends the innermost enclosing
    /// [`Ctx::block`] with a warning, or the playbook body when there is no
    /// block. [`Applied::is_available`] is for branching instead.
    pub fn step<O: Op>(&mut self, name: impl Into<String>, op: O) -> Result<Applied<O::Output>> {
        let name = name.into();
        // A cancelled run stops between steps: nothing is interrupted
        // mid-apply, and no further step starts (vision doc 5.5, 16.10).
        self.shared
            .channel
            .check_cancelled()
            .with_context(|| StepFailed::cancelled(&name, "not started"))?;
        let id = self.next_id();
        let identity = self.sys.identity().label();
        let sink = self.sys.sink().clone();
        let blocks = self.block_path();
        sink.emit(Event::StepStarted {
            id,
            blocks: blocks.clone(),
            name: name.clone(),
            identity: identity.clone(),
        });
        let t0 = Instant::now();

        // Every exit below goes through here, and this is where the status
        // is counted and marked on the enclosing blocks: one place, so a
        // branch cannot report a change its blocks do not hear about.
        let finish = |status: Status, diff: Option<crate::Diff>, note: Option<String>| {
            self.record(status);
            sink.emit(Event::StepFinished {
                id,
                blocks: blocks.clone(),
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
                return Err(e.context(StepFailed::at(&name)));
            }
            Ok(Plan::Satisfied(out)) => {
                finish(Status::Ok, None, None);
                Applied::new(name, Some(out), false, None, t0.elapsed())
            }
            Ok(Plan::Change(intent)) if self.sys.check_mode() => {
                // The one place a step's diff is made: rendered from the
                // intent, so what the report shows is what `apply` would run.
                let diff = intent.diff();
                let note = if op.always_changes() {
                    Some("action".into())
                } else {
                    None
                };
                finish(Status::WouldChange, Some(diff.clone()), note);
                // No apply, so no output: the step would change and the
                // value only exists once it has (vision doc 12). The intent
                // is dropped unexecuted.
                Applied::new(name, None, true, Some(diff), t0.elapsed())
            }
            Ok(Plan::Change(intent)) => {
                let diff = intent.diff();
                if let Err(e) = self.shared.channel.check_cancelled() {
                    finish(Status::Failed, Some(diff), Some(e.chain()));
                    return Err(e.context(StepFailed::cancelled(&name, "not applied")));
                }
                self.sys.set_phase(Phase::Applying);
                let applied = op.apply(&self.sys, intent);
                self.sys.set_phase(Phase::Idle);
                match applied {
                    Err(e) => {
                        finish(Status::Failed, Some(diff), Some(e.chain()));
                        return Err(e.context(StepFailed::at(&name)));
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
                        Applied::new(name, Some(out), false, Some(diff), t0.elapsed())
                    }
                    Ok(out) => {
                        let note = if op.always_changes() {
                            Some("action".into())
                        } else {
                            None
                        };
                        finish(Status::Changed, Some(diff.clone()), note);
                        Applied::new(name, Some(out), true, Some(diff), t0.elapsed())
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
    ///
    /// The counting happens at the sink, not here, so a warning an op writes
    /// with [`System::warn`](crate::system::System::warn) reaches the same
    /// column.
    pub fn warn(&self, msg: impl Into<String>) {
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
    ///
    /// Nothing is checked or applied, so there is no verdict to report and
    /// no [`Status`] for it: this emits [`Event::StepSkipped`], which
    /// carries `reason` where a status would sit, and moves the summary's
    /// `skipped` counter. Takes the next step id, so skips and steps stay
    /// one numbered sequence.
    pub fn skip(&mut self, name: impl Into<String>, reason: impl Into<String>) {
        let id = self.next_id();
        self.bump(|s| s.skipped += 1);
        self.sys.sink().emit(Event::StepSkipped {
            id,
            blocks: self.block_path(),
            name: name.into(),
            reason: reason.into(),
        });
    }

    /// Run `f` as a named block: a grouping of steps, not an operation.
    ///
    /// Every step inside is reported with a `[name] ` prefix
    /// (`[outer][inner] ` when blocks nest) and is counted and numbered
    /// exactly as it would be outside. The block itself draws no step id,
    /// moves no counter and has no line of its own: its only events are
    /// [`Event::BlockStarted`] and [`Event::BlockFinished`], the second on
    /// every way out.
    ///
    /// The returned [`Block`] holds the closure's value and `.changed`, true
    /// when any step inside finished `changed` or `would change`, nested
    /// blocks and [`Ctx::as_user`] children included.
    ///
    /// Under `--check`, reading the output of a would-change step inside the
    /// closure, through `?` on [`Applied::output`] or through `Deref`, ends
    /// the block there: a warning names the block and the step, the block
    /// has no value, and the playbook carries on after it. The innermost
    /// block absorbs. That is the only thing a block absorbs, and only in
    /// check mode: in a real run every step has applied, so the read cannot
    /// fail, and an `OutputUnavailable` that turns up anyway is returned as
    /// the error it is.
    ///
    /// Any other error is returned unchanged. When it did not come from a
    /// failed step, whose own line already carries the block's prefix, a
    /// `[path] block ended with an error: ..` warning says where it ended,
    /// printed once however many blocks it leaves. Any other panic is resumed
    /// untouched.
    pub fn block<T>(
        &mut self,
        name: impl Into<String>,
        f: impl FnOnce(&mut Ctx) -> Result<T>,
    ) -> Result<Block<T>> {
        let frame = Rc::new(BlockFrame {
            name: name.into(),
            changed: Cell::new(false),
        });
        let mut inner = Ctx {
            sys: self.sys.clone(),
            host: self.host.clone(),
            shared: self.shared.clone(),
            blocks: self.blocks.iter().cloned().chain([frame.clone()]).collect(),
        };
        let path = inner.block_path();
        let sink = self.sys.sink().clone();
        sink.emit(Event::BlockStarted {
            blocks: path.clone(),
        });
        let outcome = catch_unwind(AssertUnwindSafe(|| f(&mut inner)));
        drop(inner);
        let finished = || {
            sink.emit(Event::BlockFinished {
                blocks: path.clone(),
            })
        };
        let check_mode = self.sys.check_mode();

        // A missing output under `--check`, as an error or as the typed
        // payload `Deref` unwinds with, is the one thing absorbed. Everything
        // else leaves the block as it arrived, after `BlockFinished`.
        let missing = match outcome {
            Ok(Ok(value)) => {
                finished();
                return Ok(Block {
                    value: Some(value),
                    missing: None,
                    changed: frame.changed.get(),
                });
            }
            Ok(Err(e)) => match e.downcast_ref::<OutputUnavailable>() {
                Some(u) if check_mode => u.step.clone(),
                _ => {
                    let mut e = e;
                    if e.step_failed().is_none() && !e.shown_by_block {
                        self.warn(format!(
                            "{} block ended with an error: {}",
                            block_prefix(&path),
                            e.chain()
                        ));
                        e.shown_by_block = true;
                    }
                    finished();
                    return Err(e);
                }
            },
            Err(payload) => match payload.downcast::<OutputUnavailable>() {
                Ok(u) if check_mode => u.step,
                Ok(u) => {
                    finished();
                    resume_unwind(u)
                }
                Err(other) => {
                    finished();
                    resume_unwind(other)
                }
            },
        };
        self.warn(not_evaluated_further(&block_prefix(&path), &missing));
        finished();
        Ok(Block {
            value: None,
            missing: Some(missing),
            changed: frame.changed.get(),
        })
    }

    /// A `Ctx` whose ops run as another user. Same host, same counters.
    pub fn as_user(&self, name: &str) -> Ctx {
        Ctx {
            sys: self.sys.as_user(name),
            host: self.host.clone(),
            shared: self.shared.clone(),
            blocks: self.blocks.clone(),
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

    /// A step's verdict, counted once in the summary and, for a change,
    /// marked on every enclosing block, so a nested block's change reaches
    /// the outer one too. `Ok` (including `ran, unchanged`) and `Failed`
    /// mark nothing.
    fn record(&self, status: Status) {
        self.bump(|s| match status {
            Status::Ok => s.ok += 1,
            Status::Changed => s.changed += 1,
            Status::WouldChange => s.would_change += 1,
            Status::Failed => s.failed += 1,
        });
        if matches!(status, Status::Changed | Status::WouldChange) {
            for frame in self.blocks.iter() {
                frame.changed.set(true);
            }
        }
    }

    fn block_path(&self) -> Vec<String> {
        self.blocks.iter().map(|f| f.name.clone()).collect()
    }

    pub(crate) fn summary(&self) -> Summary {
        self.shared.summary.borrow().clone()
    }
}

/// What [`Ctx::block`] returns: the closure's value, and whether any step
/// inside changed.
///
/// It reads like [`Applied`], but is not one: a block has no diff, no
/// elapsed time and no status, and never stands for a step.
#[derive(Debug)]
pub struct Block<T> {
    value: Option<T>,
    /// The step whose missing output ended the block under `--check`, so
    /// reading the block's own missing value names it.
    missing: Option<String>,
    /// True when any step inside, nested blocks and `as_user` children
    /// included, finished `changed` or, under `--check`, `would change`.
    /// `ok` (including `ran, unchanged`), failed and skipped steps leave it
    /// false. Derived, never declared, so a forgotten `|=` cannot hide a
    /// change from the step that reacts to it.
    pub changed: bool,
}

impl<T> Block<T> {
    /// The closure's value, or [`OutputUnavailable`] naming the step whose
    /// output the block needed when it was ended under `--check`. An
    /// enclosing block absorbs that error like any other read of a missing
    /// output.
    pub fn output(&self) -> Result<&T> {
        self.value.as_ref().ok_or_else(|| self.unavailable().into())
    }

    /// [`Block::output`] by value.
    pub fn into_output(self) -> Result<T> {
        match self.value {
            Some(v) => Ok(v),
            None => Err(OutputUnavailable {
                step: self.missing.unwrap_or_default(),
            }
            .into()),
        }
    }

    /// Whether the block got as far as returning a value: false exactly when
    /// it was ended under `--check` by a missing output.
    pub fn is_available(&self) -> bool {
        self.value.is_some()
    }

    fn unavailable(&self) -> OutputUnavailable {
        OutputUnavailable {
            step: self.missing.clone().unwrap_or_default(),
        }
    }
}

impl<T> Deref for Block<T> {
    type Target = T;

    /// The value. When there is none it unwinds with the same typed
    /// [`OutputUnavailable`] as [`Applied`]'s `Deref`, naming the original
    /// step, so an enclosing block (or the runtime) ends there under
    /// `--check`.
    fn deref(&self) -> &T {
        match &self.value {
            Some(v) => v,
            None => OutputUnavailable::throw(&self.unavailable().step),
        }
    }
}

/// The warning a missing output leaves where it ended evaluation under
/// `--check`. `prefix` is the block path as [`block_prefix`] renders it, or
/// empty for the playbook body, which is the outermost block.
pub(crate) fn not_evaluated_further(prefix: &str, step: &str) -> String {
    let msg = format!(
        "not evaluated further under --check: needs the output of step `{step}`, \
         which would change and so has none"
    );
    if prefix.is_empty() {
        msg
    } else {
        format!("{prefix} {msg}")
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

    /// The probe's whole decision: do it. Unit-shaped, because there is
    /// nothing to choose between.
    #[derive(Debug)]
    struct DoIt;

    impl crate::op::Intent for DoIt {
        fn diff(&self) -> crate::Diff {
            crate::Diff::summary("do it")
        }
    }

    impl Op for Probe {
        type Output = ();
        type Intent = DoIt;
        fn check(&self, _: &System) -> Result<Plan<Self>> {
            self.checks.fetch_add(1, Ordering::SeqCst);
            if let Some(ch) = &self.cancel_in_check {
                ch.cancel("cancelled by the orchestrator");
            }
            Ok(Plan::Change(DoIt))
        }
        fn apply(&self, _: &System, DoIt: DoIt) -> Result<()> {
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

    /// Vision 12 at the SDK: under check mode a would-change step never
    /// reaches `apply` and has no output; a satisfied step keeps its output
    /// in either mode.
    #[test]
    fn check_mode_step_has_no_output_and_never_applies() {
        struct Done;
        impl Op for Done {
            type Output = u32;
            type Intent = std::convert::Infallible;
            fn check(&self, _: &System) -> Result<Plan<Self>> {
                Ok(Plan::Satisfied(7))
            }
            fn apply(&self, _: &System, intent: Self::Intent) -> Result<u32> {
                match intent {}
            }
        }
        let sink = Arc::new(Collect::default());
        let sys = System::fake(Arc::new(Fake::new()), sink).with_check_mode(true);
        let mut ctx = Ctx::new(sys, HostInfo::local());
        let checks = Arc::new(AtomicU32::new(0));
        let applies = Arc::new(AtomicU32::new(0));
        let r = ctx
            .step(
                "would",
                Probe {
                    checks: checks.clone(),
                    applies: applies.clone(),
                    cancel_in_check: None,
                },
            )
            .unwrap();
        assert!(r.changed && !r.is_available());
        assert!(r.diff.is_some(), "the diff is what a dry run has to show");
        let err = r.output().unwrap_err().to_string();
        assert!(err.contains("would have changed"), "{err}");
        assert_eq!(
            (
                checks.load(Ordering::SeqCst),
                applies.load(Ordering::SeqCst)
            ),
            (1, 0)
        );
        let done = ctx.step("done", Done).unwrap();
        assert!(!done.changed && done.is_available());
        assert_eq!(*done, 7);
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

    // ---- what is reported is what runs ----

    /// An intent with a diff no other code path could produce by accident,
    /// so a test can tell the reported diff came from it.
    #[derive(Debug)]
    struct Shown;

    const SHOWN: &str =
        "--- /probe (before)\n+++ /probe (after)\n@@ -1 +1 @@\n-before\n+reported by the intent\n";

    impl crate::op::Intent for Shown {
        fn diff(&self) -> crate::Diff {
            crate::Diff::text("/probe", "before\n", "reported by the intent\n")
        }
    }

    /// What `apply` does with the intent: succeed and count as changed,
    /// succeed and say nothing changed (`changed_when`), or fail.
    #[derive(Clone, Copy)]
    enum Outcome {
        Changed,
        Unchanged,
        Fails,
    }

    struct Reporting {
        outcome: Outcome,
        cancel_in_check: Option<Arc<Channel>>,
    }

    impl Op for Reporting {
        type Output = bool;
        type Intent = Shown;
        fn check(&self, _: &System) -> Result<Plan<Self>> {
            if let Some(ch) = &self.cancel_in_check {
                ch.cancel("cancelled by the orchestrator");
            }
            Ok(Plan::Change(Shown))
        }
        fn apply(&self, _: &System, Shown: Shown) -> Result<bool> {
            match self.outcome {
                Outcome::Changed => Ok(true),
                Outcome::Unchanged => Ok(false),
                Outcome::Fails => Err(crate::Error::msg("the tool said no")),
            }
        }
        fn changed_by_apply(&self, changed: &bool) -> bool {
            *changed
        }
    }

    /// The one `StepFinished` a step emitted: status, rendered diff, note.
    fn finished(sink: &Collect) -> (Status, Option<String>, Option<String>) {
        let mut found = sink.events().into_iter().filter_map(|e| match e {
            Event::StepFinished {
                status, diff, note, ..
            } => Some((status, diff.map(|d| d.render()), note)),
            _ => None,
        });
        let one = found.next().expect("a StepFinished");
        assert!(found.next().is_none(), "exactly one StepFinished");
        one
    }

    fn reporting(outcome: Outcome) -> Reporting {
        Reporting {
            outcome,
            cancel_in_check: None,
        }
    }

    /// The headline claim: the diff a step reports is the one its intent
    /// renders, on every branch of `Ctx::step` that reports a diff.
    #[test]
    fn a_would_change_step_reports_the_intents_diff() {
        let sink = Arc::new(Collect::default());
        let sys = System::fake(Arc::new(Fake::new()), sink.clone()).with_check_mode(true);
        let mut ctx = Ctx::new(sys, HostInfo::local());
        let r = ctx.step("dry", reporting(Outcome::Changed)).unwrap();
        assert_eq!(
            r.diff.as_ref().map(crate::Diff::render).as_deref(),
            Some(SHOWN)
        );
        assert_eq!(
            finished(&sink),
            (Status::WouldChange, Some(SHOWN.to_string()), None)
        );
        let summary = ctx.summary();
        assert_eq!(
            (summary.would_change, summary.changed, summary.ok),
            (1, 0, 0)
        );
    }

    #[test]
    fn a_changed_step_reports_the_intents_diff() {
        let (mut ctx, _channel, _feeder, sink) = ctx_with_channel();
        let r = ctx.step("real", reporting(Outcome::Changed)).unwrap();
        assert!(r.changed && *r);
        assert_eq!(
            r.diff.as_ref().map(crate::Diff::render).as_deref(),
            Some(SHOWN)
        );
        assert_eq!(
            finished(&sink),
            (Status::Changed, Some(SHOWN.to_string()), None)
        );
    }

    #[test]
    fn a_ran_unchanged_step_keeps_the_intents_diff_and_says_so() {
        let (mut ctx, _channel, _feeder, sink) = ctx_with_channel();
        let r = ctx.step("ran", reporting(Outcome::Unchanged)).unwrap();
        assert!(!r.changed);
        assert_eq!(
            r.diff.as_ref().map(crate::Diff::render).as_deref(),
            Some(SHOWN)
        );
        assert_eq!(
            finished(&sink),
            (
                Status::Ok,
                Some(SHOWN.to_string()),
                Some("ran, unchanged".to_string())
            )
        );
    }

    #[test]
    fn a_failed_apply_reports_the_intents_diff() {
        let (mut ctx, _channel, _feeder, sink) = ctx_with_channel();
        let err = ctx.step("boom", reporting(Outcome::Fails)).unwrap_err();
        assert!(err.chain().contains("the tool said no"), "{}", err.chain());
        let (status, diff, note) = finished(&sink);
        assert_eq!((status, diff.as_deref()), (Status::Failed, Some(SHOWN)));
        assert!(note.is_some_and(|n| n.contains("the tool said no")));
    }

    #[test]
    fn a_step_cancelled_after_check_reports_the_intents_diff() {
        let (mut ctx, channel, _feeder, sink) = ctx_with_channel();
        let op = Reporting {
            outcome: Outcome::Changed,
            cancel_in_check: Some(channel),
        };
        ctx.step("cancelled", op).unwrap_err();
        let (status, diff, note) = finished(&sink);
        assert_eq!((status, diff.as_deref()), (Status::Failed, Some(SHOWN)));
        assert!(note.is_some_and(|n| n.contains("cancelled")));
    }

    #[test]
    fn is_change_tells_the_two_plans_apart() {
        assert!(Plan::<Reporting>::Change(Shown).is_change());
        assert!(!Plan::<Reporting>::Satisfied(true).is_change());
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

    // ---- ctx.block ----

    /// A step whose verdict the test picks: satisfied with `out`, or a
    /// change whose `apply` returns `out`, so it reports `would change` and
    /// has no output under check mode.
    struct Verdict {
        change: bool,
        out: u32,
    }

    impl Op for Verdict {
        type Output = u32;
        type Intent = DoIt;
        fn check(&self, _: &System) -> Result<Plan<Self>> {
            Ok(if self.change {
                Plan::Change(DoIt)
            } else {
                Plan::Satisfied(self.out)
            })
        }
        fn apply(&self, _: &System, DoIt: DoIt) -> Result<u32> {
            Ok(self.out)
        }
    }

    fn ok(out: u32) -> Verdict {
        Verdict { change: false, out }
    }

    fn change(out: u32) -> Verdict {
        Verdict { change: true, out }
    }

    /// A `Ctx` over a `Fake`, in check mode or not, and the events it emits.
    fn ctx_in(check_mode: bool) -> (Ctx, Arc<Collect>) {
        let sink = Arc::new(Collect::default());
        let sys = System::fake(Arc::new(Fake::new()), sink.clone()).with_check_mode(check_mode);
        (Ctx::new(sys, HostInfo::local()), sink)
    }

    /// Every `WARNING:` line, as emitted.
    fn warnings(sink: &Collect) -> Vec<String> {
        sink.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Log {
                    level: Level::Warn,
                    msg,
                } => Some(msg),
                _ => None,
            })
            .collect()
    }

    /// The block events in order: `+path` for started, `-path` for finished.
    fn block_events(sink: &Collect) -> Vec<String> {
        sink.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::BlockStarted { blocks } => Some(format!("+{}", blocks.join("/"))),
                Event::BlockFinished { blocks } => Some(format!("-{}", blocks.join("/"))),
                _ => None,
            })
            .collect()
    }

    /// The steps that finished, by name, with the block path they reported.
    fn finished_steps(sink: &Collect) -> Vec<(String, Vec<String>)> {
        sink.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::StepFinished { name, blocks, .. } => Some((name, blocks)),
                _ => None,
            })
            .collect()
    }

    fn path(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    const MISSING: &str = "not evaluated further under --check: needs the output of step \
                           `read`, which would change and so has none";

    #[test]
    fn a_block_returns_its_value_and_is_unchanged_when_every_step_is_ok() {
        let (mut ctx, _sink) = ctx_in(false);
        let b = ctx
            .block("b", |ctx| {
                let a = ctx.step("one", ok(3))?;
                let c = ctx.step("two", ok(4))?;
                Ok(*a + *c)
            })
            .unwrap();
        assert!(!b.changed);
        assert!(b.is_available());
        assert_eq!(*b, 7);
        assert_eq!(*b.output().unwrap(), 7);
        assert_eq!(b.into_output().unwrap(), 7);
    }

    #[test]
    fn a_block_is_changed_when_one_step_changed() {
        let (mut ctx, _sink) = ctx_in(false);
        let b = ctx
            .block("b", |ctx| {
                ctx.step("one", ok(1))?;
                ctx.step("two", change(2))?;
                ctx.step("three", ok(3))?;
                Ok(())
            })
            .unwrap();
        assert!(b.changed);
    }

    #[test]
    fn a_block_is_changed_under_check_when_one_step_would_change() {
        let (mut ctx, _sink) = ctx_in(true);
        let b = ctx
            .block("b", |ctx| {
                ctx.step("one", ok(1))?;
                let w = ctx.step("two", change(2))?;
                assert!(!w.is_available());
                Ok(())
            })
            .unwrap();
        assert!(b.changed);
        assert!(b.is_available(), "nothing read the missing output");
    }

    /// `ran, unchanged` is `ok`, and so are skips and recovered failures:
    /// none of them changed anything.
    #[test]
    fn ran_unchanged_skipped_and_failed_steps_leave_a_block_unchanged() {
        let (mut ctx, _sink) = ctx_in(false);
        let b = ctx
            .block("b", |ctx| {
                let r = ctx.step("ran", reporting(Outcome::Unchanged))?;
                assert!(!r.changed);
                ctx.skip("skipped", "not today");
                assert!(ctx.step("fails", reporting(Outcome::Fails)).is_err());
                Ok(())
            })
            .unwrap();
        assert!(!b.changed);
    }

    /// The change is marked on every frame of the path, so it reaches the
    /// outer block through the inner one, and a sibling block that changed
    /// nothing stays unchanged.
    #[test]
    fn a_nested_change_reaches_every_enclosing_block() {
        let (mut ctx, _sink) = ctx_in(false);
        let mut inner = None;
        let mut quiet = None;
        let mut middle = None;
        let outer = ctx
            .block("outer", |ctx| {
                quiet = Some(ctx.block("quiet", |ctx| ctx.step("ok", ok(1)).map(drop))?);
                middle = Some(ctx.block("middle", |ctx| {
                    inner = Some(ctx.block("inner", |ctx| ctx.step("x", change(1)).map(drop))?);
                    Ok(())
                })?);
                Ok(())
            })
            .unwrap();
        assert!(inner.unwrap().changed, "the innermost frame");
        assert!(middle.unwrap().changed, "a frame in between");
        assert!(outer.changed, "the outermost frame");
        assert!(!quiet.unwrap().changed, "a sibling that changed nothing");
    }

    #[test]
    fn a_step_through_as_root_counts_toward_the_block_and_carries_its_prefix() {
        let (mut ctx, sink) = ctx_in(false);
        let b = ctx
            .block("b", |ctx| {
                ctx.as_root().step("as root", change(1))?;
                Ok(())
            })
            .unwrap();
        assert!(b.changed);
        assert_eq!(finished_steps(&sink), [("as root".into(), path(&["b"]))]);
    }

    #[test]
    fn step_events_carry_the_block_path() {
        let (mut ctx, sink) = ctx_in(false);
        ctx.step("top", ok(1)).unwrap();
        ctx.block("a", |ctx| {
            ctx.step("in a", ok(1))?;
            ctx.block("b", |ctx| {
                ctx.step("in b", ok(1))?;
                ctx.skip("skipped in b", "why not");
                Ok(())
            })?;
            ctx.as_escalated().step("in a, escalated", ok(1))?;
            Ok(())
        })
        .unwrap();
        ctx.step("top again", ok(1)).unwrap();

        let mut started = vec![];
        let mut finished = vec![];
        let mut skipped = vec![];
        for e in sink.events() {
            match e {
                Event::StepStarted { name, blocks, .. } => started.push((name, blocks)),
                Event::StepFinished { name, blocks, .. } => finished.push((name, blocks)),
                Event::StepSkipped { name, blocks, .. } => skipped.push((name, blocks)),
                _ => {}
            }
        }
        let expected: Vec<(String, Vec<String>)> = vec![
            ("top".into(), path(&[])),
            ("in a".into(), path(&["a"])),
            ("in b".into(), path(&["a", "b"])),
            ("in a, escalated".into(), path(&["a"])),
            ("top again".into(), path(&[])),
        ];
        assert_eq!(started, expected);
        assert_eq!(finished, expected);
        assert_eq!(skipped, [("skipped in b".into(), path(&["a", "b"]))]);
    }

    /// A block is not a step: it draws no id, and the summary counts exactly
    /// the steps inside it.
    #[test]
    fn a_block_moves_no_counter_and_draws_no_id() {
        let (mut ctx, sink) = ctx_in(false);
        ctx.step("one", ok(1)).unwrap();
        ctx.block("a", |ctx| {
            ctx.step("two", change(1))?;
            ctx.block("b", |ctx| {
                ctx.skip("three", "no");
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
        ctx.step("four", ok(1)).unwrap();
        let s = ctx.summary();
        assert_eq!(
            (s.ok, s.changed, s.would_change, s.skipped, s.failed),
            (2, 1, 0, 1, 0)
        );
        let ids: Vec<u32> = sink
            .events()
            .into_iter()
            .filter_map(|e| match e {
                Event::StepFinished { id, .. } | Event::StepSkipped { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, [1, 2, 3, 4]);
    }

    #[test]
    fn block_events_pair_up_on_a_value() {
        let (mut ctx, sink) = ctx_in(false);
        ctx.block("a", |ctx| ctx.block("b", |_| Ok(())).map(drop))
            .unwrap();
        assert_eq!(block_events(&sink), ["+a", "+a/b", "-a/b", "-a"]);
    }

    #[test]
    fn block_events_pair_up_on_an_error() {
        let (mut ctx, sink) = ctx_in(false);
        let err = ctx
            .block("a", |ctx| {
                ctx.block("b", |ctx| {
                    ctx.step("fails", reporting(Outcome::Fails)).map(drop)
                })?;
                Ok(())
            })
            .unwrap_err();
        assert!(err.chain().contains("the tool said no"), "{}", err.chain());
        assert_eq!(block_events(&sink), ["+a", "+a/b", "-a/b", "-a"]);
    }

    #[test]
    fn block_events_pair_up_on_absorption() {
        let (mut ctx, sink) = ctx_in(true);
        ctx.block("a", |ctx| {
            let r = ctx.step("read", change(1))?;
            ctx.block("b", |_| Ok(*r))?;
            Ok(())
        })
        .unwrap();
        assert_eq!(block_events(&sink), ["+a", "+a/b", "-a/b", "-a"]);
    }

    /// A panic that is not a missing output is not the block's: it is
    /// resumed untouched, payload and all, in check mode too, after
    /// `BlockFinished` has gone out.
    #[test]
    fn a_foreign_panic_is_resumed_after_block_finished() {
        let (mut ctx, sink) = ctx_in(true);
        let payload = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = ctx.block("a", |ctx| {
                ctx.block("b", |_| -> Result<()> { std::panic::panic_any(42u8) })?;
                Ok(())
            });
        }))
        .unwrap_err();
        assert_eq!(payload.downcast_ref::<u8>(), Some(&42));
        assert_eq!(block_events(&sink), ["+a", "+a/b", "-a/b", "-a"]);
        assert!(warnings(&sink).is_empty(), "{:?}", warnings(&sink));
    }

    /// The motivating shape: read, compare, act, all inside one block. The
    /// `?` on `.output()` ends the block, the step after it inside the block
    /// is not reached, and the step after the block runs.
    #[test]
    fn under_check_a_missing_output_read_with_question_mark_ends_the_block() {
        let (mut ctx, sink) = ctx_in(true);
        let b = ctx
            .block("folder", |ctx| {
                let got = ctx.step("read", change(1))?;
                let n = *got.output()?;
                ctx.step("never reached", ok(n))?;
                Ok(n)
            })
            .unwrap();
        assert!(b.changed, "the read would change");
        assert!(!b.is_available());
        if b.changed {
            ctx.step("restart", change(0)).unwrap();
        }
        assert_eq!(warnings(&sink), [format!("[folder] {MISSING}")]);
        assert_eq!(
            finished_steps(&sink),
            [
                ("read".into(), path(&["folder"])),
                ("restart".into(), path(&[]))
            ]
        );
        assert_eq!(block_events(&sink), ["+folder", "-folder"]);
    }

    #[test]
    fn under_check_a_missing_output_read_through_deref_ends_the_block() {
        let (mut ctx, sink) = ctx_in(true);
        let b = ctx
            .block("folder", |ctx| {
                let got = ctx.step("read", change(1))?;
                let ones = got.count_ones(); // a method call through `Deref`
                ctx.step("never reached", ok(ones))?;
                Ok(*got + 1)
            })
            .unwrap();
        assert!(!b.is_available());
        ctx.step("after", ok(1)).unwrap();
        assert_eq!(warnings(&sink), [format!("[folder] {MISSING}")]);
        assert_eq!(
            finished_steps(&sink),
            [
                ("read".into(), path(&["folder"])),
                ("after".into(), path(&[]))
            ]
        );
    }

    /// The block's own missing value names the step that caused it, by every
    /// route, and as the same typed signal.
    #[test]
    fn an_absorbed_blocks_value_names_the_original_step() {
        let (mut ctx, _sink) = ctx_in(true);
        let b = ctx
            .block("folder", |ctx| {
                let got = ctx.step("read", change(1))?;
                Ok(*got)
            })
            .unwrap();
        let err = b.output().unwrap_err();
        assert_eq!(
            err.downcast_ref::<OutputUnavailable>().unwrap().step,
            "read"
        );
        let payload = std::panic::catch_unwind(AssertUnwindSafe(|| *b)).unwrap_err();
        assert_eq!(
            payload.downcast::<OutputUnavailable>().unwrap().step,
            "read"
        );
        let err = b.into_output().unwrap_err();
        assert_eq!(
            err.downcast_ref::<OutputUnavailable>().unwrap().step,
            "read"
        );
    }

    /// The innermost block absorbs; the outer one goes on to its next step
    /// and returns its value.
    #[test]
    fn the_innermost_block_absorbs_and_the_outer_continues() {
        let (mut ctx, sink) = ctx_in(true);
        let outer = ctx
            .block("outer", |ctx| {
                let inner = ctx.block("inner", |ctx| {
                    let got = ctx.step("read", change(1))?;
                    Ok(*got)
                })?;
                assert!(!inner.is_available());
                ctx.step("outer goes on", ok(1))?;
                Ok("done")
            })
            .unwrap();
        assert_eq!(*outer, "done");
        assert_eq!(warnings(&sink), [format!("[outer][inner] {MISSING}")]);
        assert_eq!(
            finished_steps(&sink),
            [
                ("read".into(), path(&["outer", "inner"])),
                ("outer goes on".into(), path(&["outer"]))
            ]
        );
    }

    /// Reading an absorbed block's value is itself a missing-output read,
    /// absorbed by the block around it, and still names the original step.
    #[test]
    fn reading_an_absorbed_blocks_value_is_absorbed_by_the_outer_block() {
        for through_deref in [true, false] {
            let (mut ctx, sink) = ctx_in(true);
            let outer = ctx
                .block("outer", |ctx| {
                    let inner = ctx.block("inner", |ctx| {
                        let got = ctx.step("read", change(1))?;
                        Ok(*got)
                    })?;
                    let v = if through_deref {
                        *inner
                    } else {
                        *inner.output()?
                    };
                    ctx.step("never reached", ok(v))?;
                    Ok(v)
                })
                .unwrap();
            assert!(!outer.is_available());
            assert_eq!(
                outer
                    .output()
                    .unwrap_err()
                    .downcast_ref::<OutputUnavailable>()
                    .unwrap()
                    .step,
                "read"
            );
            assert_eq!(
                warnings(&sink),
                [
                    format!("[outer][inner] {MISSING}"),
                    format!("[outer] {MISSING}")
                ]
            );
        }
    }

    /// A block absorbs a read of a step outside it too: it cannot go on
    /// either way.
    #[test]
    fn a_block_absorbs_a_read_of_a_step_outside_it() {
        let (mut ctx, sink) = ctx_in(true);
        let got = ctx.step("read", change(1)).unwrap();
        let b = ctx.block("uses it", |_| Ok(*got + 1)).unwrap();
        assert!(!b.is_available());
        assert_eq!(warnings(&sink), [format!("[uses it] {MISSING}")]);
    }

    /// In a real run nothing is absorbed. An `OutputUnavailable` returned by
    /// hand is an error like any other, and leaves with its block's line.
    #[test]
    fn a_real_run_does_not_absorb_a_returned_output_unavailable() {
        let (mut ctx, sink) = ctx_in(false);
        let err = ctx
            .block("b", |_| -> Result<()> {
                Err(OutputUnavailable {
                    step: "by hand".into(),
                }
                .into())
            })
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<OutputUnavailable>().unwrap().step,
            "by hand"
        );
        assert_eq!(
            warnings(&sink),
            [
                "[b] block ended with an error: step `by hand` would have changed; its output \
              is unavailable in check mode"
            ]
        );
        assert_eq!(block_events(&sink), ["+b", "-b"]);
    }

    /// Nor the typed payload: in a real run it is resumed as it came.
    #[test]
    fn a_real_run_does_not_absorb_the_deref_payload() {
        let (mut ctx, sink) = ctx_in(false);
        let missing: Applied<u32> = Applied::new(
            "nowhere".into(),
            None,
            true,
            None,
            std::time::Duration::ZERO,
        );
        let payload = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = ctx.block("b", |_| Ok(*missing));
        }))
        .unwrap_err();
        assert_eq!(
            payload.downcast::<OutputUnavailable>().unwrap().step,
            "nowhere"
        );
        assert_eq!(block_events(&sink), ["+b", "-b"]);
        assert!(warnings(&sink).is_empty(), "{:?}", warnings(&sink));
    }

    /// The author's own `bail!` gets a line under the block's name, once,
    /// however many blocks it leaves; the error itself is returned untouched.
    #[test]
    fn a_body_error_without_a_failed_step_is_named_once_under_its_block() {
        let (mut ctx, sink) = ctx_in(false);
        let err = ctx
            .block("outer", |ctx| {
                ctx.block("inner", |_| -> Result<()> {
                    crate::bail!("unexpected folder type")
                })?;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(err.chain(), "unexpected folder type");
        assert_eq!(
            warnings(&sink),
            ["[outer][inner] block ended with an error: unexpected folder type"]
        );
        assert_eq!(
            block_events(&sink),
            ["+outer", "+outer/inner", "-outer/inner", "-outer"]
        );
    }

    /// The same in check mode: only a missing output is absorbed there.
    #[test]
    fn under_check_a_body_error_is_still_an_error() {
        let (mut ctx, sink) = ctx_in(true);
        let err = ctx
            .block("b", |_| -> Result<()> { crate::bail!("nope") })
            .unwrap_err();
        assert_eq!(err.chain(), "nope");
        assert_eq!(warnings(&sink), ["[b] block ended with an error: nope"]);
    }

    /// A failed step's `FAILED` line already carries the prefix, so its
    /// error leaves the block with no extra line.
    #[test]
    fn a_failed_step_adds_no_block_line() {
        let (mut ctx, sink) = ctx_in(false);
        let err = ctx
            .block("b", |ctx| {
                ctx.step("fails", reporting(Outcome::Fails)).map(drop)
            })
            .unwrap_err();
        assert_eq!(err.step_failed().unwrap().step, "fails");
        assert!(warnings(&sink).is_empty(), "{:?}", warnings(&sink));
    }

    /// `Applied`'s `Deref` unwinds with the typed payload, not a string
    /// panic, so a catcher can tell it from a bug.
    #[test]
    fn applied_deref_unwinds_with_a_typed_payload() {
        let missing: Applied<u32> =
            Applied::new("read".into(), None, true, None, std::time::Duration::ZERO);
        let payload = std::panic::catch_unwind(AssertUnwindSafe(|| *missing)).unwrap_err();
        let u = payload
            .downcast::<OutputUnavailable>()
            .expect("an OutputUnavailable payload, not a string");
        assert_eq!(u.step, "read");
    }
}
