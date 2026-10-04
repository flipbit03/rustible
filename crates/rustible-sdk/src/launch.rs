//! Starting the playbook binary as an account that cannot read the copy
//! already on the target (vision 11.3, 5.2 step 8; issue #62).
//!
//! The orchestrator uploads the binary into the **login user's**
//! `~/.cache/rustible/bin/`, and any other unprivileged account usually
//! cannot reach it: homes are 0750 or 0700 by default on Ubuntu and Debian,
//! and `~/.cache` is 0700 on macOS. So for a target that is neither root nor
//! the login user, the binary is streamed to the target instead: `/bin/sh`,
//! run as that account behind the escalation prefix, writes the bytes it
//! reads on stdin into the account's own `~/.cache/rustible/bin/<name>`, and
//! the binary is exec'd from there. An account with no usable home gets a
//! private per-run directory, `${TMPDIR:-/tmp}/rustible-<random>`, made with
//! `mkdir -m 700`, and the copy there removes itself.
//!
//! Two callers start a binary that way: [`Spawner`](crate::backend::Spawner)
//! for an `as_user` helper (`--helper`), and the orchestrator in
//! `rustible-cli` for a playbook whose `escalate_user` is unprivileged
//! (`--remote`). One spawns local processes synchronously, the other remote
//! ones over an ssh multiplex, so this module does no I/O at all: it holds
//! the scripts, the command lines, the decisions between spawns and the
//! messages, and each caller drives it with its own processes. It is public
//! for that second caller, not for playbooks.
//!
//! ## The spawns
//!
//! Every script is a constant; whatever varies (the binary's name, its
//! size, the temp directory's suffix) is a positional argument, so no
//! account or playbook name is ever interpolated into shell text. Each is
//! one line, and every path operand follows `--` (a `TMPDIR` of `-p` is a
//! path, not a flag): over ssh the command passes through the login's own
//! shell first, and csh refuses a newline inside quotes. Each
//! script writes **one byte to stdout before anything else**, and the caller
//! waits for it before writing a frame or a byte of the binary: until then
//! the escalation tool may still be reading a password from the same pipe.
//!
//! - [`TRY`] answers `R` and execs the copy when there is one, or answers
//!   `N` and exits. On a warm run it is the only spawn.
//! - [`INSTALL`] answers `I`, then reads the binary from stdin into a temp
//!   name, checks its size with `wc -c`, and renames it into place.
//! - [`EXEC`] answers `R` and execs its arguments. It is how a root helper
//!   gets a ready byte when a password is fed to `sudo -S` (root is not
//!   streamed: it can read the login user's copy).
//!
//! [`Launch`] says which to run next: try the home, install there, try
//! again, then install into the temp directory and try that, and refuse
//! only when both places failed, naming both causes.

use std::time::Duration;

/// Exits with the binary running (`R`) or answers `N` and exits 0.
///
/// `$1` is the binary's name, `$2` the mode flag it is exec'd with
/// (`--helper` or `--remote`), and `$3`, when set, the suffix of the per-run
/// temp directory, in which case the copy is exec'd with
/// [`EPHEMERAL_FLAG`] so it removes itself.
pub const TRY: &str = concat!(
    r#"if [ -n "$3" ]; then p="${TMPDIR:-/tmp}/rustible-$3/$1"; "#,
    r#"else case $HOME in /*) ;; *) printf N; exit 0 ;; esac; "#,
    r#"p="$HOME/.cache/rustible/bin/$1"; fi; "#,
    r#"case $p in /*) ;; *) p="./$p" ;; esac; "#,
    r#"if [ -f "$p" ] && [ -x "$p" ]; then printf R; exec "$p" "$2" ${3:+--ephemeral}; fi; "#,
    "printf N",
);

