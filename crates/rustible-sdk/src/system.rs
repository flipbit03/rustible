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
    /// Another user. On a real system every primitive goes through an
    /// `Elevated` helper running as that user (vision doc 11.3); on a `Fake`
    /// system commands get a `sudo -n -u <user>` prefix so tests can assert it.
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

    pub fn stat(&self, p: impl AsRef<Path>) -> Result<Option<Stat>> {
        let p = p.as_ref();
        self.backend.stat(p).map_err(Self::io(p))
    }

    pub fn read(&self, p: impl AsRef<Path>) -> Result<Vec<u8>> {
        let p = p.as_ref();
        self.backend.read(p).map_err(Self::io(p))
    }

    pub fn read_to_string(&self, p: impl AsRef<Path>) -> Result<String> {
        let bytes = self.read(p)?;
        String::from_utf8(bytes).map_err(|e| Error::msg(format!("not utf-8: {e}")))
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

    pub fn remove(&self, p: impl AsRef<Path>) -> Result<()> {
        let p = p.as_ref();
        self.guard_mutation(p)?;
        self.backend.remove(p).map_err(Self::io(p))
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
