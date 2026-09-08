//! [`keys_to_user`] and [`KeysToUser`]: fetch a GitHub user's keys and put
//! them in a system user's `authorized_keys`.

use std::sync::Arc;

use rustible_sdk::prelude::*;
use rustible_std::ssh::authorized_keys::{KeysReport, Present};

use crate::fetch::Fetch;
use crate::user_keys::UserKeys;

/// Install the GitHub user `gh_login`'s public keys into the system user
/// `sys_user`'s `~/.ssh/authorized_keys`. Ansible's
///
/// ```yaml
/// - ansible.posix.authorized_key:
///     user: "{{ sys_user }}"
///     key: https://github.com/{{ gh_login }}.keys
/// ```
///
/// Two steps run through `ctx.step` and show in the run output: `Fetch
/// GitHub keys of <gh_login>` (a lookup, always `ok`) and `Install GitHub
/// keys of <gh_login> for <sys_user>` (`changed` when a key was appended).
/// The second step is `ssh::authorized_keys::Present::for_user_name`, so its
/// rules apply: `sys_user` must exist in `/etc/passwd` and `~/.ssh` must
/// already exist (vision 6.7). Returns the install step's report.
///
/// **Additive**: keys already in the file that GitHub does not list are
/// left alone. This is the default because it can never lock anyone out;
/// [`KeysToUser::exclusive`] gives the "exactly these" behaviour. Keys are
/// installed with the comment `github:<gh_login>`, since the `.keys`
/// endpoint strips comments; see [`KeysToUser::without_comment`].
///
/// ```no_run
/// use rustible_sdk::prelude::*;
/// use rustible_github::keys_to_user;
///
/// fn role(ctx: &mut Ctx) -> Result<()> {
///     let r = keys_to_user(ctx, "flipbit03", "cadu")?;
///     if r.changed {
///         ctx.log(format!("added {} key(s) to {}", r.added.len(), r.path.display()));
///     }
///     Ok(())
/// }
/// ```
pub fn keys_to_user(
    ctx: &mut Ctx,
    gh_login: impl Into<String>,
    sys_user: impl Into<String>,
) -> Result<Applied<KeysReport>> {
    KeysToUser::new(gh_login, sys_user).run(ctx)
}

/// The configurable form of [`keys_to_user`]. Not an `Op` itself: it is a
/// helper that runs two ops, which is how a collection composes behaviour
/// without hiding steps from the report.
///
/// ```no_run
/// use rustible_sdk::prelude::*;
/// use rustible_github::KeysToUser;
///
/// fn role(ctx: &mut Ctx) -> Result<()> {
///     // "Exactly the keys flipbit03 has on GitHub, nothing else."
///     KeysToUser::new("flipbit03", "cadu").exclusive(true).run(ctx)?;
///     Ok(())
/// }
/// ```
#[derive(Debug, Clone)]
pub struct KeysToUser {
    gh_login: String,
    sys_user: String,
    exclusive: bool,
    comment: Option<String>,
    fetch: Option<Arc<dyn Fetch>>,
}

impl KeysToUser {
    /// Keys of GitHub user `gh_login` for the system account `sys_user`.
    /// Additive, with the comment `github:<gh_login>`.
    pub fn new(gh_login: impl Into<String>, sys_user: impl Into<String>) -> Self {
        let gh_login = gh_login.into();
        KeysToUser {
            comment: Some(format!("github:{gh_login}")),
            gh_login,
            sys_user: sys_user.into(),
            exclusive: false,
            fetch: None,
        }
    }

    /// Remove every key in the file that GitHub does not list (Ansible's
    /// `exclusive: true`). Comments and blank lines stay. **Refuses to run
    /// when GitHub returns no keys**: emptying `authorized_keys` because a
    /// user deleted their GitHub keys is a lockout, not a desired state. Use
    /// `ssh::authorized_keys::Present` directly if that really is wanted.
    pub fn exclusive(mut self, on: bool) -> Self {
        self.exclusive = on;
        self
    }