/// Writes the binary read on stdin into the account's cache, or into the
/// per-run temp directory when `$3` (its suffix) is set. `$1` is the name,
/// `$2` the expected size in bytes.
///
/// Exit 0 is installed. [`UNUSABLE`] and [`NOEXEC`] mean this place cannot
/// hold a runnable copy, which sends a home on to the temp directory; any
/// other status is a refusal. Each failure ends with one `rustible: <cause>`
/// line on stderr, which [`Launch`] lifts into its message.
pub const INSTALL: &str = concat!(
    "printf I; umask 077; ",
    r#"if [ -n "$3" ]; then d="${TMPDIR:-/tmp}/rustible-$3"; "#,
    r#"e=$(mkdir -m 700 -- "$d" 2>&1) || { echo "rustible: cannot create $d: ${e##*: }" >&2; exit 10; }; "#,
    r#"else case $HOME in /*) ;; *) echo "rustible: no usable home (HOME is \"$HOME\", not an absolute path)" >&2; exit 10 ;; esac; "#,
    r#"[ -d "$HOME" ] || { echo "rustible: no usable home ($HOME does not exist)" >&2; exit 10; }; "#,
    r#"d="$HOME/.cache/rustible/bin"; "#,
    r#"e=$(mkdir -p -- "$d" 2>&1) || { echo "rustible: no usable home (cannot create $d: ${e##*: })" >&2; exit 10; }; fi; "#,
    r#"t="$d/.$1.$$.tmp"; trap 'rm -f -- "$t"; [ -z "$3" ] || rmdir -- "$d" 2>/dev/null' EXIT; "#,
    r#"cat > "$t" || { echo "rustible: cannot write to $d" >&2; exit 10; }; "#,
    r#"n=$(wc -c < "$t" | tr -d ' '); "#,
    r#"[ "$n" = "$2" ] || { echo "rustible: received $n of $2 bytes for $d/$1" >&2; exit 1; }; "#,
    r#"chmod -- 700 "$t" && mv -f -- "$t" "$d/$1" || exit 1; "#,
    r#"[ -x "$d/$1" ] || { rm -f -- "$d/$1"; echo "rustible: $d is on a noexec filesystem" >&2; exit 11; }; "#,
    "exit 0",
);

/// Answers `R`, then execs its arguments: `sh -c EXEC rustible <exe>
/// --helper`.
pub const EXEC: &str = r#"printf R; exec "$@""#;

/// [`INSTALL`]'s status for a place that cannot hold the copy: no home, a
/// home or temp directory it cannot create or write.
pub const UNUSABLE: i32 = 10;

/// [`INSTALL`]'s status for a copy that is not executable once written: a
/// `noexec` filesystem. The copy has been removed.
pub const NOEXEC: i32 = 11;

/// The argument after the mode flag that tells a binary started from the
/// per-run temp directory to remove its copy. A helper does it as it starts;
/// a `--remote` binary when it exits, because its own helpers are started
/// from that path.
pub const EPHEMERAL_FLAG: &str = "--ephemeral";

/// How long a spawn has to write its first byte. Past it the child is
/// killed and the launch refused: `sudo -S` that rejected a password waits
/// for another line on stdin, and so does a prompt nobody recognises.
pub const FIRST_BYTE_DEADLINE: Duration = Duration::from_secs(60);

/// What the exec'd binary is asked to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A per-identity helper (`--helper`), started by [`Spawner`](crate::backend::Spawner).
    Helper,
    /// The playbook itself (`--remote`), started by the orchestrator.
    Remote,
}

impl Mode {
    /// The flag the binary is exec'd with.
    pub fn flag(self) -> &'static str {
        match self {
            Mode::Helper => "--helper",
            Mode::Remote => "--remote",
        }
    }
}

/// Where a copy goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// `$HOME/.cache/rustible/bin/`, kept as a cache across runs.
    Home,
    /// `${TMPDIR:-/tmp}/rustible-<suffix>/`, private to this run and removed
    /// by the binary itself.
    Temp,
}

/// One spawn of a launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spawn {
    /// [`TRY`]: exec the copy at this place if there is one.
    Try(Place),
    /// [`INSTALL`]: stream the binary to this place.
    Install(Place),
}

/// What a [`TRY`] spawn answered with its first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// `R`: the binary is running; frames follow on the same pipes.
    Running,
    /// `N`: no runnable copy there; the shell has exited.
    Missing,
}

impl Answer {
    /// The answer a byte stands for, `None` for anything that is not one.
    pub fn from_byte(b: u8) -> Option<Answer> {
        match b {
            b'R' => Some(Answer::Running),
            b'N' => Some(Answer::Missing),
            _ => None,
        }
    }
}

/// The first byte [`INSTALL`] writes, once it is running as the account.
pub const INSTALL_READY: u8 = b'I';

/// What to do after a spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// The binary is running: the last spawn's pipes are its channel.
    Ready,
    /// Run this spawn next.
    Spawn(Spawn),
    /// Give up, with the message to report.
    Refuse(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    TryHome,
    InstallHome,
    RetryHome,
    InstallTemp,
    TryTemp,
    Done,
}

