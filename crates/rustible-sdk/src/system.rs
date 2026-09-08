//! The op's handle to the machine. Concrete struct over a swappable backend.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::backend::{Backend, CmdSpec, Elevated, Fake, Local, Output, Spawner, Stat};
use crate::error::{CmdFailed, Error, IoAt, MutationDuringCheck, Result, SpawnFailed};
use crate::event::{Event, Level, SharedSink};
use crate::facts::Facts;
use crate::secret::Secret;

/// Which half of a step is running.
///
/// [`Ctx::step`](crate::ctx::Ctx::step) sets this around each call to
/// `check` and `apply` and puts it back to `Idle` afterwards. Every mutating
/// primitive on [`System`] consults it, so an op that writes from `check`
/// gets a [`MutationDuringCheck`] error instead of quietly breaking dry
/// runs. One atomic is shared by every clone of a `System` and by each
/// `Elevated` helper, so going through [`System::as_user`] does not escape
/// the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    /// Between steps: playbook code, or an op's constructor. Mutations are
    /// allowed, because nothing here is claiming to be a dry run.
    Idle = 0,
    /// Inside `Op::check`, which is contractually read-only. Every mutating
    /// primitive refuses.
    Checking = 1,
    /// Inside `Op::apply`, reached only outside check mode. Mutations go
    /// through.
    Applying = 2,
}

/// Who the primitives on a [`System`] handle run as.
///
/// A handle starts at `Own` and only [`System::as_user`] produces anything
/// else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// Whatever the process runs as.
    Own,
    /// Another user. On a real system every primitive goes through an
    /// `Elevated` helper running as that user (vision doc 11.3); on a `Fake`
    /// system commands get a `sudo -n -u <user>` prefix so tests can assert it.
    User(String),
}

impl Identity {
    /// How the identity appears in the report and in `CmdRan` events:
    /// `"self"` for [`Identity::Own`], otherwise the user name.
    pub fn label(&self) -> String {
        match self {
            Identity::Own => "self".into(),
            Identity::User(u) => u.clone(),
        }
    }
}

/// A named resource an earlier step in this run would create (check mode
/// only). See [`System::note_would_create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    /// The kind of resource, e.g. `"group"`. Ops agree on these strings.
    pub kind: String,
    /// The resource's own name, as the creating op would pass it to the
    /// system. Matched literally, so the looker-up and the noter have to
    /// spell it the same way.
    pub name: String,
    /// A numeric id the creating step knows for sure (a requested gid).
    pub id: Option<u32>,
}

/// How this run reaches other identities: the process's own user, the
/// host's escalation method, and one `Elevated` backend per user, spawned on
/// first use and kept for the run (vision doc 11.3). Only a real system has
/// one; `Fake` systems have none.
pub struct Escalation {
    own_user: String,
    local: Arc<dyn Backend>,
    spawner: Spawner,
    helpers: Mutex<BTreeMap<String, Arc<Elevated>>>,
}

impl Escalation {
    fn backend_for(&self, user: &str, phase: &Arc<AtomicU8>) -> Arc<dyn Backend> {
        if user == self.own_user {
            return self.local.clone();
        }
        let mut helpers = self.helpers.lock().unwrap();
        helpers
            .entry(user.to_string())
            .or_insert_with(|| Arc::new(Elevated::new(user, self.spawner.clone(), phase.clone())))
            .clone()
    }
}

/// The op's handle to one machine, at one identity.
///
/// Everything an op is allowed to do to the world goes through this: the
/// reads, the mutations (guarded by [`Phase`] so `check` cannot cheat),
/// [`System::cmd`], the [`Facts`] gathered at startup, and the check-mode
/// bookkeeping that lets a dry run accept a prerequisite an earlier step
/// planned. The runtime builds one per host and hands it to every op; a
/// playbook never needs to build one outside tests.
///
/// Cloning is cheap and deliberate. A clone shares the backend, the facts,
/// the phase and the planned-resource list with the original, so the handles
/// produced by [`System::as_user`] and by nested sections all speak for one
/// run. What a clone can differ in is its identity, its backend, and its
/// check-mode flag.
///
/// Use [`System::local`] for a real machine and [`System::fake`] for tests.
#[derive(Clone)]
pub struct System {
    backend: Arc<dyn Backend>,
    facts: Arc<Facts>,
    identity: Identity,
    check_mode: bool,
    phase: Arc<AtomicU8>,
    /// Shared by every clone (sections, `as_user`), so one run has one list.
    planned: Arc<Mutex<Vec<Planned>>>,
    sink: SharedSink,
    escalation: Option<Arc<Escalation>>,
}