    /// Install the keys with this comment instead of `github:<gh_login>`.
    /// Comments never affect matching, so changing it later does not
    /// rewrite lines that are already there.
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    /// Install the keys exactly as GitHub serves them, with no comment.
    pub fn without_comment(mut self) -> Self {
        self.comment = None;
        self
    }

    /// Fetch through this [`Fetch`] instead of the default HTTPS client
    /// (see [`UserKeys::fetch_with`]).
    pub fn fetch_with(mut self, fetch: impl Fetch + 'static) -> Self {
        self.fetch = Some(Arc::new(fetch));
        self
    }

    /// Run both steps. See [`keys_to_user`] for what they are.
    pub fn run(self, ctx: &mut Ctx) -> Result<Applied<KeysReport>> {
        let mut lookup = UserKeys::of(&self.gh_login);
        if let Some(f) = &self.fetch {
            lookup = lookup.fetch_with(f.clone());
        }
        let keys = ctx.step(format!("Fetch GitHub keys of {}", self.gh_login), lookup)?;

        if keys.is_empty() {
            if self.exclusive {
                bail!(
                    "GitHub user `{}` has no public keys; refusing to empty {}'s authorized_keys \
                     in exclusive mode",
                    self.gh_login,
                    self.sys_user
                );
            }
            ctx.warn(format!(
                "GitHub user `{}` has no public keys; nothing to install for {}",
                self.gh_login, self.sys_user
            ));
        }

        let lines: Vec<String> = keys
            .iter()
            .map(|k| {
                let mut k = k.clone();
                if k.comment.is_none() {
                    k.comment = self.comment.clone();
                }
                k.to_line()
            })
            .collect();

        ctx.step(
            format!(
                "Install GitHub keys of {} for {}",
                self.gh_login, self.sys_user
            ),
            Present::for_user_name(&self.sys_user)
                .exclusive(self.exclusive)
                .keys(lines),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::{Collect, Event, Level, Status};
    use rustible_sdk::{Ctx, HostInfo};

    use super::*;
    use crate::fetch::Response;
    use crate::user_keys::tests::{Canned, ED1, ED2, RSA, body};

    const URL: &str = "https://github.com/flipbit03.keys";
    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\ncadu:x:1000:1000::/home/cadu:/bin/zsh\n";
    const AK: &str = "/home/cadu/.ssh/authorized_keys";

    fn fake_fs() -> Fake {
        Fake::new()
            .with_file("/etc/passwd", PASSWD)
            .with_dir("/home/cadu")
            .with_dir("/home/cadu/.ssh")
    }

    fn mk_ctx(fake: &Arc<Fake>, sink: &Arc<Collect>) -> Ctx {
        Ctx::new(System::fake(fake.clone(), sink.clone()), HostInfo::local())
    }

    fn finished(sink: &Collect) -> Vec<(String, Status)> {
        sink.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::StepFinished { name, status, .. } => Some((name, status)),
                _ => None,
            })
            .collect()
    }

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

    #[test]
    fn installs_keys_with_github_comment_then_is_idempotent() {
        let canned = Canned::answering(URL, Ok(Response::ok(body())));
        let fake = Arc::new(fake_fs());
        let sink = Arc::new(Collect::default());
        let mut ctx = mk_ctx(&fake, &sink);

        let r = KeysToUser::new("flipbit03", "cadu")
            .fetch_with(canned.clone())
            .run(&mut ctx)
            .unwrap();
        assert!(r.changed);
        assert_eq!(r.added.len(), 3);
        assert_eq!(r.path.to_str().unwrap(), AK);
        assert_eq!(
            fake.content(AK).unwrap(),
            format!("{RSA} github:flipbit03\n{ED1} github:flipbit03\n{ED2} github:flipbit03\n")
        );
        assert_eq!(
            finished(&sink),
            vec![
                ("Fetch GitHub keys of flipbit03".to_string(), Status::Ok),
                (
                    "Install GitHub keys of flipbit03 for cadu".to_string(),
                    Status::Changed
                ),
            ]
        );

        // Second run: fetch is ok again, install is ok, file untouched.
        let sink2 = Arc::new(Collect::default());
        let mut ctx = mk_ctx(&fake, &sink2);
        let r = keys_to_user_via(&mut ctx, canned.clone()).unwrap();
        assert!(!r.changed);
        assert_eq!(r.already_present.len(), 3);
        assert_eq!(
            finished(&sink2)
                .into_iter()
                .map(|(_, s)| s)
                .collect::<Vec<_>>(),
            vec![Status::Ok, Status::Ok]
        );
        assert_eq!(canned.asked().len(), 2, "one fetch per run");
    }

