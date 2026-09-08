//! Kernel parameters. Ansible's `ansible.posix.sysctl`.
//!
//! [`Present`] persists `key = value` in a drop-in under `/etc/sysctl.d/`
//! (default `99-rustible.conf`) and, unless `.apply_now(false)`, makes the
//! live value in `/proc/sys` match with `sysctl -w`.

use std::path::{Path, PathBuf};

use rustible_sdk::prelude::*;

const DEFAULT_FILE: &str = "/etc/sysctl.d/99-rustible.conf";

/// Output of [`Present`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysctlReport {
    pub key: String,
    pub value: String,
    /// The live value before this step; `None` when the kernel has no such
    /// key (only possible with `.apply_now(false)`).
    pub previous_live: Option<String>,
    /// The drop-in file the setting is persisted in.
    pub file: PathBuf,
}

/// Ensure a kernel parameter is set, persistently and (by default) live.
/// `sysctl: name=... value=... sysctl_set=yes reload=yes`.
///
/// Both the file and the live value count: the step is `changed` when either
/// differs and `ok` only when both match. Values are compared token-wise, so
/// `4096 131072 6291456` equals the kernel's tab-separated form. Needs root.
/// Refuses keys with characters outside `[A-Za-z0-9_./-]`, and, when
/// applying live, keys the running kernel does not have.
#[derive(Debug, Clone)]
pub struct Present {
    key: String,
    value: String,
    file: PathBuf,
    apply_now: bool,
}

impl Present {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Present {
            key: key.into(),
            value: value.into(),
            file: PathBuf::from(DEFAULT_FILE),
            apply_now: true,
        }
    }

    /// Persist in this file instead of `/etc/sysctl.d/99-rustible.conf`.
    /// Its directory must exist.
    pub fn file(mut self, path: impl Into<PathBuf>) -> Self {
        self.file = path.into();
        self
    }

    /// Also run `sysctl -w` so the running kernel picks the value up
    /// (default: yes). Turn off in containers, where `/proc/sys` is read-only.
    pub fn apply_now(mut self, on: bool) -> Self {
        self.apply_now = on;
        self
    }
}

/// Why a string is not a sysctl key. Pure.
pub fn validate_key(key: &str) -> std::result::Result<(), String> {
    if key.is_empty() {
        return Err("sysctl key is empty".into());
    }
    if let Some(bad) = key
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-')))
    {
        return Err(format!(
            "sysctl key `{key}` contains `{bad}`; allowed: letters, digits, `_`, `.`, `/`, `-`"
        ));
    }
    Ok(())
}

/// `/proc/sys/<key with dots as slashes>`.
pub fn proc_path(key: &str) -> PathBuf {
    Path::new("/proc/sys").join(key.replace('.', "/"))
}

/// Whitespace-insensitive form, so `a\tb` and `a b` compare equal.
pub fn normalize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `key = value` from one sysctl.conf line, ignoring comments and blanks. A
/// leading `-` (sysctl's "ignore errors" marker) is stripped from the key.
pub fn parse_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
        return None;
    }
    let (k, v) = line.split_once('=')?;
    let k = k.trim().trim_start_matches('-').trim();
    (!k.is_empty()).then_some((k, v.trim()))
}