impl System {
    /// A system over a backend of your own: a test double, a recording
    /// proxy, anything implementing `Backend`. [`System::local`] and
    /// [`System::fake`] cover the two cases that exist in practice.
    ///
    /// The identity is [`Identity::Own`] and the phase starts at
    /// [`Phase::Idle`]. There is no escalation, so [`System::as_user`] on
    /// the result only relabels the handle: primitives keep going to the
    /// same backend, and commands pick up a `sudo -n -u <user>` prefix.
    pub fn new(
        backend: Arc<dyn Backend>,
        facts: Facts,
        check_mode: bool,
        sink: SharedSink,
    ) -> Self {
        System {
            backend,
            facts: Arc::new(facts),
            identity: Identity::Own,
            check_mode,
            phase: Arc::new(AtomicU8::new(Phase::Idle as u8)),
            planned: Arc::new(Mutex::new(Vec::new())),
            sink,
            escalation: None,
        }
    }

    /// Real machine, real facts; other identities through `sudo` without a
    /// password. See `with_escalation` for the inventory's method.
    pub fn local(check_mode: bool, sink: SharedSink) -> Self {
        let backend: Arc<dyn Backend> = Arc::new(Local);
        let facts = Facts::gather(&*backend);
        let mut sys = Self::new(backend.clone(), facts, check_mode, sink);
        sys.escalation = Some(Arc::new(Escalation {
            own_user: sys.facts.user.clone(),
            local: backend,
            spawner: Spawner {
                method: "sudo".into(),
                exe: std::env::current_exe().unwrap_or_default(),
                password: None,
            },
            helpers: Mutex::new(BTreeMap::new()),
        }));
        sys
    }

    /// Set the host's escalation method (`sudo`, `doas`, `none`) and the
    /// password `sudo -S` gets when `sudo -n` is refused. No effect on a
    /// `Fake` system.
    pub fn with_escalation(mut self, method: &str, password: Option<Secret>) -> Self {
        if let Some(esc) = &self.escalation {
            self.escalation = Some(Arc::new(Escalation {
                own_user: esc.own_user.clone(),
                local: esc.local.clone(),
                spawner: Spawner {
                    method: method.to_string(),
                    exe: esc.spawner.exe.clone(),
                    password,
                },
                helpers: Mutex::new(BTreeMap::new()),
            }));
        }
        self
    }

    /// Fake backend for tests. Facts default to a plausible Debian box; use
    /// `with_facts` to change them.
    pub fn fake(fake: Arc<Fake>, sink: SharedSink) -> Self {
        let facts = Facts {
            os: crate::facts::Os::Linux,
            distro: crate::facts::Distro::Debian,
            distro_version: "12".into(),
            arch: crate::facts::Arch::X86_64,
            kernel: "6.1.0-fake".into(),
            hostname: "fake".into(),
            package_manager: crate::facts::Pm::Apt,
            init: crate::facts::Init::Systemd,
            cpus: 2,
            memory_mb: 2048,
            user: "root".into(),
            is_root: true,
        };
        Self::new(fake, facts, false, sink)
    }

    /// Replace the facts, for a test that needs a different distro, init or
    /// unprivileged user than [`System::fake`]'s Debian default. Handles
    /// cloned before this call keep the facts they were built with.
    pub fn with_facts(mut self, facts: Facts) -> Self {
        self.facts = Arc::new(facts);
        self
    }

    /// Turn dry-run mode on or off.
    ///
    /// This flag is not what stops a mutation: [`Phase`] does that, and it
    /// does it in every mode. What the flag decides is that
    /// [`Ctx::step`](crate::ctx::Ctx::step) never calls `apply` at all, and
    /// that the [`System::would_create`] family answers rather than
    /// returning nothing. As with [`System::with_facts`], only handles
    /// cloned after the call see the new value.
    pub fn with_check_mode(mut self, on: bool) -> Self {
        self.check_mode = on;
        self
    }

    // ---- context ----

    /// The facts gathered once before the first step. They describe the
    /// machine as it was then: an op that installs `systemd` does not change
    /// what [`Facts::init`] says for the rest of the run.
    pub fn facts(&self) -> &Facts {
        &self.facts
    }