    fn keys_to_user_via(ctx: &mut Ctx, fetch: Arc<Canned>) -> Result<Applied<KeysReport>> {
        KeysToUser::new("flipbit03", "cadu")
            .fetch_with(fetch)
            .run(ctx)
    }

    #[test]
    fn additive_by_default_keeps_strangers() {
        let canned = Canned::answering(URL, Ok(Response::ok(format!("{ED1}\n"))));
        let stranger = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAISTRANGER ci@jenkins\n";
        let fake = Arc::new(fake_fs().with_file_mode(AK, stranger, 0o600));
        let sink = Arc::new(Collect::default());
        let mut ctx = mk_ctx(&fake, &sink);

        let r = keys_to_user_via(&mut ctx, canned).unwrap();
        assert!(r.changed);
        assert!(r.removed.is_empty());
        assert_eq!(
            fake.content(AK).unwrap(),
            format!("{stranger}{ED1} github:flipbit03\n")
        );
    }

    #[test]
    fn exclusive_removes_strangers() {
        let canned = Canned::answering(URL, Ok(Response::ok(format!("{ED1}\n"))));
        let stranger = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAISTRANGER ci@jenkins\n";
        let fake = Arc::new(fake_fs().with_file_mode(
            AK,
            format!("# keep me\n{stranger}{ED1} old-comment\n"),
            0o600,
        ));
        let sink = Arc::new(Collect::default());
        let mut ctx = mk_ctx(&fake, &sink);

        let r = KeysToUser::new("flipbit03", "cadu")
            .exclusive(true)
            .fetch_with(canned)
            .run(&mut ctx)
            .unwrap();
        assert!(r.changed);
        assert_eq!(r.removed.len(), 1);
        assert_eq!(r.removed[0].comment.as_deref(), Some("ci@jenkins"));
        assert!(
            r.added.is_empty(),
            "ED1 was already there, under another comment"
        );
        // The existing line keeps its own comment: identity is type+key.
        assert_eq!(
            fake.content(AK).unwrap(),
            format!("# keep me\n{ED1} old-comment\n")
        );
    }

    #[test]
    fn exclusive_with_no_keys_refuses_to_empty_the_file() {
        let canned = Canned::answering(URL, Ok(Response::ok("")));
        let before = format!("{ED1} cadu@x86\n");
        let fake = Arc::new(fake_fs().with_file_mode(AK, &before, 0o600));
        let sink = Arc::new(Collect::default());
        let mut ctx = mk_ctx(&fake, &sink);

        let e = KeysToUser::new("flipbit03", "cadu")
            .exclusive(true)
            .fetch_with(canned)
            .run(&mut ctx)
            .unwrap_err()
            .chain();
        assert!(e.contains("has no public keys"), "{e}");
        assert!(
            e.contains("refusing to empty cadu's authorized_keys"),
            "{e}"
        );
        assert_eq!(fake.content(AK).unwrap(), before, "file untouched");
        // The fetch step ran and passed; the install step never started.
        assert_eq!(
            finished(&sink),
            vec![("Fetch GitHub keys of flipbit03".to_string(), Status::Ok)]
        );
    }