/// The value the file currently gives `key` (last line wins, as sysctl reads it).
pub fn value_in(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter_map(parse_line)
        .rfind(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
}

/// Pure planning: the new file text with exactly one `key = value` line, or
/// `None` when the file already has one such line and no duplicates. The
/// first line for the key is rewritten in place, later duplicates dropped,
/// and a missing key appended. Comments and other keys are untouched.
pub fn plan_sysctl_line(text: &str, key: &str, value: &str) -> Option<String> {
    let wanted = format!("{key} = {value}");
    let mut hits = text
        .lines()
        .enumerate()
        .filter(|(_, l)| parse_line(l).is_some_and(|(k, _)| k == key));
    let first = hits.next().map(|(i, _)| i);
    let duplicates: Vec<usize> = hits.map(|(i, _)| i).collect();

    if let Some(i) = first
        && duplicates.is_empty()
        && text
            .lines()
            .nth(i)
            .and_then(parse_line)
            .is_some_and(|(_, v)| normalize(v) == normalize(value))
    {
        return None;
    }

    let mut lines: Vec<String> = text
        .lines()
        .enumerate()
        .filter(|(i, _)| !duplicates.contains(i))
        .map(|(i, l)| {
            if Some(i) == first {
                wanted.clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    if first.is_none() {
        lines.push(wanted);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    Some(out)
}

impl Present {
    /// Current file text, `""` when the file does not exist yet. The
    /// directory must exist; the file is created but not its parent.
    fn read_file(&self, sys: &System) -> Result<String> {
        if sys.stat(&self.file)?.is_none() {
            let parent = self.file.parent().unwrap_or(Path::new("/"));
            match sys.stat_follow(parent)? {
                Some(s) if s.kind == rustible_sdk::backend::FileKind::Dir => {}
                _ => bail!(
                    "{} does not exist; sysctl::Present creates the file but not its directory (use file::Directory)",
                    parent.display()
                ),
            }
        }
        crate::file::read_text_or_empty(sys, &self.file, true)
    }

    fn read_live(&self, sys: &System) -> Result<Option<String>> {
        let p = proc_path(&self.key);
        if !sys.exists(&p)? {
            return Ok(None);
        }
        Ok(Some(sys.read_to_string(&p)?.trim().to_string()))
    }
}

impl Op for Present {
    type Output = SysctlReport;

    fn check(&self, sys: &System) -> Result<Plan<SysctlReport>> {
        if let Err(why) = validate_key(&self.key) {
            bail!("sysctl::Present: {why}");
        }
        if self.value.contains('\n') {
            bail!(
                "sysctl::Present: value for `{}` contains a newline",
                self.key
            );
        }
        if !sys.is_root() {
            bail!(
                "sysctl::Present needs root, but this runs as `{}` (use escalate = true or ctx.as_root())",
                sys.facts().user
            );
        }

        let text = self.read_file(sys)?;
        let live = self.read_live(sys)?;
        if live.is_none() && self.apply_now {
            bail!(
                "sysctl key `{}` does not exist on this kernel ({} is missing); \
                 use .apply_now(false) to only persist it",
                self.key,
                proc_path(&self.key).display()
            );
        }

        let mut changes = vec![];
        if plan_sysctl_line(&text, &self.key, &self.value).is_some() {
            changes.push(AttrChange {
                name: self.file.display().to_string(),
                from: value_in(&text, &self.key).unwrap_or_else(|| "absent".into()),
                to: self.value.clone(),
            });
        }
        if self.apply_now
            && let Some(current) = &live
            && normalize(current) != normalize(&self.value)
        {
            changes.push(AttrChange {
                name: "live".into(),
                from: current.clone(),
                to: self.value.clone(),
            });
        }
        let report = SysctlReport {
            key: self.key.clone(),
            value: self.value.clone(),
            previous_live: live,
            file: self.file.clone(),
        };
        if changes.is_empty() {
            return Ok(Plan::Satisfied(report));
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: format!("sysctl {}", self.key),
                changes,
            },
            report,
        ))
    }

    fn apply(&self, sys: &System, change: Change<SysctlReport>) -> Result<SysctlReport> {
        let Some(report) = change.predicted else {
            bail!("sysctl::Present::apply received a change without its prediction");
        };
        // The diff carries no text, so re-plan against the file as it is now.
        let text = self.read_file(sys)?;
        if let Some(new_text) = plan_sysctl_line(&text, &self.key, &self.value) {
            sys.write_atomic(&self.file, new_text.as_bytes())?;
        }
        if self.apply_now
            && let Some(current) = &report.previous_live
            && normalize(current) != normalize(&self.value)
        {
            sys.cmd("sysctl")
                .args(["-w", &format!("{}={}", self.key, self.value)])
                .run()?;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;

    use super::*;

    const KEY: &str = "net.ipv4.ip_forward";
    const PROC: &str = "/proc/sys/net/ipv4/ip_forward";

    fn sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default()))
    }

    /// A box with `/etc/sysctl.d`, the drop-in holding `text` (or absent), and
    /// the live value `live` (or no such key).
    fn box_with(text: Option<&str>, live: Option<&str>) -> Fake {
        let mut fake = Fake::new()
            .with_dir("/etc/sysctl.d")
            .with_cmd("sysctl", None, 0, "");
        if let Some(t) = text {
            fake = fake.with_file(DEFAULT_FILE, t);
        }
        if let Some(l) = live {
            fake = fake.with_file(PROC, format!("{l}\n"));
        }
        fake
    }

    // ---- pure ----

    #[test]
    fn key_validation() {
        for k in [
            KEY,
            "kernel.sysrq",
            "net/ipv4/ip_forward",
            "fs.inotify.max_user_watches",
            "a-b_c",
        ] {
            assert_eq!(validate_key(k), Ok(()), "{k}");
        }
        assert!(validate_key("").unwrap_err().contains("empty"));
        assert!(
            validate_key("net.ipv4 ip_forward")
                .unwrap_err()
                .contains("` `")
        );
        assert!(validate_key("a=b").unwrap_err().contains("`=`"));
        assert!(validate_key("a;b").unwrap_err().contains("`;`"));
    }

    #[test]
    fn proc_path_maps_dots_to_slashes() {
        assert_eq!(proc_path(KEY), PathBuf::from(PROC));
        assert_eq!(
            proc_path("kernel.sysrq"),
            PathBuf::from("/proc/sys/kernel/sysrq")
        );
    }

    #[test]
    fn parse_line_handles_spacing_comments_and_ignore_marker() {
        assert_eq!(parse_line("net.ipv4.ip_forward=1"), Some((KEY, "1")));
        assert_eq!(parse_line("  net.ipv4.ip_forward = 1  "), Some((KEY, "1")));
        assert_eq!(parse_line("-net.ipv4.ip_forward = 1"), Some((KEY, "1")));
        assert_eq!(parse_line("# net.ipv4.ip_forward = 1"), None);
        assert_eq!(parse_line("; comment"), None);
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("no equals sign"), None);
        assert_eq!(parse_line("= 1"), None);
    }

    #[test]
    fn plan_appends_to_empty_and_existing_text() {
        assert_eq!(
            plan_sysctl_line("", KEY, "1"),
            Some("net.ipv4.ip_forward = 1\n".into())
        );
        assert_eq!(
            plan_sysctl_line("# managed\nkernel.sysrq = 0\n", KEY, "1"),
            Some("# managed\nkernel.sysrq = 0\nnet.ipv4.ip_forward = 1\n".into())
        );
    }

    #[test]
    fn plan_replaces_in_place_and_keeps_the_rest() {
        let text = "# managed\nnet.ipv4.ip_forward=0\nkernel.sysrq = 0\n";
        assert_eq!(
            plan_sysctl_line(text, KEY, "1"),
            Some("# managed\nnet.ipv4.ip_forward = 1\nkernel.sysrq = 0\n".into())
        );
    }

    #[test]
    fn plan_is_satisfied_regardless_of_spacing() {
        assert_eq!(
            plan_sysctl_line("net.ipv4.ip_forward = 1\n", KEY, "1"),
            None
        );
        assert_eq!(plan_sysctl_line("net.ipv4.ip_forward=1\n", KEY, "1"), None);
        assert_eq!(plan_sysctl_line("-net.ipv4.ip_forward=1", KEY, "1"), None);
        assert_eq!(
            plan_sysctl_line(
                "net.ipv4.tcp_rmem = 4096\t131072 6291456\n",
                "net.ipv4.tcp_rmem",
                "4096 131072 6291456"
            ),
            None
        );
    }

    #[test]
    fn plan_collapses_duplicates_onto_the_first_line() {
        let text = "net.ipv4.ip_forward = 1\nkernel.sysrq = 0\nnet.ipv4.ip_forward = 0\n";
        assert_eq!(
            plan_sysctl_line(text, KEY, "1"),
            Some("net.ipv4.ip_forward = 1\nkernel.sysrq = 0\n".into())
        );
        assert_eq!(
            value_in(text, KEY),
            Some("0".into()),
            "last line wins when read"
        );
    }

    #[test]
    fn plan_does_not_match_a_key_prefix_or_comment() {
        let text = "# net.ipv4.ip_forward = 1\nnet.ipv4.ip_forward_extra = 1\n";
        assert_eq!(
            plan_sysctl_line(text, KEY, "1"),
            Some(format!("{text}net.ipv4.ip_forward = 1\n"))
        );
    }

    // ---- Fake ----

    #[test]
    fn satisfied_when_file_and_live_match() {
        let fake = Arc::new(box_with(Some("net.ipv4.ip_forward=1\n"), Some("1")));
        let Plan::Satisfied(r) = Present::new(KEY, "1").check(&sys(&fake)).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(r.previous_live.as_deref(), Some("1"));
        assert_eq!(r.file, PathBuf::from(DEFAULT_FILE));
        assert!(fake.argvs().is_empty());
    }

    #[test]
    fn change_when_only_the_file_differs_and_apply_writes_without_sysctl() {
        let fake = Arc::new(box_with(None, Some("1")));
        let s = sys(&fake);
        let op = Present::new(KEY, "1");
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(
            c.diff.render(),
            "sysctl net.ipv4.ip_forward:\n  /etc/sysctl.d/99-rustible.conf: absent -> 1\n"
        );
        op.apply(&s, c).unwrap();
        assert_eq!(
            fake.content(DEFAULT_FILE).unwrap(),
            "net.ipv4.ip_forward = 1\n"
        );
        assert!(
            fake.argvs().is_empty(),
            "live already matched: no sysctl -w"
        );
        assert!(matches!(op.check(&s).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn change_when_only_live_differs_and_apply_runs_sysctl_w() {
        let fake = Arc::new(box_with(Some("net.ipv4.ip_forward = 1\n"), Some("0")));
        let s = sys(&fake);
        let op = Present::new(KEY, "1");
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff.short(), "live=1");
        let r = op.apply(&s, c).unwrap();
        assert_eq!(r.previous_live.as_deref(), Some("0"));
        assert_eq!(
            fake.argvs(),
            vec![vec!["sysctl", "-w", "net.ipv4.ip_forward=1"]]
        );
        assert_eq!(
            fake.content(DEFAULT_FILE).unwrap(),
            "net.ipv4.ip_forward = 1\n"
        );
    }

    #[test]
    fn change_when_both_differ() {
        let fake = Arc::new(box_with(Some("# empty\n"), Some("0")));
        let s = sys(&fake);
        let op = Present::new(KEY, "1");
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff.short(), "/etc/sysctl.d/99-rustible.conf=1 live=1");
        op.apply(&s, c).unwrap();
        assert_eq!(
            fake.content(DEFAULT_FILE).unwrap(),
            "# empty\nnet.ipv4.ip_forward = 1\n"
        );
        assert_eq!(fake.argvs().len(), 1);
    }

    #[test]
    fn apply_now_false_only_persists() {
        let fake = Arc::new(box_with(None, Some("0")));
        let s = sys(&fake);
        let op = Present::new(KEY, "1").apply_now(false);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff.short(), "/etc/sysctl.d/99-rustible.conf=1");
        op.apply(&s, c).unwrap();
        assert!(fake.argvs().is_empty());
        assert_eq!(
            fake.content(DEFAULT_FILE).unwrap(),
            "net.ipv4.ip_forward = 1\n"
        );
        // Live still differs, but with apply_now(false) that is not our business.
        assert!(matches!(op.check(&s).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn missing_proc_key_is_refused_unless_persist_only() {
        let fake = Arc::new(box_with(None, None));
        let err = Present::new("net.nope", "1")
            .check(&sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist on this kernel"), "{err}");
        assert!(err.contains("/proc/sys/net/nope"), "{err}");

        let op = Present::new("net.nope", "1").apply_now(false);
        let Plan::Change(c) = op.check(&sys(&fake)).unwrap() else {
            panic!("persist-only must plan the file")
        };
        assert_eq!(c.predicted.as_ref().unwrap().previous_live, None);
        op.apply(&sys(&fake), c).unwrap();
        assert_eq!(fake.content(DEFAULT_FILE).unwrap(), "net.nope = 1\n");
    }

    #[test]
    fn custom_file_is_used() {
        let fake = Arc::new(box_with(None, Some("0")).with_dir("/etc/sysctl.d/x"));
        let s = sys(&fake);
        let op = Present::new(KEY, "1").file("/etc/sysctl.d/x/10-fwd.conf");
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!()
        };
        let r = op.apply(&s, c).unwrap();
        assert_eq!(r.file, PathBuf::from("/etc/sysctl.d/x/10-fwd.conf"));
        assert_eq!(
            fake.content("/etc/sysctl.d/x/10-fwd.conf").unwrap(),
            "net.ipv4.ip_forward = 1\n"
        );
    }

    #[test]
    fn missing_directory_is_refused() {
        let fake = Arc::new(Fake::new().with_file(PROC, "0\n"));
        let err = Present::new(KEY, "1")
            .check(&sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("/etc/sysctl.d does not exist"), "{err}");
    }

    #[test]
    fn multi_value_keys_compare_token_wise() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/etc/sysctl.d")
                .with_file(DEFAULT_FILE, "net.ipv4.tcp_rmem = 4096 131072 6291456\n")
                .with_file("/proc/sys/net/ipv4/tcp_rmem", "4096\t131072\t6291456\n"),
        );
        let op = Present::new("net.ipv4.tcp_rmem", "4096 131072 6291456");
        assert!(matches!(op.check(&sys(&fake)).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn refuses_bad_key_newline_value_and_non_root() {
        let fake = Arc::new(box_with(None, Some("0")));
        let err = Present::new("net.ipv4 ip_forward", "1")
            .check(&sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("sysctl::Present: sysctl key"), "{err}");
        let err = Present::new(KEY, "1\n0")
            .check(&sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("newline"), "{err}");

        let mut facts = sys(&fake).facts().clone();
        facts.is_root = false;
        facts.user = "cadu".into();
        let s = sys(&fake).with_facts(facts);
        let err = Present::new(KEY, "1").check(&s).unwrap_err().to_string();
        assert!(err.contains("needs root") && err.contains("cadu"), "{err}");
    }

    #[test]
    fn apply_without_prediction_is_refused() {
        let fake = Arc::new(box_with(None, Some("0")));
        let err = Present::new(KEY, "1")
            .apply(
                &sys(&fake),
                Change {
                    diff: Diff::summary("x"),
                    predicted: None,
                },
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("without its prediction"), "{err}");
        assert!(fake.file(DEFAULT_FILE).is_none());
    }

    #[test]
    fn check_mode_predicts_writes_nothing_and_runs_nothing() {
        let fake = Arc::new(box_with(None, Some("0")));
        let s = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        let mut ctx = Ctx::new(s, rustible_sdk::HostInfo::local());
        let r = ctx.step("fwd", Present::new(KEY, "1")).unwrap();
        assert!(r.changed && r.predicted && r.is_available());
        assert_eq!(r.previous_live.as_deref(), Some("0"));
        assert_eq!(r.key, KEY);
        assert!(
            fake.file(DEFAULT_FILE).is_none(),
            "check mode must not write"
        );
        assert!(fake.argvs().is_empty(), "check must run no commands");
    }

    #[test]
    fn step_changed_then_ok_when_live_follows() {
        let fake = Arc::new(box_with(None, Some("0")));
        let mut ctx = Ctx::new(sys(&fake), rustible_sdk::HostInfo::local());
        let r = ctx.step("fwd", Present::new(KEY, "1")).unwrap();
        assert!(r.changed);
        // The fake kernel does not react to sysctl -w; simulate it.
        rustible_sdk::backend::Backend::write(&*fake, Path::new(PROC), b"1\n").unwrap();
        let r = ctx.step("fwd", Present::new(KEY, "1")).unwrap();
        assert!(!r.changed);
    }
}