    /// True during a dry run. The usual reason an op looks is to decide
    /// whether a missing prerequisite may still be satisfied, which
    /// [`System::would_create`] answers.
    pub fn check_mode(&self) -> bool {
        self.check_mode
    }

    /// Who the primitives on this handle run as. [`Identity::Own`] unless
    /// the handle came from [`System::as_user`].
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// True when the primitives on this handle run with uid 0: the
    /// process's own [`Facts::is_root`] for [`Identity::Own`], and simply
    /// whether the name is `root` for a switched identity. An op that
    /// refuses to run unprivileged gates on this rather than on the facts,
    /// so `as_user("root")` counts as root even when the process is not.
    pub fn is_root(&self) -> bool {
        match &self.identity {
            Identity::Own => self.facts.is_root,
            Identity::User(u) => u == "root",
        }
    }

    /// A clone that runs as another user. Not a mutation. On a real system
    /// the clone's backend is the `Elevated` helper for that user (shared by
    /// every clone asking for the same user); asking for the process's own
    /// user gives back the plain local system.
    pub fn as_user(&self, name: &str) -> System {
        let mut s = self.clone();
        match &self.escalation {
            Some(esc) if name == esc.own_user => {
                s.identity = Identity::Own;
                s.backend = esc.local.clone();
            }
            Some(esc) => {
                s.identity = Identity::User(name.to_string());
                s.backend = esc.backend_for(name, &self.phase);
            }
            None => s.identity = Identity::User(name.to_string()),
        }
        s
    }

    // ---- check-mode planning ----

    /// Record that this step's `check` returned a change that creates the
    /// named resource. An op calls this from `check` when it plans a
    /// creation, so a later step in the same dry run can accept the
    /// prerequisite (`group::Present` notes the group, `user::Present` with
    /// `.groups([..])` accepts it through [`would_create`](Self::would_create)).
    /// Nothing is created: vision 6.7 holds, and outside check mode the
    /// record is inert because the real step creates the real thing before
    /// the next step looks.
    pub fn note_would_create(&self, kind: &str, name: impl Into<String>, id: Option<u32>) {
        self.planned.lock().unwrap().push(Planned {
            kind: kind.into(),
            name: name.into(),
            id,
        });
    }