    #[test]
    fn additive_with_no_keys_is_ok_and_warns() {
        let canned = Canned::answering(URL, Ok(Response::ok("")));
        let fake = Arc::new(fake_fs());
        let sink = Arc::new(Collect::default());
        let mut ctx = mk_ctx(&fake, &sink);

        let r = keys_to_user_via(&mut ctx, canned).unwrap();
        assert!(!r.changed);
        assert!(fake.file(AK).is_none(), "nothing to write, nothing created");
        assert_eq!(
            finished(&sink)
                .into_iter()
                .map(|(_, s)| s)
                .collect::<Vec<_>>(),
            vec![Status::Ok, Status::Ok]
        );
        let w = warnings(&sink);
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("flipbit03") && w[0].contains("no public keys"),
            "{w:?}"
        );
    }

    #[test]
    fn custom_comment_and_no_comment() {
        let canned = Canned::answering(URL, Ok(Response::ok(format!("{ED1}\n"))));
        let fake = Arc::new(fake_fs());
        let sink = Arc::new(Collect::default());
        let mut ctx = mk_ctx(&fake, &sink);
        KeysToUser::new("flipbit03", "cadu")
            .comment("cadu via github")
            .fetch_with(canned.clone())
            .run(&mut ctx)
            .unwrap();
        assert_eq!(
            fake.content(AK).unwrap(),
            format!("{ED1} cadu via github\n")
        );

        let fake = Arc::new(fake_fs());
        let mut ctx = mk_ctx(&fake, &sink);
        KeysToUser::new("flipbit03", "cadu")
            .without_comment()
            .fetch_with(canned)
            .run(&mut ctx)
            .unwrap();
        assert_eq!(fake.content(AK).unwrap(), format!("{ED1}\n"));
    }

    #[test]
    fn unknown_github_user_fails_before_touching_the_file() {
        let canned = Canned::answering(
            "https://github.com/nobody-here.keys",
            Ok(Response::with_status(404, "Not Found")),
        );
        let fake = Arc::new(fake_fs());
        let sink = Arc::new(Collect::default());
        let mut ctx = mk_ctx(&fake, &sink);

        let e = KeysToUser::new("nobody-here", "cadu")
            .fetch_with(canned)
            .run(&mut ctx)
            .unwrap_err()
            .chain();
        assert!(
            e.contains("GitHub user `nobody-here` does not exist"),
            "{e}"
        );
        assert!(fake.file(AK).is_none());
        assert_eq!(
            finished(&sink),
            vec![(
                "Fetch GitHub keys of nobody-here".to_string(),
                Status::Failed
            )]
        );
    }

    #[test]
    fn unknown_system_user_fails_in_the_install_step() {
        let canned = Canned::answering(URL, Ok(Response::ok(body())));
        let fake = Arc::new(fake_fs());
        let sink = Arc::new(Collect::default());
        let mut ctx = mk_ctx(&fake, &sink);

        let e = KeysToUser::new("flipbit03", "nobody")
            .fetch_with(canned)
            .run(&mut ctx)
            .unwrap_err()
            .chain();
        assert!(e.contains("user `nobody` does not exist"), "{e}");
        assert_eq!(
            finished(&sink),
            vec![
                ("Fetch GitHub keys of flipbit03".to_string(), Status::Ok),
                (
                    "Install GitHub keys of flipbit03 for nobody".to_string(),
                    Status::Failed
                ),
            ]
        );
    }

    #[test]
    fn check_mode_reports_would_change_without_writing() {
        let canned = Canned::answering(URL, Ok(Response::ok(body())));
        let fake = Arc::new(fake_fs());
        let sink = Arc::new(Collect::default());
        let mut ctx = Ctx::new(
            System::fake(fake.clone(), sink.clone()).with_check_mode(true),
            HostInfo::local(),
        );

        let r = keys_to_user_via(&mut ctx, canned).unwrap();
        assert!(r.changed && r.predicted);
        assert_eq!(r.added.len(), 3, "Present predicts its report");
        assert!(fake.file(AK).is_none());
        assert_eq!(
            finished(&sink)
                .into_iter()
                .map(|(_, s)| s)
                .collect::<Vec<_>>(),
            vec![Status::Ok, Status::WouldChange]
        );
    }
}
