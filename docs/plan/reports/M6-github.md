# M6 `rustible-github`

**Branch:** `m6-github` · **Governing sections:** vision 5.3, 6.2, 6.5, 6.7, 6.9, 7, 9, 12.

The first collection, and the reason it exists is as much demonstration as
function: it is a plain crate on `rustible-sdk` and `rustible-std`, never on
`rustible-cli`, with nothing in the core aware of it. A playbook picks it up
with `cargo add rustible-github`. Anyone writing a third-party collection can
copy this crate's shape without asking for anything from Rustible.

## What was built

**`UserKeys::of(login)`** is a read-only op (vision 6.5). It runs through
`ctx.step`, shows in the run report, is timed, and can never say `changed`.
Its `check` does the whole job and returns `Plan::Satisfied(Vec<PublicKey>)`;
`apply` bails, since it must never be reached. The output type is
`rustible_std::ssh::authorized_keys::PublicKey`, re-exported here, so the
lookup and the installer compose with no conversion.

**`keys_to_user(ctx, gh_login, sys_user)`** and its builder **`KeysToUser`**
are the composition Ansible reaches for a role and four modules to express.
It is not an `Op`. It is a helper that runs two `ctx.step` calls, so the fetch
and the install stay two visible lines in the run output rather than one
opaque one. A collection composing behaviour should not hide work from the
report.

**`Fetch` / `Https`** are the HTTP boundary, described below.

**`validate_login`** is a pure function exported for anyone who wants it.

Sizes: 1261 lines across five modules, of which roughly half is tests.

## Verification

Everything in `docs/plan/logs/M6-github-done.txt`, run twice: once on the
branch as committed, then again after `git merge main`. All commands exit 0.

| Command | Result |
| --- | --- |
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace` | pass |
| `cargo test -p rustible-github` | 26 pass, 1 ignored; 4 doctests, 1 ignored |
| `cargo test -p rustible-github -- --ignored` | the real HTTPS test passes |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --lib` | clean |
| `cargo package -p rustible-github --no-verify --allow-dirty` | packages |

`cargo package` only builds the `.crate` file. Nothing was published, and the
crate stays squatted at `0.0.1`.

## The endpoint: `<login>.keys`, not the REST API

`https://github.com/<login>.keys` returns `text/plain`, one `<type> <base64>`
line per key, comments stripped. `https://api.github.com/users/<login>/keys`
returns the same keys as JSON with an added integer id.

The plain-text endpoint wins on every axis that matters here. It needs no
token; the API's anonymous quota is 60 requests an hour, shared across
everything on the host, and a playbook that installs keys for a dozen users
on a box behind one NAT can hit it. It needs no JSON parser, so no
`serde_json` in a crate that otherwise has three dependencies. And the id the
API adds is the only extra field, which nothing here consumes. Ansible's own
`authorized_key` module points at the same `.keys` URL in its documented
example.

The cost is that a private GitHub Enterprise instance serves `.keys` only if
it is reachable unauthenticated. That case is covered by `.fetch_with(..)`,
which lets a playbook route the request however it needs.

## TLS: pure Rust, no C

`ureq` 3 with `default-features = false`, plus `rustls-no-provider` and
`rustls-webpki-roots`. The crypto provider is wired by hand to
`rustls_rustcrypto::provider()`.

This shape exists because ureq's default `rustls` feature pulls `ring`, which
is C, and vision 5.3 forbids dependencies bundling C. Trust roots come from
the bundled webpki set rather than the system store, so the binary does not
depend on the target having a usable `/etc/ssl` layout, which suits a
statically linked playbook binary landing on an unknown host.

The one thing to be aware of: `rustls-rustcrypto` is version `0.0.2-alpha`.
It is the only pure-Rust provider available today and it is measurably slower
than `ring`, which does not matter for one small `GET` per playbook run. It is
pinned in the workspace manifest in the same shape the `m6-net-archive`
branch uses, so the two merge without a fight. If the alpha ever becomes a
problem, the reversal is one feature flag and accepting `ring`.

The default client caps the body at 1 MiB, uses a 20-second connect and read
timeout, and sends a `rustible-github/<version>` user agent, because GitHub
rejects requests that send none.

## How the network boundary is tested

`UserKeys` never calls `ureq` directly. It calls `Fetch`, a one-method trait:

```rust
pub trait Fetch: Send + Sync + fmt::Debug {
    fn get(&self, url: &str) -> Result<Response>;
}
```

`Https` is the real implementation and the default. Tests inject `Canned`,
which answers from a table and records every URL it was asked for. Everything
above the socket is therefore ordinary offline unit-testing: the URL that gets
built, 404 versus empty body, non-200 statuses, transport errors, CRLF and
blank lines, an unparseable line, the login validator refusing before any
request, and every branch of the two-step composition including check mode.

A non-2xx status is deliberately not an error at the `Fetch` level. The op
decides what a 404 means, which keeps the transport dumb and the policy in one
readable place.