    /// In check mode: has an earlier step planned to create this resource?
    /// Always `false` outside check mode, so a real run never accepts a
    /// prerequisite that is not on the machine.
    pub fn would_create(&self, kind: &str, name: &str) -> bool {
        self.check_mode
            && self
                .planned
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.kind == kind && p.name == name)
    }

    /// In check mode: the planned resource of this kind and name, with the
    /// id the planning step knew, if any. `None` outside check mode.
    pub fn would_create_id_by_name(&self, kind: &str, name: &str) -> Option<Planned> {
        if !self.check_mode {
            return None;
        }
        self.planned
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.kind == kind && p.name == name)
            .cloned()
    }

    /// In check mode: the planned resource of this kind with this id, if an
    /// earlier step planned it with the id known. `None` outside check mode.
    pub fn would_create_id(&self, kind: &str, id: u32) -> Option<Planned> {
        if !self.check_mode {
            return None;
        }
        self.planned
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.kind == kind && p.id == Some(id))
            .cloned()
    }

    pub(crate) fn set_phase(&self, p: Phase) {
        self.phase.store(p as u8, Ordering::SeqCst);
    }

    fn phase(&self) -> Phase {
        match self.phase.load(Ordering::SeqCst) {
            1 => Phase::Checking,
            2 => Phase::Applying,
            _ => Phase::Idle,
        }
    }

    fn guard_mutation(&self, p: &Path) -> Result<()> {
        if self.phase() == Phase::Checking {
            return Err(MutationDuringCheck {
                path: p.to_path_buf(),
            }
            .into());
        }
        Ok(())
    }

    fn io(p: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
        move |source| {
            IoAt {
                path: p.to_path_buf(),
                source,
            }
            .into()
        }
    }

    // ---- reads ----

    /// Whether anything at all sits at `p`, without following symlinks, so
    /// a dangling symlink exists. An absent path is `Ok(false)`, not an
    /// error; the error case is a stat that fails for another reason, such
    /// as a parent directory this identity cannot search.
    pub fn exists(&self, p: impl AsRef<Path>) -> Result<bool> {
        let p = p.as_ref();
        Ok(self.backend.stat(p).map_err(Self::io(p))?.is_some())
    }

    /// `lstat`: a symlink reports `FileKind::Symlink`.
    pub fn stat(&self, p: impl AsRef<Path>) -> Result<Option<Stat>> {
        let p = p.as_ref();
        self.backend.stat(p).map_err(Self::io(p))
    }

    /// `stat` that follows symlinks: a link to a directory reports `Dir`.
    pub fn stat_follow(&self, p: impl AsRef<Path>) -> Result<Option<Stat>> {
        let p = p.as_ref();
        self.backend.stat_follow(p).map_err(Self::io(p))
    }

    /// The whole file, in memory, as bytes. A missing or unreadable file is
    /// an [`IoAt`] error naming the path: there is no "missing counts as
    /// empty" shortcut, so an op that tolerates absence asks
    /// [`System::exists`] first. Reading through a helper puts the file in
    /// one frame, which caps it at
    /// [`MAX_FRAME_PAYLOAD`](crate::protocol::MAX_FRAME_PAYLOAD).
    pub fn read(&self, p: impl AsRef<Path>) -> Result<Vec<u8>> {
        let p = p.as_ref();
        self.backend.read(p).map_err(Self::io(p))
    }

    /// The whole file as text. Errors with `not utf-8` on bytes that are
    /// not, rather than substituting replacement characters, so an op never
    /// rewrites a config file it silently mangled. Use [`System::read`] for
    /// anything that may be binary.
    pub fn read_to_string(&self, p: impl AsRef<Path>) -> Result<String> {
        let bytes = self.read(p)?;
        String::from_utf8(bytes).map_err(|e| Error::msg(format!("not utf-8: {e}")))
    }

    /// Where the symbolic link at `p` points. Errors if `p` is not a symlink.
    pub fn read_link(&self, p: impl AsRef<Path>) -> Result<PathBuf> {
        let p = p.as_ref();
        self.backend.read_link(p).map_err(Self::io(p))
    }

    /// Full paths of the direct children of the directory `p`.
    pub fn read_dir(&self, p: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let p = p.as_ref();
        self.backend.read_dir(p).map_err(Self::io(p))
    }

    // ---- mutations: guarded and logged ----

    /// Replace the contents of `p` through a temporary file in the same
    /// directory and a rename, so a concurrent reader sees either the old
    /// file or the new one and a failure part way leaves the old one intact.
    ///
    /// An existing file keeps its mode and owner; a new one is created with
    /// the mode any newly created file gets, 0666 minus the umask. Refused
    /// with [`MutationDuringCheck`] inside `check`, and errors as [`IoAt`]
    /// if the directory is not writable or the rename fails. Logs the path
    /// and byte count at debug level.
    pub fn write_atomic(&self, p: impl AsRef<Path>, bytes: &[u8]) -> Result<()> {
        let p = p.as_ref();
        self.guard_mutation(p)?;
        self.backend.write(p, bytes).map_err(Self::io(p))?;
        self.debug(format!("wrote {} ({} bytes)", p.display(), bytes.len()));
        Ok(())
    }

    /// Create `p` and every missing parent. Succeeds when `p` is already a
    /// directory, and fails when it exists as something else. New
    /// directories get the default mode; set the ones that matter afterwards
    /// with [`System::set_mode`], which is a separate step so an op can
    /// report it as a separate change. Refused inside `check`.
    pub fn mkdir_all(&self, p: impl AsRef<Path>) -> Result<()> {
        let p = p.as_ref();
        self.guard_mutation(p)?;
        self.backend.mkdir_all(p).map_err(Self::io(p))
    }

    /// Remove one entry: a file, a symlink, or an empty directory. A
    /// populated directory is an error; use `remove_all` for trees.
    pub fn remove(&self, p: impl AsRef<Path>) -> Result<()> {
        let p = p.as_ref();
        self.guard_mutation(p)?;
        self.backend.remove(p).map_err(Self::io(p))
    }

    /// Remove a directory tree. The only recursive delete.
    pub fn remove_all(&self, p: impl AsRef<Path>) -> Result<()> {
        let p = p.as_ref();
        self.guard_mutation(p)?;
        self.backend.remove_all(p).map_err(Self::io(p))?;
        self.debug(format!("removed tree {}", p.display()));
        Ok(())
    }

    /// Atomically move `from` to `to`, replacing `to` if it exists.
    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
        let (from, to) = (from.as_ref(), to.as_ref());
        self.guard_mutation(to)?;
        self.backend.rename(from, to).map_err(Self::io(to))
    }

    /// Set the permission bits of `p` to `mode`, written as an octal
    /// literal such as `0o644`. The value replaces the current bits instead
    /// of merging with them, and setuid, setgid and sticky live in the same
    /// number. Symlinks are followed, so this changes the target's mode.
    /// Refused inside `check`; fails when this identity owns neither the
    /// file nor root.
    pub fn set_mode(&self, p: impl AsRef<Path>, mode: u32) -> Result<()> {
        let p = p.as_ref();
        self.guard_mutation(p)?;
        self.backend.set_mode(p, mode).map_err(Self::io(p))
    }

    /// Set the owner and group of `p`. Both are numeric ids, not names: an
    /// op resolves a name first, usually from what the user or group op
    /// returned. Symlinks are followed. Refused inside `check`, and fails
    /// with `EPERM` unless this identity is root, since Linux lets nobody
    /// else give a file away.
    pub fn set_owner(&self, p: impl AsRef<Path>, uid: u32, gid: u32) -> Result<()> {
        let p = p.as_ref();
        self.guard_mutation(p)?;
        self.backend.set_owner(p, uid, gid).map_err(Self::io(p))
    }

    /// Create the symbolic link `link` pointing at `target`. Fails if `link`
    /// exists; remove it first to replace it.
    pub fn symlink(&self, target: impl AsRef<Path>, link: impl AsRef<Path>) -> Result<()> {
        let (target, link) = (target.as_ref(), link.as_ref());
        self.guard_mutation(link)?;
        self.backend.symlink(target, link).map_err(Self::io(link))?;
        self.debug(format!("linked {} -> {}", link.display(), target.display()));
        Ok(())
    }

    /// Copy `p` to `p.~rustible.<unix-ts>` and return that path.
    pub fn backup(&self, p: impl AsRef<Path>) -> Result<PathBuf> {
        let p = p.as_ref();
        self.guard_mutation(p)?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut name = p.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".~rustible.{ts}"));
        let dest = p.with_file_name(name);
        self.backend.copy(p, &dest).map_err(Self::io(p))?;
        Ok(dest)
    }

    // ---- processes ----

    /// Start building a command to run on the target as this handle's
    /// identity. Nothing runs until [`Cmd::run`] or [`Cmd::ok`], so this
    /// itself is safe to call from `check`; the [`Phase`] guard covers the
    /// filesystem primitives, not commands, and keeping `check` read-only
    /// is the op's job from here on.
    ///
    /// On a real system a switched identity is already the helper process's
    /// own user, so the argv is exactly what you build. On a `Fake` system
    /// there is no helper, so a switched identity gets a
    /// `sudo -n -u <user>` prefix instead, which is what a test asserts on.
    pub fn cmd(&self, program: impl Into<String>) -> Cmd {
        Cmd {
            sys: self.clone(),
            spec: CmdSpec {
                program: program.into(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
                stdin: None,
                // With a helper the process already is the user; without one
                // (a `Fake`), the prefix stands in for it.
                prefix: match (&self.identity, &self.escalation) {
                    (Identity::User(u), None) => {
                        vec!["sudo".into(), "-n".into(), "-u".into(), u.clone()]
                    }
                    _ => vec![],
                },
            },
            allow_failure: false,
        }
    }

    // ---- reporting ----

    /// Put a warning in the run's event stream. It is shown at every
    /// verbosity, prefixed `WARNING:`, and does not affect the step's
    /// status: this is for something the operator should know that is not
    /// worth failing over.
    pub fn warn(&self, msg: impl Into<String>) {
        self.sink.emit(Event::Log {
            level: Level::Warn,
            msg: msg.into(),
        });
    }

    /// Put a line in the run's event stream that only `-v` and above
    /// renders. The mutating primitives here use it to record what they
    /// touched, so an op rarely has to narrate its own writes.
    pub fn debug(&self, msg: impl Into<String>) {
        self.sink.emit(Event::Log {
            level: Level::Debug,
            msg: msg.into(),
        });
    }

    pub(crate) fn sink(&self) -> &SharedSink {
        &self.sink
    }
}