/// One launch of the binary as one account: the decisions between spawns,
/// with the I/O left to the caller.
///
/// ```text
/// try home ─R─▶ ready          (a warm run: one spawn)
///    └N─▶ install home ─0─▶ try home ─R─▶ ready
///            │                  └N─┐
///            └10/11 (unusable) ───▶ install temp ─0─▶ try temp ─R─▶ ready
///                                       └10/11 ─▶ refuse, naming both causes
/// ```
///
/// A binary whose name is not `<playbook>-<sha256>` (one run directly rather
/// than through the orchestrator) starts at the temp directory: the name is
/// what a cached copy is found by, so a name that does not change with the
/// contents would find a stale build.
#[derive(Debug, Clone)]
pub struct Launch {
    name: String,
    size: u64,
    mode: Mode,
    user: String,
    label: String,
    suffix: String,
    state: State,
    home_cause: Option<String>,
    installed: Option<Place>,
}

impl Launch {
    /// `name` is the binary's file name, `size` its length in bytes, `user`
    /// the account it is started as, and `label` how messages name the
    /// launch (`as_user(svc)`, ``escalating to `svc` ``).
    pub fn new(
        name: impl Into<String>,
        size: u64,
        mode: Mode,
        user: impl Into<String>,
        label: impl Into<String>,
    ) -> Launch {
        let name = name.into();
        let state = if is_content_addressed(&name) {
            State::TryHome
        } else {
            State::InstallTemp
        };
        Launch {
            name,
            size,
            mode,
            user: user.into(),
            label: label.into(),
            suffix: random_suffix(),
            state,
            home_cause: None,
            installed: None,
        }
    }

    /// The same launch with a chosen temp-directory suffix (tests).
    pub fn with_suffix(mut self, suffix: impl Into<String>) -> Launch {
        self.suffix = suffix.into();
        self
    }

    /// The suffix of this launch's temp directory, `rustible-<suffix>`.
    pub fn suffix(&self) -> &str {
        &self.suffix
    }

    /// The binary's file name in either place.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The spawn to run now.
    pub fn spawn(&self) -> Spawn {
        match self.state {
            State::TryHome | State::RetryHome | State::Done => Spawn::Try(Place::Home),
            State::InstallHome => Spawn::Install(Place::Home),
            State::InstallTemp => Spawn::Install(Place::Temp),
            State::TryTemp => Spawn::Try(Place::Temp),
        }
    }

    /// Where this launch installed a copy, if it had to.
    pub fn installed(&self) -> Option<Place> {
        self.installed
    }

    /// The command for `spawn`, to put after the escalation words:
    /// `/bin/sh -c <script> rustible <args>`.
    pub fn argv(&self, spawn: Spawn) -> Vec<String> {
        let mut argv = vec!["/bin/sh".to_string(), "-c".to_string()];
        match spawn {
            Spawn::Try(place) => {
                argv.extend([TRY.into(), "rustible".into(), self.name.clone()]);
                argv.push(self.mode.flag().into());
                if place == Place::Temp {
                    argv.push(self.suffix.clone());
                }
            }
            Spawn::Install(place) => {
                argv.extend([INSTALL.into(), "rustible".into(), self.name.clone()]);
                argv.push(self.size.to_string());
                if place == Place::Temp {
                    argv.push(self.suffix.clone());
                }
            }
        }
        argv
    }

    /// A [`TRY`] spawn answered.
    pub fn after_try(&mut self, answer: Answer) -> Next {
        if answer == Answer::Running {
            self.state = State::Done;
            return Next::Ready;
        }
        match self.state {
            State::TryHome => self.go(State::InstallHome),
            State::RetryHome => {
                self.home_cause = Some(format!(
                    "{} was installed in its home but is still not executable there",
                    self.name
                ));
                self.go(State::InstallTemp)
            }
            State::TryTemp => self.refuse_both(&format!(
                "{} was installed in the temp directory but is still not executable there",
                self.name
            )),
            _ => self.out_of_order("a try"),
        }
    }

    /// An [`INSTALL`] spawn exited with `status` (-1 for a signal), having
    /// written `stderr`.
    pub fn after_install(&mut self, status: i32, stderr: &str) -> Next {
        let cause = install_cause(stderr);
        match (self.state, status) {
            (State::InstallHome, 0) => {
                self.installed = Some(Place::Home);
                self.go(State::RetryHome)
            }
            (State::InstallHome, UNUSABLE | NOEXEC) => {
                self.home_cause = Some(cause.unwrap_or_else(|| "its home is unusable".into()));
                self.go(State::InstallTemp)
            }
            (State::InstallTemp, 0) => {
                self.installed = Some(Place::Temp);
                self.go(State::TryTemp)
            }
            (State::InstallTemp, UNUSABLE | NOEXEC) => {
                let cause = cause.unwrap_or_else(|| "the temp directory is unusable".into());
                self.refuse_both(&cause)
            }
            (State::InstallHome | State::InstallTemp, _) => {
                self.state = State::Done;
                Next::Refuse(match cause {
                    Some(cause) => format!("{}: {cause}", self.label),
                    None => format!(
                        "{}: installing its copy of the playbook binary exited {status}{}",
                        self.label,
                        tail(stderr)
                    ),
                })
            }
            _ => self.out_of_order("an install"),
        }
    }

