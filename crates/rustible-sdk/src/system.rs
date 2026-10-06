//! The op's handle to the machine. Concrete struct over a swappable backend.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::backend::{Backend, CmdSpec, Elevated, Fake, HelperGone, Local, Output, Spawner, Stat};
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

/// A command that could not be spawned, or a dead helper's report printed
/// once, as in `System::io`.
fn spawn_failed(program: &str, source: std::io::Error) -> Error {
    match HelperGone::inside(&source) {
        Some(gone) => Error::msg(format!("could not spawn `{program}`: {gone}")),
        None => SpawnFailed {
            program: program.to_string(),
            source,
        }
        .into(),
    }
}

/// The op's handle to one machine, at one identity.
///
/// Everything an op is allowed to do to the world goes through this: the
/// reads, the mutations (guarded by [`Phase`] so `check` cannot cheat),
/// [`System::cmd`] and the [`Facts`] gathered at startup. The runtime builds
/// one per host and hands it to every op; a playbook never needs to build one
/// outside tests.
///
/// Cloning is cheap and deliberate. A clone shares the backend, the facts
/// and the phase with the original, so the handles produced by
/// [`System::as_user`] and by nested sections all speak for one run. What a
/// clone can differ in is its identity, its backend, and its check-mode
/// flag.
///
/// Use [`System::local`] for a real machine and [`System::fake`] for tests.
#[derive(Clone)]
pub struct System {
    backend: Arc<dyn Backend>,
    facts: Arc<Facts>,
    identity: Identity,
    check_mode: bool,
    phase: Arc<AtomicU8>,
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
                note: None,
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
                    note: esc.spawner.note.clone(),
                },
                helpers: Mutex::new(BTreeMap::new()),
            }));
        }
        self
    }

    /// The sentence [`System::with_escalation_note`] set, for the runtime's
    /// tests.
    #[cfg(test)]
    pub(crate) fn escalation_note(&self) -> Option<&str> {
        self.escalation.as_ref()?.spawner.note.as_deref()
    }

    /// A sentence appended to every escalation failure (a helper that
    /// died, which is how a refused `sudo` shows). The runtime sets
    /// [`LoginOverride::note`](crate::ctx::LoginOverride::note) here when the
    /// playbook's `ssh_user` chose the account escalating. No effect on a
    /// `Fake` system.
    pub fn with_escalation_note(mut self, note: Option<String>) -> Self {
        if let Some(esc) = &self.escalation {
            let mut spawner = esc.spawner.clone();
            spawner.note = note;
            self.escalation = Some(Arc::new(Escalation {
                own_user: esc.own_user.clone(),
                local: esc.local.clone(),
                spawner,
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
            package_managers: [crate::facts::Pm::Apt].into_iter().collect(),
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
    /// that an op defers a refusal about a prerequisite another step could
    /// create (vision doc 12). As with [`System::with_facts`], only handles
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

    /// True during a dry run. The one reason an op looks is a prerequisite
    /// another step in the run could create — a group, a home, a unit —
    /// which `check` refuses in a real run and reports as `would change`
    /// here; a dry run's plan never reaches `apply`, so nothing acts on the
    /// tolerance (vision doc 6.7, 12).
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
            // A dead helper's report is already whole; as an `IoAt` source it
            // would print twice (see `HelperGone`).
            if let Some(gone) = HelperGone::inside(&source) {
                return Error::msg(format!("{}: {gone}", p.display()));
            }
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
    /// [`System::exists`] first. Through a helper the file crosses in chunks
    /// of 1 MiB, so any size works; it is still whole in memory here.
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
    /// An existing file keeps its mode, setuid and setgid included, and its
    /// owner and group, or the write fails and the old file stays. Keeping
    /// the mode needs no privilege: an unprivileged rewrite of this
    /// identity's own setuid file stays setuid. Keeping an owner and group
    /// other than this identity's takes root (`CAP_CHOWN`), and without it
    /// the change of owner is not an error: an unprivileged rewrite of
    /// another user's file, or of one whose group this identity is not in,
    /// leaves it with this identity's user or group and the same mode, so
    /// another user's `4755` file becomes this identity's `4755` file. An
    /// op that needs the owner checks it afterwards. `p` is followed if it
    /// is a symlink for the mode and owner to keep, and the link itself is
    /// replaced by the new file. A new file is created with the mode any
    /// newly created file gets, 0666 minus the umask.
    ///
    /// Refused with [`MutationDuringCheck`] inside `check`. Errors as
    /// [`IoAt`], leaving the old file as it was, if the directory is not
    /// writable or the rename fails, or if the mode cannot be kept: a root
    /// without `CAP_FOWNER` cannot set setuid or setgid again on another
    /// user's file after giving it back, and setgid on a file whose group
    /// this identity is not in takes `CAP_FSETID` (the kernel drops the bit
    /// without an error, so the write reads the mode back). Logs the path
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

    /// Copy `p` to a new file beside it, `<name>.~rustible.<unix-ts>`, and
    /// return that path.
    ///
    /// The backup is always a new file: anything already at the name, a
    /// symlink included, is left untouched and another name is tried,
    /// `<name>.~rustible.<unix-ts>.<8 random hex digits>`, up to ten names in
    /// all; when every one is taken the backup fails naming them, and nothing
    /// is written. The suffixes are random so that nobody can plant every
    /// name in advance and stop the backup. The copy has `p`'s permission
    /// bits without setuid, setgid and sticky, and `p`'s owner and group
    /// where the runner may give them ([`Backend::copy`] has the rules, and
    /// why).
    pub fn backup(&self, p: impl AsRef<Path>) -> Result<PathBuf> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.backup_at(p.as_ref(), ts, random_suffix)
    }

    /// How many names [`backup`](Self::backup) tries before it gives up.
    const BACKUP_NAMES: u32 = 10;

    /// [`backup`](Self::backup) with the clock read and the suffix of each
    /// retry given, so a test can plant what sits at each name.
    fn backup_at(&self, p: &Path, ts: u64, mut suffix: impl FnMut() -> String) -> Result<PathBuf> {
        self.guard_mutation(p)?;
        let mut first = p.file_name().unwrap_or_default().to_os_string();
        first.push(format!(".~rustible.{ts}"));
        let mut tried = Vec::new();
        for n in 0..Self::BACKUP_NAMES {
            let mut name = first.clone();
            if n > 0 {
                name.push(format!(".{}", suffix()));
            }
            let dest = p.with_file_name(name);
            match self.backend.copy(p, &dest) {
                Ok(()) => return Ok(dest),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => tried.push(dest),
                Err(e) => {
                    return Err(Self::io(p)(e).context(format!(
                        "could not back up {} to {}",
                        p.display(),
                        dest.display()
                    )));
                }
            }
        }
        let tried: Vec<String> = tried.iter().map(|d| d.display().to_string()).collect();
        Err(Error::msg(format!(
            "could not back up {}: all {} names tried exist already ({}). \
             A backup is never written through an existing path or symlink; \
             look at what is there, and move it away if it is stale",
            p.display(),
            Self::BACKUP_NAMES,
            tried.join(", "),
        )))
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
    ///
    /// Counted in [`Summary::warnings`](crate::event::Summary::warnings)
    /// exactly as [`Ctx::warn`](crate::ctx::Ctx::warn) is, because the count
    /// is taken at the sink every event passes rather than by the caller.
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
            .map_err(|source| spawn_failed(&self.spec.program, source))?;
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
                signal: out.signal,
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

/// Eight hex digits nobody can predict, for a backup's retry names. Each
/// `RandomState` is keyed from the OS's randomness (per thread, then stepped
/// for every new one), so hashing nothing with a fresh one is enough and needs
/// no dependency.
fn random_suffix() -> String {
    use std::hash::BuildHasher;
    let h = std::collections::hash_map::RandomState::new().hash_one(());
    format!("{:08x}", h as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::FileKind;
    use crate::event::Collect;

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

    /// The runtime calls `with_escalation` and then `with_escalation_note`;
    /// each rebuilds the escalation, so neither may drop what the other set.
    #[test]
    fn the_escalation_note_and_method_survive_each_other() {
        let spawner = |s: &System| s.escalation.as_ref().unwrap().spawner.clone();
        let sys = System::local(false, Arc::new(Collect::default()))
            .with_escalation("doas", None)
            .with_escalation_note(Some("the login user `x`".into()));
        assert_eq!(spawner(&sys).method, "doas");
        assert_eq!(spawner(&sys).note.as_deref(), Some("the login user `x`"));
        let sys = sys.with_escalation("sudo", None);
        assert_eq!(spawner(&sys).note.as_deref(), Some("the login user `x`"));
    }

    // ---- backup (issue #75) ----

    fn fake_sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default()))
    }

    /// Retry suffixes `1`, `2`, ... in place of random ones, so a test can
    /// plant every name; `calls` counts how many were asked for.
    fn counting(calls: &std::cell::Cell<u32>) -> impl FnMut() -> String + '_ {
        move || {
            calls.set(calls.get() + 1);
            calls.get().to_string()
        }
    }

    /// A symlink at the backup's name, dangling or not, is never written
    /// through: the backup takes the next free name, and the links and the
    /// file one points at stay exactly as they were.
    #[test]
    fn a_backup_skips_a_symlink_planted_at_its_name() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/x", "old\n")
                .with_file_mode("/etc/victim", "victim\n", 0o600)
                .with_symlink("/etc/x.~rustible.100", "/etc/victim")
                .with_symlink("/etc/x.~rustible.100.1", "/etc/nowhere"),
        );
        let dest = fake_sys(&fake)
            .backup_at(Path::new("/etc/x"), 100, counting(&Default::default()))
            .unwrap();
        assert_eq!(dest, PathBuf::from("/etc/x.~rustible.100.2"));
        assert_eq!(fake.content(&dest).unwrap(), "old\n");
        let victim = fake.file("/etc/victim").unwrap();
        assert_eq!(
            (victim.bytes.as_slice(), victim.mode),
            (&b"victim\n"[..], 0o600)
        );
        for (link, target) in [
            ("/etc/x.~rustible.100", "/etc/victim"),
            ("/etc/x.~rustible.100.1", "/etc/nowhere"),
        ] {
            let l = fake.file(link).unwrap();
            assert_eq!(
                (l.kind, l.bytes.as_slice()),
                (FileKind::Symlink, target.as_bytes())
            );
        }
        assert!(
            fake.file("/etc/nowhere").is_none(),
            "a dangling link is not followed either"
        );
    }

    /// A regular file at the backup's name is someone's, perhaps an earlier
    /// backup in the same second: left alone, and the next name taken.
    #[test]
    fn a_backup_skips_a_file_already_at_its_name() {
        let fake = Arc::new(Fake::new().with_file("/etc/x", "new\n").with_file_mode(
            "/etc/x.~rustible.7",
            "earlier\n",
            0o640,
        ));
        let dest = fake_sys(&fake)
            .backup_at(Path::new("/etc/x"), 7, counting(&Default::default()))
            .unwrap();
        assert_eq!(dest, PathBuf::from("/etc/x.~rustible.7.1"));
        assert_eq!(fake.content(&dest).unwrap(), "new\n");
        let earlier = fake.file("/etc/x.~rustible.7").unwrap();
        assert_eq!(
            (earlier.bytes.as_slice(), earlier.mode),
            (&b"earlier\n"[..], 0o640)
        );
    }

    /// When every name is taken the backup fails, names every name it
    /// tried, and writes nothing anywhere.
    #[test]
    fn a_backup_with_every_name_taken_fails_and_writes_nothing() {
        let names: Vec<String> = std::iter::once("/etc/x.~rustible.5".to_string())
            .chain((1..System::BACKUP_NAMES).map(|n| format!("/etc/x.~rustible.5.{n}")))
            .collect();
        assert_eq!(names.len(), 10);
        let mut fake = Fake::new()
            .with_file("/etc/x", "old\n")
            .with_file("/etc/victim", "victim\n");
        for name in &names {
            fake = fake.with_symlink(name, "/etc/victim");
        }
        let fake = Arc::new(fake);
        let err = fake_sys(&fake)
            .backup_at(Path::new("/etc/x"), 5, counting(&Default::default()))
            .unwrap_err()
            .chain();
        assert!(
            err.contains(&format!(
                "could not back up /etc/x: all 10 names tried exist already ({})",
                names.join(", ")
            )),
            "{err}"
        );
        for name in &names {
            assert_eq!(fake.file(name).unwrap().kind, FileKind::Symlink, "{name}");
        }
        assert!(fake.file("/etc/x.~rustible.5.10").is_none());
        assert_eq!(fake.content("/etc/victim").unwrap(), "victim\n");
    }

    /// Setuid, setgid and sticky never reach the backup; the rest of the
    /// mode does. Root backing up a setuid file made a root-owned setuid
    /// copy of content its owner controlled.
    #[test]
    fn a_backup_carries_no_setuid_setgid_or_sticky() {
        for (mode, want) in [
            (0o4755, 0o755),
            (0o2755, 0o755),
            (0o6755, 0o755),
            (0o2745, 0o745),
            (0o1755, 0o755),
            (0o7777, 0o777),
            (0o640, 0o640),
        ] {
            let fake = Arc::new(Fake::new().with_file_mode("/usr/bin/x", "elf", mode));
            let dest = fake_sys(&fake).backup("/usr/bin/x").unwrap();
            let b = fake.file(&dest).unwrap();
            assert_eq!(
                (b.mode, b.uid, b.gid, b.kind, b.bytes.as_slice()),
                (want, 0, 0, FileKind::File, &b"elf"[..]),
                "{mode:o}"
            );
            assert_eq!(
                fake.file("/usr/bin/x").unwrap().mode,
                mode,
                "the source keeps its mode"
            );
        }
    }

    /// The same rules through `Local` on the real filesystem: a symlink and
    /// a file at the first two names are skipped and left as they were, and
    /// the backup is a new regular file with the mode minus setuid. Linux
    /// only, for the setuid half: nobody has measured an unprivileged setuid
    /// `chmod` on a mac here.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_local_backup_skips_planted_names_and_drops_setuid() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str| dir.path().join(name);
        let (p, victim) = (at("x"), at("victim"));
        std::fs::write(&p, "old\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o4755)).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            mode(&p),
            0o4755,
            "planted as asked, or the test proves nothing"
        );
        std::fs::write(&victim, "victim\n").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&victim, at("x.~rustible.42")).unwrap();
        std::fs::write(at("x.~rustible.42.1"), "earlier\n").unwrap();

        let sys = System::local(false, Arc::new(Collect::default()));
        let dest = sys
            .backup_at(&p, 42, counting(&Default::default()))
            .unwrap();
        assert_eq!(dest, at("x.~rustible.42.2"));
        assert!(
            std::fs::symlink_metadata(&dest)
                .unwrap()
                .file_type()
                .is_file()
        );
        assert_eq!(mode(&dest), 0o755);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old\n");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "victim\n");
        assert_eq!(mode(&victim), 0o600);
        let link = std::fs::symlink_metadata(at("x.~rustible.42")).unwrap();
        assert!(link.file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(at("x.~rustible.42.1")).unwrap(),
            "earlier\n"
        );
    }

    /// Root backing up another user's file gives the backup that user's
    /// owner and group: a root-owned copy of their content is something a
    /// tool that trusts root-owned files (logrotate) would act on as root.
    #[test]
    fn a_backup_keeps_the_owner_of_the_file() {
        let fake = Arc::new(Fake::new().with_file_mode("/home/x/app.conf", "cfg", 0o4644));
        let sys = fake_sys(&fake);
        sys.set_owner("/home/x/app.conf", 65534, 65534).unwrap();
        let dest = sys.backup("/home/x/app.conf").unwrap();
        let b = fake.file(&dest).unwrap();
        assert_eq!((b.uid, b.gid, b.mode), (65534, 65534, 0o644));
    }

    /// Only a name already taken moves the backup on to the next one. Any
    /// other failure ends it there, naming the backup it was making, rather
    /// than trying nine more and reporting that they all exist.
    #[test]
    fn a_backup_that_fails_otherwise_tries_one_name() {
        let fake = Arc::new(Fake::new());
        let calls = std::cell::Cell::new(0);
        let err = fake_sys(&fake)
            .backup_at(Path::new("/etc/missing"), 5, counting(&calls))
            .unwrap_err()
            .chain();
        assert!(
            err.contains("could not back up /etc/missing to /etc/missing.~rustible.5:")
                && !err.contains("exist already"),
            "{err}"
        );
        assert_eq!(calls.get(), 0, "a second name was tried");
    }

    /// A directory is not backed up, and nothing is created for it.
    #[test]
    fn a_backup_of_a_directory_is_refused() {
        let fake = Arc::new(Fake::new().with_dir("/etc/d"));
        let calls = std::cell::Cell::new(0);
        let err = fake_sys(&fake)
            .backup_at(Path::new("/etc/d"), 5, counting(&calls))
            .unwrap_err()
            .chain();
        assert!(err.contains("not a regular file"), "{err}");
        assert_eq!(calls.get(), 0);
        assert!(fake.file("/etc/d.~rustible.5").is_none());
    }

    /// The retry names are random, so planting every name a counter would
    /// produce does not stop a backup.
    #[test]
    fn predictable_names_do_not_block_a_backup() {
        let mut fake = Fake::new().with_file("/etc/x", "old\n");
        fake = fake.with_file("/etc/x.~rustible.5", "planted");
        for n in 0..100 {
            fake = fake.with_file(format!("/etc/x.~rustible.5.{n}"), "planted");
        }
        let fake = Arc::new(fake);
        let dest = fake_sys(&fake)
            .backup_at(Path::new("/etc/x"), 5, random_suffix)
            .unwrap();
        let name = dest.to_string_lossy().into_owned();
        let suffix = name.strip_prefix("/etc/x.~rustible.5.").expect(&name);
        assert!(
            suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "{name}"
        );
        assert_eq!(fake.content(&dest).unwrap(), "old\n");
        let (a, b) = (random_suffix(), random_suffix());
        assert_ne!(a, b, "two suffixes in a row");
    }
}