/// Builder returned by `sys.cmd()`.
pub struct Cmd {
    sys: System,
    spec: CmdSpec,
    allow_failure: bool,
}

impl Cmd {
    /// Append one argument. It reaches the program exactly as written:
    /// there is no shell in the way, so quoting, `*` globbing, `|` and `>`
    /// are all just characters in an argument.
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.spec.args.push(a.into());
        self
    }

    /// Append several arguments in order. The same as calling
    /// [`Cmd::arg`] once per item.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.spec.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set one environment variable on top of the environment the child
    /// would inherit. Setting the same key twice keeps the last value.
    /// `LANG` and `LC_ALL` are already forced to `C` for every command, so
    /// an op can parse a tool's output without a locale changing the words
    /// under it; overriding them here is how you get the locale back.
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.spec.env.insert(k.into(), v.into());
        self
    }

    /// Run the child in this directory. The default is to inherit the
    /// current directory of whichever process spawns it, which is the
    /// playbook binary or, for a switched identity, its helper. A directory
    /// that does not exist fails the spawn, not the command.
    pub fn cwd(mut self, p: impl Into<PathBuf>) -> Self {
        self.spec.cwd = Some(p.into());
        self
    }

    /// Feed these bytes to the child's stdin, then close it. Without this
    /// the child gets `/dev/null`, never the operator's terminal, so a
    /// program that would prompt reads EOF instead of hanging the run. The
    /// bytes are written from a separate thread while stdout and stderr are
    /// drained, so a child that talks back before reading its input does not
    /// deadlock.
    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.spec.stdin = Some(bytes.into());
        self
    }

    /// A non-zero exit is returned as `Output` instead of an error.
    pub fn allow_failure(mut self) -> Self {
        self.allow_failure = true;
        self
    }

    /// Run. Errors on non-zero exit unless `allow_failure`.
    pub fn run(self) -> Result<Output> {
        let t0 = Instant::now();
        let out = self
            .sys
            .backend
            .spawn(&self.spec)
            .map_err(|source| SpawnFailed {
                program: self.spec.program.clone(),
                source,
            })?;
        self.sys.sink.emit(Event::CmdRan {
            identity: self.sys.identity.label(),
            argv: self.spec.argv(),
            status: out.status,
            elapsed_ms: t0.elapsed().as_millis() as u64,
        });
        if !out.success() && !self.allow_failure {
            return Err(CmdFailed {
                argv: self.spec.argv(),
                status: out.status,
                stderr: out.stderr_str(),
            }
            .into());
        }
        Ok(out)
    }

    /// Run; `None` on non-zero exit. For "does this succeed" probes.
    pub fn ok(self) -> Result<Option<Output>> {
        let out = self.allow_failure().run()?;
        Ok(out.success().then_some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Collect;

    #[test]
    fn planned_resources_are_shared_by_clones_and_visible_in_check_mode_only() {
        let fake = Arc::new(Fake::new());
        let sys = System::fake(fake, Arc::new(Collect::default())).with_check_mode(true);
        assert!(!sys.would_create("group", "docker"));
        sys.as_user("root")
            .note_would_create("group", "docker", None);
        assert!(sys.would_create("group", "docker"), "clones share the list");
        assert!(!sys.would_create("user", "docker"), "kind matters");
        assert!(sys.would_create_id("group", 998).is_none());
        sys.note_would_create("group", "fixed", Some(998));
        assert_eq!(sys.would_create_id("group", 998).unwrap().name, "fixed");

        let real = sys.with_check_mode(false);
        assert!(!real.would_create("group", "docker"));
        assert!(real.would_create_id("group", 998).is_none());
    }

    #[test]
    fn symlink_is_refused_during_check() {
        let fake = Arc::new(Fake::new());
        let sys = System::fake(fake.clone(), Arc::new(Collect::default()));
        sys.set_phase(Phase::Checking);
        let err = sys.symlink("/target", "/link").unwrap_err().to_string();
        assert!(err.contains("during check()"), "{err}");
        assert!(fake.file("/link").is_none());

        sys.set_phase(Phase::Applying);
        sys.symlink("/target", "/link").unwrap();
        assert_eq!(sys.read_link("/link").unwrap(), PathBuf::from("/target"));
        assert!(sys.read_dir("/").is_err(), "no such dir in the fake");
    }
}