    fn go(&mut self, state: State) -> Next {
        self.state = state;
        Next::Spawn(self.spawn())
    }

    fn refuse_both(&mut self, temp_cause: &str) -> Next {
        self.state = State::Done;
        let causes = match &self.home_cause {
            Some(home) => format!("{home} and {temp_cause}"),
            None => temp_cause.to_string(),
        };
        Next::Refuse(format!(
            "{}: {causes}, so {} cannot run its copy of the playbook binary",
            self.label, self.user
        ))
    }

    fn out_of_order(&mut self, what: &str) -> Next {
        self.state = State::Done;
        Next::Refuse(format!(
            "{}: {what} answered out of order (rustible bug)",
            self.label
        ))
    }
}

/// The last `rustible: <cause>` line [`INSTALL`] wrote.
fn install_cause(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("rustible: "))
        .map(str::to_string)
}

/// `: <last line>` of stderr, or nothing when it said nothing.
fn tail(stderr: &str) -> String {
    match stderr.lines().map(str::trim).rfind(|l| !l.is_empty()) {
        Some(l) => format!(": {l}"),
        None => String::new(),
    }
}

/// `<playbook>-<64 lowercase hex>`, the orchestrator's content-addressed
/// name for an upload.
pub fn is_content_addressed(name: &str) -> bool {
    match name.rsplit_once('-') {
        Some((stem, hash)) => {
            !stem.is_empty()
                && hash.len() == 64
                && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        }
        None => false,
    }
}

