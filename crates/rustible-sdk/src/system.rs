//! The op's handle to the machine. Concrete struct over a swappable backend.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::backend::{Backend, CmdSpec, Fake, Local, Output, Stat};
use crate::error::{CmdFailed, Error, IoAt, MutationDuringCheck, Result, SpawnFailed};
use crate::event::{Event, Level, SharedSink};
use crate::facts::Facts;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Idle = 0,
    Checking = 1,
    Applying = 2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// Whatever the process runs as.
    Own,
    /// Run commands via sudo as this user. File ops still happen as the
    /// process user in this spike; the `Elevated` helper backend is future work.
    User(String),
}

impl Identity {
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
    pub name: String,
    /// A numeric id the creating step knows for sure (a requested gid).
    pub id: Option<u32>,
}

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
}

impl System {
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
        }
    }

    /// Real machine, real facts.
    pub fn local(check_mode: bool, sink: SharedSink) -> Self {
        let backend = Arc::new(Local);
        let facts = Facts::gather(&*backend);
        Self::new(backend, facts, check_mode, sink)
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

    pub fn with_facts(mut self, facts: Facts) -> Self {
        self.facts = Arc::new(facts);
        self
    }

    pub fn with_check_mode(mut self, on: bool) -> Self {
        self.check_mode = on;
        self
    }

    // ---- context ----

    pub fn facts(&self) -> &Facts {
        &self.facts
    }

    pub fn check_mode(&self) -> bool {
        self.check_mode
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn is_root(&self) -> bool {
        match &self.identity {
            Identity::Own => self.facts.is_root,
            Identity::User(u) => u == "root",
        }
    }

    /// A clone whose commands run as another user. Not a mutation.
    pub fn as_user(&self, name: &str) -> System {
        let mut s = self.clone();
        s.identity = Identity::User(name.to_string());
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

    pub fn read(&self, p: impl AsRef<Path>) -> Result<Vec<u8>> {
        let p = p.as_ref();
        self.backend.read(p).map_err(Self::io(p))
    }

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

    pub fn write_atomic(&self, p: impl AsRef<Path>, bytes: &[u8]) -> Result<()> {
        let p = p.as_ref();
        self.guard_mutation(p)?;
        self.backend.write(p, bytes).map_err(Self::io(p))?;
        self.debug(format!("wrote {} ({} bytes)", p.display(), bytes.len()));
        Ok(())
    }

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

    pub fn set_mode(&self, p: impl AsRef<Path>, mode: u32) -> Result<()> {
        let p = p.as_ref();
        self.guard_mutation(p)?;
        self.backend.set_mode(p, mode).map_err(Self::io(p))
    }

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

    pub fn cmd(&self, program: impl Into<String>) -> Cmd {
        Cmd {
            sys: self.clone(),
            spec: CmdSpec {
                program: program.into(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
                stdin: None,
                prefix: match &self.identity {
                    Identity::Own => vec![],
                    Identity::User(u) => vec!["sudo".into(), "-n".into(), "-u".into(), u.clone()],
                },
            },
            allow_failure: false,
        }
    }

    // ---- reporting ----

    pub fn warn(&self, msg: impl Into<String>) {
        self.sink.emit(Event::Log {
            level: Level::Warn,
            msg: msg.into(),
        });
    }

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
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.spec.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.spec.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.spec.env.insert(k.into(), v.into());
        self
    }

    pub fn cwd(mut self, p: impl Into<PathBuf>) -> Self {
        self.spec.cwd = Some(p.into());
        self
    }

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