Exactly one test crosses the network, `network_fetches_flipbit03_keys_over_https`,
and it is `#[ignore]`d. It fetches Cadu's real keys over real TLS, which is the
only way to know the hand-wired rustls provider actually completes a handshake
against GitHub. It passes. It stays ignored so that `cargo test` is
deterministic and offline everywhere, CI included, per the M7 CI contract.

The offline claim was checked rather than asserted. With `HTTPS_PROXY`,
`HTTP_PROXY` and `ALL_PROXY` all pointed at a dead port (ureq reads the proxy
environment), the default suite still passes and the ignored test fails with
`Connection refused`. The failure is the control: it proves the blackhole was
in effect, so the default suite's passing means it never reached the network.
Both runs are in the done log. A network namespace would have been the
cleaner instrument, but this host has `kernel.apparmor_restrict_unprivileged_userns = 1`
and bubblewrap cannot bring up loopback, so the proxy blackhole stands in.

## `keys_to_user` is additive by default

Additive: keys already in `authorized_keys` that GitHub does not list are left
alone. `.exclusive(true)` gives Ansible's `exclusive: true` behaviour.

Additive is the default for one reason: it cannot lock anyone out. Exclusive
mode makes GitHub the sole authority over an account's SSH access, and the
consequence of getting that wrong is losing the box. That is a decision a
playbook author should make deliberately, by typing it. Ansible's default
agrees, so the least surprising behaviour is also the safest one.

Exclusive mode carries one guard that Ansible does not have: **an empty key
list is a hard error, not an empty file.** If a GitHub user deletes their keys,
or their account is renamed so the login resolves to someone with none, the
Ansible behaviour is to truncate `authorized_keys` and lock the account. Here
that fails the step and names the user. Anyone who genuinely wants to empty the
file can call `ssh::authorized_keys::Present` directly, which is the honest way
to ask for it.

In additive mode an empty list is not an error. It warns and installs nothing.

Keys are installed with the comment `github:<login>`, since the `.keys`
endpoint strips comments and an unlabelled key in `authorized_keys` is hard to
audit later. Comments never take part in matching, so changing the comment
later does not rewrite lines that are already there. `.comment(..)` and
`.without_comment()` override.

## Errors versus empty, throughout

A login that cannot exist fails at `check` before any request, so a typo like
`cadu@x86` or an empty var fails locally with a message rather than as a 404.
A 404 fails the step naming the user: an account that does not exist is a
mistake, and installing nothing quietly would hide it. A 200 with an empty
body is a successful step returning an empty `Vec`: that user exists and has
published no keys. A body line that is not a public key fails the step naming
the line number, because a silently dropped key is worse than a loud failure,
especially under `exclusive`.

## Deviations and things worth knowing

**Check mode still performs the fetch.** `UserKeys::check` is where the work
happens, so a `--check` run makes a real HTTPS request. This is correct for a
read-only lookup (vision 12: check mode must not *change* anything, and a GET
changes nothing) but it does mean a dry run needs egress.

**The target needs egress, not the controller.** Ansible's
`lookup('url', ...)` runs on the controller. Rustible runs the playbook binary
on the target host (vision 5), so `github.com` must be reachable *from the
managed machine*. This is a real behavioural difference from the Ansible
equivalent and is documented in the crate docs. Air-gapped targets should
fetch keys elsewhere and pass them to `ssh::authorized_keys::Present`.

**`~/.ssh` is not created.** The install step is
`ssh::authorized_keys::Present::for_user_name`, so its rules apply unchanged:
the system user must exist in `/etc/passwd` and `~/.ssh` must already exist
(vision 6.7, prerequisites are not created silently). A playbook does
`user::Present` and `file::Directory` first; the crate-level example shows it.

**No `System` primitive was added.** An HTTP request is neither a file nor a
process, and the SDK has no network primitive by design. Rather than widen
`Backend`, the collection owns its own seam. That is the point worth copying:
a collection needing something the core does not model adds a trait to itself,
not a method to the SDK.

**Nothing outside the crate was modified** except six lines of workspace
dependencies in the root manifest.

**The M7 release workflow already lists `rustible-github`**, so no change was
needed there.

## Merge

`git merge main` brought in M3's CLI and M5. The only conflict was
`Cargo.lock`, resolved by taking main's copy and letting cargo re-add the three
network dependencies from the merged manifest. `docs/plan/DECISIONS.md` merged
cleanly and keeps both sides. The whole gate was re-run after the merge and is
recorded in the done log.

## Unverified

The self-review step of the unattended protocol (running the `code-review`
skill on the branch diff) was not run: this session picked the branch up to
finish its verification after the original agent ran out of quota, and the
diff has had no second pair of eyes beyond the test suite.

The real-network test was exercised against `github.com` only. GitHub
Enterprise via `.fetch_with(..)` is designed for but untested.