/// Sixteen hex digits nobody can predict: the temp directory's suffix.
/// Predicting it gains nothing but a refused run, since `mkdir` fails on a
/// name that exists, but a guessable one would make that refusal cheap.
pub fn random_suffix() -> String {
    use std::hash::BuildHasher;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // `RandomState` is keyed from the OS's randomness once per process.
    let n = std::collections::hash_map::RandomState::new().hash_one((
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    format!("{n:016x}")
}

/// The escalation words that start a command as `user` through `method`
/// (`sudo`, `doas`; the inventory's `escalate`). With a password, sudo reads
/// it from stdin (`-S`, no prompt); doas cannot, and `none` means the host
/// does not escalate. `set_home` adds sudo's `-H`, so `$HOME` is the
/// target's even where sudoers keeps the caller's; doas always sets it.
pub fn escalation(
    method: &str,
    user: &str,
    with_password: bool,
    set_home: bool,
) -> std::io::Result<Vec<String>> {
    let mut words: Vec<String> = match (method, with_password) {
        ("sudo", false) => vec!["sudo".into(), "-n".into()],
        ("sudo", true) => vec!["sudo".into(), "-S".into(), "-p".into(), String::new()],
        ("doas", false) => vec!["doas".into(), "-n".into()],
        ("doas", true) => {
            return Err(std::io::Error::other(
                "doas cannot take a password from a pipe; configure doas for passwordless use",
            ));
        }
        ("none", _) => {
            return Err(std::io::Error::other(format!(
                "cannot run as `{user}`: this host's escalation method is `none`"
            )));
        }
        (other, _) => {
            return Err(std::io::Error::other(format!(
                "unknown escalation method `{other}` (expected sudo, doas, or none)"
            )));
        }
    };
    if set_home && method == "sudo" {
        words.push("-H".into());
    }
    words.extend(["-u".into(), user.to_string()]);
    Ok(words)
}

/// True for the line `sudo -S` writes when it rejected a password and is
/// about to read another (`Sorry, try again.`), or gives up (`N incorrect
/// password attempts`). The spawns run under `LC_ALL=C`, so the text is
/// sudo's own; a site that rewords it (`insults`, `badpass_message`) is
/// caught by [`FIRST_BYTE_DEADLINE`] instead.
pub fn password_rejected(line: &str) -> bool {
    let line = line.trim();
    line.ends_with("Sorry, try again.") || line.contains("incorrect password attempt")
}

/// The refusal for a rejected password.
pub fn password_rejected_message(user: &str, line: &str) -> String {
    format!(
        "escalation as `{user}` failed: the password given with --escalate-password-env \
         was rejected ({})",
        line.trim()
    )
}

/// The refusal for a spawn that wrote nothing within `deadline`.
pub fn deadline_message(user: &str, deadline: Duration, stderr: &str) -> String {
    format!(
        "escalation as `{user}` did not answer within {deadline:?}; it may be waiting for \
         a password it did not accept, or at a prompt rustible does not recognise{}",
        tail(stderr)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "web_site-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn launch() -> Launch {
        Launch::new(NAME, 1234, Mode::Helper, "svc", "as_user(svc)").with_suffix("feed")
    }

    // ---- pure ----

    #[test]
    fn a_warm_run_is_one_spawn() {
        let mut l = launch();
        assert_eq!(l.spawn(), Spawn::Try(Place::Home));
        assert_eq!(l.after_try(Answer::Running), Next::Ready);
        assert_eq!(l.installed(), None);
    }

    #[test]
    fn a_cold_run_installs_in_the_home_and_tries_again() {
        let mut l = launch();
        assert_eq!(
            l.after_try(Answer::Missing),
            Next::Spawn(Spawn::Install(Place::Home))
        );
        assert_eq!(l.after_install(0, ""), Next::Spawn(Spawn::Try(Place::Home)));
        assert_eq!(l.after_try(Answer::Running), Next::Ready);
        assert_eq!(l.installed(), Some(Place::Home));
    }

    #[test]
    fn an_unusable_or_noexec_home_falls_through_to_the_temp_directory() {
        for status in [UNUSABLE, NOEXEC] {
            let mut l = launch();
            l.after_try(Answer::Missing);
            assert_eq!(
                l.after_install(
                    status,
                    "rustible: no usable home (/nonexistent does not exist)\n"
                ),
                Next::Spawn(Spawn::Install(Place::Temp))
            );
            assert_eq!(l.after_install(0, ""), Next::Spawn(Spawn::Try(Place::Temp)));
            assert_eq!(l.after_try(Answer::Running), Next::Ready);
            assert_eq!(l.installed(), Some(Place::Temp));
        }
    }

    #[test]
    fn a_home_copy_that_still_does_not_run_falls_through_too() {
        let mut l = launch();
        l.after_try(Answer::Missing);
        l.after_install(0, "");
        assert_eq!(
            l.after_try(Answer::Missing),
            Next::Spawn(Spawn::Install(Place::Temp))
        );
        l.after_install(0, "");
        let Next::Refuse(msg) = l.after_try(Answer::Missing) else {
            panic!("expected a refusal")
        };
        assert!(
            msg.contains("installed in its home but is still not executable"),
            "{msg}"
        );
        assert!(
            msg.contains("installed in the temp directory but is still"),
            "{msg}"
        );
    }

    #[test]
    fn both_places_unusable_is_refused_naming_both_causes() {
        let mut l = launch();
        l.after_try(Answer::Missing);
        l.after_install(
            UNUSABLE,
            "mkdir: whatever\nrustible: no usable home (/nonexistent does not exist)\n",
        );
        let next = l.after_install(
            NOEXEC,
            "rustible: /tmp/rustible-feed is on a noexec filesystem\n",
        );
        assert_eq!(
            next,
            Next::Refuse(
                "as_user(svc): no usable home (/nonexistent does not exist) and \
                 /tmp/rustible-feed is on a noexec filesystem, so svc cannot run its \
                 copy of the playbook binary"
                    .into()
            )
        );
    }

    #[test]
    fn any_other_install_failure_is_refused_with_its_cause() {
        let mut l = launch();
        l.after_try(Answer::Missing);
        assert_eq!(
            l.after_install(1, "rustible: received 5 of 1234 bytes for /h/x\n"),
            Next::Refuse("as_user(svc): received 5 of 1234 bytes for /h/x".into())
        );
        let mut l = launch();
        l.after_try(Answer::Missing);
        assert_eq!(
            l.after_install(127, "sh: 1: cat: not found\n"),
            Next::Refuse(
                "as_user(svc): installing its copy of the playbook binary exited 127: \
                 sh: 1: cat: not found"
                    .into()
            )
        );
    }

    #[test]
    fn a_name_without_a_content_hash_goes_straight_to_the_temp_directory() {
        let mut l = Launch::new("repro-ws", 10, Mode::Helper, "svc", "as_user(svc)");
        assert_eq!(l.spawn(), Spawn::Install(Place::Temp));
        let Next::Refuse(msg) = l.after_install(
            UNUSABLE,
            "rustible: cannot create /t/x: Permission denied\n",
        ) else {
            panic!("expected a refusal")
        };
        assert_eq!(
            msg,
            "as_user(svc): cannot create /t/x: Permission denied, so svc cannot run its \
             copy of the playbook binary"
        );
    }

    #[test]
    fn content_addressed_names() {
        assert!(is_content_addressed(NAME));
        assert!(!is_content_addressed("repro-ws"));
        assert!(!is_content_addressed(&format!("-{}", "a".repeat(64))));
        assert!(!is_content_addressed(&format!("x-{}", "A".repeat(64))));
        assert!(!is_content_addressed(&format!("x-{}", "a".repeat(63))));
    }

    #[test]
    fn argv_passes_everything_variable_as_arguments() {
        let l = launch();
        let a = l.argv(Spawn::Try(Place::Home));
        assert_eq!(a[..2], ["/bin/sh", "-c"]);
        assert_eq!(a[2], TRY);
        assert_eq!(a[3..], ["rustible", NAME, "--helper"]);
        assert_eq!(
            l.argv(Spawn::Try(Place::Temp))[3..],
            ["rustible", NAME, "--helper", "feed"]
        );
        assert_eq!(
            l.argv(Spawn::Install(Place::Home))[3..],
            ["rustible", NAME, "1234"]
        );
        assert_eq!(
            l.argv(Spawn::Install(Place::Temp))[3..],
            ["rustible", NAME, "1234", "feed"]
        );
        let r = Launch::new(NAME, 1, Mode::Remote, "svc", "x");
        assert_eq!(r.argv(Spawn::Try(Place::Home))[5], "--remote");
        // The script's literal and the runtime's flag are the same word.
        assert!(TRY.contains(&format!("${{3:+{EPHEMERAL_FLAG}}}")));
    }

    #[test]
    fn escalation_words() {
        let w = |m, pw, home| escalation(m, "svc", pw, home).unwrap();
        assert_eq!(w("sudo", false, false), ["sudo", "-n", "-u", "svc"]);
        assert_eq!(w("sudo", false, true), ["sudo", "-n", "-H", "-u", "svc"]);
        assert_eq!(
            w("sudo", true, true),
            ["sudo", "-S", "-p", "", "-H", "-u", "svc"]
        );
        assert_eq!(w("doas", false, true), ["doas", "-n", "-u", "svc"]);
        assert!(escalation("doas", "svc", true, true).is_err());
        assert!(
            escalation("none", "svc", false, true)
                .unwrap_err()
                .to_string()
                .contains("`none`")
        );
        assert!(escalation("pkexec", "svc", false, true).is_err());
    }

    #[test]
    fn sudos_rejection_lines() {
        assert!(password_rejected("Sorry, try again."));
        assert!(password_rejected("sudo: 1 incorrect password attempt"));
        assert!(password_rejected("sudo: 3 incorrect password attempts"));
        assert!(!password_rejected("sudo: a password is required"));
        assert!(!password_rejected(
            "We trust you have received the usual lecture"
        ));
    }

    #[test]
    fn the_deadline_message_names_the_wait_and_the_last_line() {
        assert_eq!(
            deadline_message("svc", FIRST_BYTE_DEADLINE, "x\nPassword for svc:\n"),
            "escalation as `svc` did not answer within 60s; it may be waiting for a password \
             it did not accept, or at a prompt rustible does not recognise: Password for svc:"
        );
    }

    #[test]
    fn suffixes_differ() {
        let (a, b) = (random_suffix(), random_suffix());
        assert_ne!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    // ---- real /bin/sh ----
    //
    // The scripts run as the test's own user with `HOME` and `TMPDIR`
    // pointed into a temp directory, which is what they see behind sudo.
    // These run on the macOS job too, so bash 3.2 and BSD `wc` see them.

    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    /// Run `script` with `args`, `bytes` on stdin: (status, stdout, stderr).
    fn sh(
        script: &str,
        args: &[&str],
        home: &Path,
        tmp: &Path,
        bytes: &[u8],
    ) -> (i32, String, String) {
        sh_in(Path::new("/"), script, args, home, tmp, bytes)
    }

    fn sh_in(
        cwd: &Path,
        script: &str,
        args: &[&str],
        home: &Path,
        tmp: &Path,
        bytes: &[u8],
    ) -> (i32, String, String) {
        let mut child = Command::new("/bin/sh")
            .args(["-c", script, "rustible"])
            .args(args)
            .env("HOME", home)
            .env("TMPDIR", tmp)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(bytes);
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    const BIN: &[u8] = b"#!/bin/sh\necho \"ran $0 $*\"\n";

    #[test]
    fn install_then_try_runs_the_copy_from_the_home() {
        let dir = tempfile::tempdir().unwrap();
        let (home, tmp) = (dir.path().join("home"), dir.path().join("tmp"));
        std::fs::create_dir_all(&home).unwrap();
        let size = BIN.len().to_string();
        assert_eq!(
            sh(TRY, &[NAME, "--helper"], &home, &tmp, b""),
            (0, "N".into(), String::new())
        );
        let (status, out, err) = sh(INSTALL, &[NAME, &size], &home, &tmp, BIN);
        assert_eq!((status, out.as_str()), (0, "I"), "{err}");
        let copy = home.join(".cache/rustible/bin").join(NAME);
        assert_eq!(std::fs::read(&copy).unwrap(), BIN);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&copy), 0o700);
        assert_eq!(mode(copy.parent().unwrap()), 0o700);
        let (status, out, _) = sh(TRY, &[NAME, "--helper"], &home, &tmp, b"");
        assert_eq!(
            (status, out),
            (0, format!("Rran {} --helper\n", copy.display()))
        );
        // No temp file is left next to it.
        assert_eq!(
            std::fs::read_dir(copy.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn a_copy_in_the_temp_directory_is_run_as_ephemeral() {
        let dir = tempfile::tempdir().unwrap();
        let (home, tmp) = (dir.path().join("nope"), dir.path().join("tmp"));
        std::fs::create_dir_all(&tmp).unwrap();
        let size = BIN.len().to_string();
        let (status, _, err) = sh(INSTALL, &[NAME, &size], &home, &tmp, BIN);
        assert_eq!(status, UNUSABLE);
        assert_eq!(
            install_cause(&err).unwrap(),
            format!("no usable home ({} does not exist)", home.display())
        );
        let (status, out, err) = sh(INSTALL, &[NAME, &size, "feed"], &home, &tmp, BIN);
        assert_eq!((status, out.as_str()), (0, "I"), "{err}");
        let d = tmp.join("rustible-feed");
        assert_eq!(
            std::fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let (_, out, _) = sh(TRY, &[NAME, "--remote", "feed"], &home, &tmp, b"");
        assert_eq!(
            out,
            format!("Rran {} --remote --ephemeral\n", d.join(NAME).display())
        );
    }

    /// The temp directory is 0700 because `mkdir -m 700` says so, not
    /// because of the script's umask: run without the umask, it still is.
    #[test]
    fn the_temp_directory_is_private_whatever_the_umask() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().to_path_buf();
        let loose = format!("umask 022; {}", INSTALL.replace("umask 077; ", ""));
        assert_ne!(loose, INSTALL);
        let (status, _, err) = sh(&loose, &[NAME, "3", "feed"], &tmp.join("h"), &tmp, b"abc");
        assert_eq!(status, 0, "{err}");
        let mode = std::fs::metadata(tmp.join("rustible-feed"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// A `TMPDIR` that looks like a flag is a path: every path operand
    /// follows `--`.
    #[test]
    fn a_tmpdir_that_looks_like_a_flag_is_a_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("-p")).unwrap();
        let (status, _, err) = sh_in(
            dir.path(),
            INSTALL,
            &[NAME, &BIN.len().to_string(), "feed"],
            &dir.path().join("h"),
            Path::new("-p"),
            BIN,
        );
        assert_eq!(status, 0, "{err}");
        assert_eq!(
            std::fs::read(dir.path().join("-p/rustible-feed").join(NAME)).unwrap(),
            BIN
        );
    }

    /// `--` ends the options, so it goes before every operand, a mode
    /// included: BSD `chmod 700 -- f` reads `--` as a file, and GNU
    /// `chmod 700 -p/f` reads `-p/f` as options.
    ///
    /// GNU's tools permute their arguments and accept `--` anywhere, so a
    /// run on Linux cannot catch a misplaced one; this reads the scripts.
    #[test]
    fn end_of_options_precedes_every_operand() {
        for script in [TRY, INSTALL] {
            // Every `--`: only options, and the value `-m` takes, come
            // between the utility and it.
            for (i, _) in script.match_indices(" -- ") {
                let head = &script[..i];
                let start = head
                    .rfind(|c| matches!(c, '(' | ';' | '{' | '&' | '|' | '\''))
                    .map_or(0, |p| p + 1);
                let words: Vec<&str> = head[start..].split_whitespace().collect();
                let (utility, flags) = words.split_first().unwrap();
                let mut it = flags.iter();
                while let Some(w) = it.next() {
                    assert!(w.starts_with('-'), "{utility}: `{w}` comes before `--`");
                    if *w == "-m" {
                        it.next();
                    }
                }
            }
            // Every utility that takes a path has one, before the path.
            for utility in ["mkdir ", "rm ", "rmdir ", "chmod ", "mv "] {
                for (i, _) in script.match_indices(utility) {
                    if i > 0 && !script[..i].ends_with([' ', '(', '{', '\'']) {
                        continue;
                    }
                    let rest = &script[i..];
                    let first_path = rest.find('"').unwrap();
                    assert!(
                        rest[..first_path].contains(" -- "),
                        "{}",
                        &rest[..first_path]
                    );
                }
            }
        }
        assert!(INSTALL.contains(r#"chmod -- 700 "$t""#), "{INSTALL}");
    }

    /// `exec` is given a path that cannot be read as an option: bash's
    /// `exec` (macOS's `/bin/sh`) takes options, and a relative `TMPDIR`
    /// could start with `-`.
    #[test]
    fn a_relative_temp_copy_is_execd_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("-p/rustible-feed");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(NAME), BIN).unwrap();
        std::fs::set_permissions(d.join(NAME), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (_, out, err) = sh_in(
            dir.path(),
            TRY,
            &[NAME, "--remote", "feed"],
            &dir.path().join("h"),
            Path::new("-p"),
            b"",
        );
        assert_eq!(
            out,
            format!("Rran ./-p/rustible-feed/{NAME} --remote --ephemeral\n"),
            "{err}"
        );
    }

    /// Over ssh the command passes through the login's shell, and csh
    /// refuses a newline inside quotes.
    #[test]
    fn every_script_is_one_line() {
        for script in [TRY, INSTALL, EXEC] {
            assert!(!script.contains('\n'), "{script}");
        }
    }

    #[test]
    fn a_temp_directory_that_exists_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().to_path_buf();
        std::fs::create_dir(tmp.join("rustible-feed")).unwrap();
        let (status, _, err) = sh(INSTALL, &[NAME, "3", "feed"], &tmp.join("h"), &tmp, b"abc");
        assert_eq!(status, UNUSABLE, "{err}");
        let cause = install_cause(&err).unwrap();
        assert!(
            cause.starts_with(&format!("cannot create {}/rustible-feed: ", tmp.display())),
            "{cause}"
        );
        assert!(cause.ends_with("File exists"), "{cause}");
        // Nothing was written into the directory somebody else made.
        assert_eq!(
            std::fs::read_dir(tmp.join("rustible-feed"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn a_short_stream_is_refused_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let (status, _, err) = sh(INSTALL, &[NAME, "999"], &home, &home, BIN);
        assert_eq!(status, 1);
        let bin = home.join(".cache/rustible/bin");
        assert_eq!(
            install_cause(&err).unwrap(),
            format!(
                "received {} of 999 bytes for {}/{NAME}",
                BIN.len(),
                bin.display()
            )
        );
        assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 0);
        // The same in the temp directory, which goes too.
        let (status, _, _) = sh(INSTALL, &[NAME, "999", "feed"], &home, &home, BIN);
        assert_eq!(status, 1);
        assert!(!home.join("rustible-feed").exists());
    }

    #[test]
    fn a_copy_that_is_not_executable_is_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let bin = home.join(".cache/rustible/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(NAME), BIN).unwrap();
        std::fs::set_permissions(bin.join(NAME), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(sh(TRY, &[NAME, "--helper"], &home, &home, b"").1, "N");
        let (status, _, err) = sh(INSTALL, &[NAME, &BIN.len().to_string()], &home, &home, BIN);
        assert_eq!(status, 0, "{err}");
        assert!(
            sh(TRY, &[NAME, "--helper"], &home, &home, b"")
                .1
                .starts_with("Rran ")
        );
    }

    #[test]
    fn a_relative_home_is_not_a_home() {
        let dir = tempfile::tempdir().unwrap();
        let rel = Path::new("relative");
        assert_eq!(sh(TRY, &[NAME, "--helper"], rel, dir.path(), b"").1, "N");
        let (status, _, err) = sh(INSTALL, &[NAME, "1"], rel, dir.path(), b"x");
        assert_eq!(status, UNUSABLE);
        assert_eq!(
            install_cause(&err).unwrap(),
            "no usable home (HOME is \"relative\", not an absolute path)"
        );
    }
}
