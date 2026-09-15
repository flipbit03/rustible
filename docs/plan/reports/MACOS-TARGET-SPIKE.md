# macOS as a *target*

Spike, 2026-09-12. Question: can Rustible manage macOS hosts, from a Linux
controller and from a macOS controller? Vision 5.3 defers macOS targets
("their own toolchain and SDK requirements") and `DECISIONS` [M7-macos]
records the local probe refusing `Darwin arm64` by name.

Everything below was run against a real machine: `macbook.local`, Apple
silicon (T6041), macOS 26.3 (build 25D125), Xcode Command Line Tools with
Apple clang 17, passwordless `sudo`, Remote Login on. The controller was the
x86_64 Linux dev box. Where a number is measured it says on what; where
something is inferred it says so.

**Status, 2026-09-13.** Sections 1-6 are the spike as it was run, and section
2's SDK recipe is **superseded**: Cadu rejected copying an Apple SDK off a mac,
the alternative was measured, and `docs/plan/M8.md` moved the whole build to
zig, which needs no SDK and deleted the toolchain matrix that section 2 was
working around. `darwin_env` and `SDKROOT` no longer exist in the tree. Section
10 (the ops, facts, gates and `brew`) stands unchanged and is built.
Findings 5.1, 5.2 and 5.4 are fixed; 5.3 and 5.5 are not, and say so.
---

## Verdict

**The toolchain objection is wrong, and the operations objection is right.**

Shipping a Rustible playbook to a mac and running it there works, today, in
209 lines of patch. A Linux box cross-compiled an arm64 Mach-O binary, shipped
it over the existing SSH transport, escalated through the mac's own `sudo`,
wrote root-owned files, did TLS from the target, streamed a file each way, and
reported `changed` then `ok` on a second run. A macOS controller does the same
with **zero** configuration, because there the host toolchain *is* the Apple
toolchain.

What does not work is `rustible-std`. Of the twelve operation modules, four
are Linux package/init/kernel managers that refuse a mac cleanly (good), and
three read `/etc/passwd` and `/etc/group`, which on macOS **exist but describe
nothing** — the real accounts live in Open Directory. Those three do not
refuse. They answer, and the answer is wrong: `user::Absent::new("cadu")`
reports `ok` on the machine where `cadu` is the logged-in account. `hostname::Is`
is worse: it reports `changed` after writing a file macOS ignores.

So the honest shape of the work is not "make the build work". It is: gate the
operations that cannot tell the truth on a mac, then write the Darwin ones.
Section 6 reads how Ansible did it; section 7 sizes it.

---

## 1. What was proven

| controller | target | transport | result |
|---|---|---|---|
| Linux x86_64 | macOS arm64 | SSH | `changed` → `ok` → `--check` clean |
| macOS arm64 | macOS arm64 | SSH | `changed` → `ok` |
| macOS arm64 | macOS arm64 | `connection="local"` | `changed` → `ok` |

Plus, not run but built: `x86_64-apple-darwin` cross-compiles from Linux, so
Intel macs are a target too. There is no Intel mac here to run it on.

The playbook is `examples/workspace/playbooks/mac.rs`, written to touch only
portable things, in the same spirit as `playbooks/vagrant.rs`: facts, a shell
command, a directory and a file as root, a line edit, and an HTTPS download
performed *by the target*. `playbooks/macdeep.rs` covers the runtime rather
than the ops: the escalation helper, channel streaming, the archive extractor
and `fetch`.

Second run against the mac, verbatim:

```
[macbook]    connected: aarch64-apple-darwin home /Users/cadu in 264.50ms
built mac for aarch64-apple-darwin in 95.83ms
[macbook]    binary already cached (2451008 bytes) in 46.20ms
[macbook]    Other("macos") on Aarch64, root=true
[macbook]  sw_vers .................................... changed         $ /usr/bin/sw_vers   action
[macbook]    ProductName: macOS | ProductVersion: 26.3 | BuildVersion: 25D125 |
[macbook]  spike directory ............................ ok
[macbook]  marker file ................................ ok
[macbook]  host line .................................. ok
[macbook]  fetch a known file over https .............. ok
[macbook]    796 bytes over TLS
total 462.95ms
```

`sw_vers` reports `changed` because `shell::Command` with no `creates`,
`removes` or `changed_when` always changes; that is the op behaving as
documented, not a Darwin problem.

---

## 2. The build, which is the part everyone assumes is impossible

Two things are needed to make a Mach-O binary on Linux, and only one of them
costs anything.

**The linker is free.** `rust-lld` ships inside every rustup toolchain and is
a multi-flavour driver — `ld64.lld` when asked. Nothing is installed:

```sh
CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=<sysroot>/lib/rustlib/<host>/bin/rust-lld
CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS=-Clinker-flavor=ld64.lld
```

It also **ad-hoc signs** the output, which matters: arm64 macOS refuses an
unsigned binary outright. `codesign -dv` on the binary that ran:

```
Format=Mach-O thin (arm64)
CodeDirectory v=20400 ... flags=0x20002(adhoc,linker-signed)
Signature=adhoc
```

**The SDK is the one real cost.** `-lSystem` needs Apple's `.tbd` link stubs,
and `ring` (the TLS provider, the project's one C dependency) includes
`TargetConditionals.h`. Apple's SDK is not redistributable, so Rustible cannot
carry it the way it carries musl's headers. The operator copies it once, from
a mac they own:

```sh
ssh <mac> 'cd $(xcrun --show-sdk-path) && tar czf - usr/lib usr/include' \
  | tar xzf - -C ~/.local/share/rustible/MacOSX.sdk
export SDKROOT=~/.local/share/rustible/MacOSX.sdk
```

14.4 MB over the wire, 94 MB unpacked, out of a 302 MB SDK. `SDKROOT` is then
read by *both* halves of the build with no further plumbing: `cc-rs` turns it
into `-isysroot` for ring, `rustc` into the linker's library search path. That
was checked by removing every other variable — `SDKROOT` plus `CC_<triple>`
plus the two linker variables is the complete set.

Both halves are genuinely required, and they fail at different times. Without
`usr/lib` the linker cannot resolve `-lSystem`; without `usr/include` ring
dies three screens into a build script with `'TargetConditionals.h' file not
found`. `darwin_env` checks for one file from each and refuses up front with
the copy command, rather than letting cc-rs explain it.

### Is this a violation of the dependency rule?

Judgement call, and I think no, with one caveat.

- **The target still installs nothing.** That is the half of the rule the
  project exists for, and it is untouched. macOS binaries link `libSystem`
  dynamically, but `libSystem` *is* the operating system — it is on every mac
  by definition, the way the kernel is.
- **The controller installs no program.** rustup and clang, exactly as today.
  No osxcross, no cctools, no zig, no docker. The Makefile, CI and the
  existing musl paths are untouched.
- **The caveat:** it adds a *file* the operator must obtain, and obtaining it
  requires access to a mac. That is qualitatively different from `apt install
  clang`, and it should be an explicit decision rather than something that
  slides in. It is also self-limiting: someone who wants to manage macOS hosts
  has a mac.

A macOS controller has none of this. `darwin_env` returns immediately on a
mac; `cc` links Mach-O and `xcrun` finds the SDK.

### Measurements

| | |
|---|---|
| cold build (ring, rustls, the lot) + ship + run, Linux → mac | **19.1 s** |
| warm run, Linux → mac | **0.46 s** |
| upload of the 2.45 MB binary over LAN SSH | 214 ms |
| probe + connect | 265 ms |
| `mac.rs` binary, `aarch64-apple-darwin`, `dist` profile | **2,451,008 B** |
| the same playbook, `aarch64-unknown-linux-musl` | 2,766,880 B |

The Darwin binary is *smaller* than the musl one, which is what dynamic
`libSystem` buys.

GNU `ar` produced a Mach-O static archive that `ld64.lld` accepted; no
`llvm-ar` was needed. That worked with the versions here and is the one place
I would pin something (`rustup component add llvm-tools` gives `llvm-ar`
inside the rule) rather than trust indefinitely.

---

## 3. The patch

209 lines added, 14 removed, two files.

**`transport.rs::triple_for`** — four lines of map. `uname -m` on a mac says
`arm64`, not `aarch64`:

```rust
"Darwin arm64"  => "aarch64-apple-darwin".into(),
"Darwin x86_64" => "x86_64-apple-darwin".into(),
```

**`toolchain.rs::darwin_env`** — the environment above, the SDK guard, and
`rust_lld()`, which finds the linker through `rustc --print sysroot` because
it is deliberately not on `PATH`.

Nothing else. Not the inventory, not the protocol, not the runtime, not the
`.cargo/config.toml` template (the linker goes in the environment, so the
generated workspace needs no Darwin stanza), not `Facts`, not the SDK crates,
not a single operation. `rustible-sdk` compiles for `aarch64-apple-darwin`
untouched — every backend primitive is plain POSIX.

`make` is green on Linux; `cargo test --workspace` and clippy are green on
macOS. Two new unit tests cover `darwin_env` on both platforms (nothing to
configure on a mac; linker + SDK elsewhere; an incomplete SDK refused).

`rustible toolchain check --target aarch64-apple-darwin` reports it. The
*default* target list is left as the two musl triples on purpose: adding
Darwin would make `toolchain check` fail on every machine without an SDK.
(`toolchain check` was later deleted altogether under M8, along with the SDK
path it reported on; only `toolchain install` remains.)

---

## 4. What works on a mac target today

Everything that is not a Linux administration tool.

| | |
|---|---|
| probe, build, upload, cache, exec | yes |
| SSH transport, ControlMaster | yes |
| `connection="local"` on a mac | yes |
| escalation, `escalate = true` (`sudo`) | yes |
| escalation helper, `ctx.as_root()` (`sudo -n -u root <bin> --helper`) | yes |
| `ctx.local_file` / `local_secret` streaming | yes (lands in the mac's per-user `$TMPDIR`) |
| `ctx.fetch` | yes |
| check mode and diffs | yes |
| `file::{Copy, Directory, Line, Block, Symlink, Attrs, Absent}` | yes |
| `shell::Command` | yes |
| `http::Download` (TLS from the target) | yes |
| `archive::Extracted` | yes (extracted a tarball the mac's own `tar` made) |
| `rustible-github` | untested, but it is `http` plus `authorized_keys` |

Sample from `macdeep.rs`:

```
[macbook]    HELPER commands run as cadu here and as root through the helper
[macbook]  root-owned line through the helper ......... changed  as root
[macbook]    STREAM hosts.kdl arrived at /var/folders/n6/…/T/.rustible-18d4a73…/1/hosts.kdl sha256 a4ad6cce…
[macbook]  extract it with rustible's own extractor ... changed  extract /tmp/spike.tar.gz (tar.gz: 1 files, 1 dirs, …)
[macbook]    ARCHIVE extracted: "hello"
[macbook]    FETCH pulled a.txt back to the workspace
```

---

## 5. What is broken, in order of danger

### 5.1 Ops that answer wrongly instead of refusing — the real finding

`/etc/passwd` and `/etc/group` exist on macOS. They are stubs. The file says
so itself:

```
# Note that this file is consulted directly only when the system is running
# in single-user mode.  At other times this information is provided by
# Open Directory.
```

`/etc/passwd` on this machine carries 132 entries and **not one login
account**: `root`, `daemon`, `nobody`, and 129 `_service` users. `/etc/group`
has a `staff:*:20:root` line while `id cadu` says `uid=501(cadu) gid=20(staff)`.
Rustible reads those files through `sys` and believes them — so it is right
about `root` and blind to every human on the machine, which is the worst
possible split for a spot check. Measured, on the machine where `cadu` is the account running
the playbook:

| op | said | truth |
|---|---|---|
| `user::Existing::named("cadu")` | **error**: "user `cadu` does not exist" | uid 501, home `/Users/cadu` |
| `user::Absent::new("cadu")` | **ok** — already absent | the logged-in user |
| `user::Membership` of `cadu` in `staff` | **error**: "user `cadu` does not exist" | it is their primary group |
| `ssh::authorized_keys::Present::for_user_name("cadu")` | **error**: "does not exist in /etc/passwd" | `~cadu/.ssh` is right there |
| `user::Present` (check phase) | plans to create, predicting home `/home/rustiblespike`, then fails on missing `useradd` | macOS homes are `/Users/<name>` |
| `group::Absent::new("staff")` | tries to run `groupdel` | fooled by the stub line |

The first four are the dangerous shape, and `user::Absent` is the worst of
them: it reports success for work it did not do and could not have done. This
is exactly the failure vision 6 forbids — "refuse, do not invent" — and the
ops are not at fault, because on Linux those files *are* the database.

**A refusal naming Open Directory is correct and cheap; a wrong answer is not
recoverable by the playbook author, who has no way to know.** But note where
the refusal belongs, because "bail the op on macOS" is too blunt by one step:

`ssh::authorized_keys` is **not** a Linux-only op. Only its name-lookup form
is. It offers `for_account(home, uid, gid)` and `for_user(&Account)`, both
documented "No lookup", and measured on the mac:

```
PROBE authorized_keys::for_user_name  => ERR: user `cadu` does not exist in /etc/passwd
PROBE authorized_keys::for_account    => OK/CHANGED     (+1 -0 lines)
PROBE authorized_keys::for_account (undo) => OK/CHANGED (+0 -1 lines)
```

`for_account("/Users/cadu", 501, 20)` wrote and then removed a key in
`/Users/cadu/.ssh/authorized_keys` correctly, today, with no patch. So the
answer "which platforms does this op support" is not a property of the *type*
`Present` — it is `Linux | macOS` for one constructor and `Linux` for the
other, and the difference is a field (`Target::Name` reads `/etc/passwd`,
`Target::Account` does not).

That is an argument for asking each op the question per *instance*, not for
moving the gate somewhere else; section 6 has the shape. What it is
definitely an argument against is a blanket per-op "Linux only", which would
delete working functionality here.

> **Fixed (section 10).** All six `user`/`group` ops refuse a mac by name, and
> `ssh::authorized_keys` refuses only its `/etc/passwd` lookup —
> `for_account` still works, with a tier-2 test pinning it.

### 5.2 `hostname::Is` and `sysctl::Present` report `changed` for work they did not do

It created `/etc/hostname` — a file macOS ignores entirely — and ran
`hostname <name>`, which sets the running kernel name only. The three names
macOS actually keeps were untouched:

```
scutil --get HostName       MACBOOK
scutil --get LocalHostName  MACBOOK
scutil --get ComputerName   MACBOOK
```

And the next run reports `ok`, because `/etc/hostname` now matches. A silent
no-op that converges to green. (The file and the running name were restored
afterwards.)

**`sysctl::Present` is the same defect, currently masked.** In section 5's
first probe it refused with "`/etc/sysctl.d` does not exist" — which reads
like a gate and is not one. It is an accident of that directory being absent.
Give it the directory and turn off the live write it also complains about, and
it is happy:

```
PROBE file::Directory /etc/sysctl.d  => OK/CHANGED
PROBE sysctl apply_now(false)        => OK/CHANGED   /etc/sysctl.d/99-rustible.conf=49152
```

```
macbook$ cat /etc/sysctl.d/99-rustible.conf
kern.maxfiles = 49152
```

macOS has never read that path in its life. `changed` reported, nothing done,
and the next run says `ok`. This is the argument against auditing op by op:
`sysctl` was not on the suspect list until the accidental refusal was removed.
**The set to gate is "every place that reads a Linux file", and it is found by
grepping for the file, not by reviewing ops.** (Removed afterwards.)

Being on this list is not a verdict on the op. `sysctl::Present` is also the
*easiest* op here to give a correct Darwin arm — macOS has a real
`/etc/sysctl.conf` — and section 6 argues it should get one. It lies today
precisely because it has no such arm; the two readings are the same fact.

> **Fixed (section 10).** Both refuse a mac by name. The `sysctl` test plants
> `/etc/sysctl.d` first, so the gate is proved to hold where the accidental
> refusal used to be all there was. Neither has a Darwin arm yet.

### 5.3 Cancellation does nothing on macOS

`transport.rs::kill_script` filters `pgrep -f` candidates through
`/proc/<pid>/exe`, which is the right call on Linux — a command line is text
anything can carry, `/proc` is the kernel's answer. macOS has no `/proc`, so
`readlink` fails, the guard's `|| continue` skips **every** candidate, and the
script kills nothing.

Demonstrated rather than argued. With a playbook binary mid-`sleep 30` on the
mac, the exact script was run by hand:

```
kill script ran
--- still alive?
56277  ?? Ss  /Users/cadu/.cache/rustible/bin/macslow-3a1b913… --remote
```

…and the run went on to execute the step that must never run after `Cancel`,
creating `/tmp/rustible-cancel-next-step-ran`.

Under an actual ctrl-c the binary *did* die and the next step did not run —
but that is the SSH session teardown, which `transport.rs`'s own comment
already calls "luck rather than cancellation". The fix is small: `ps -o
comm=`, or `lsof -p`, behind an `if [ -d /proc ]`.

### 5.4 `Facts` is nearly empty on a mac

`Facts::gather` reads `/etc/os-release`, `/proc/sys/kernel/osrelease`,
`/proc/sys/kernel/hostname`, `/proc/cpuinfo`, `/proc/meminfo` and
`/proc/1/comm`. None exist. Measured:

```
os=Other("macos") distro=Other("") ver="" arch=Aarch64 kernel="" hostname=""
pm=Other("") init=Other("") cpus=1 mem=0MB user="root" root=true
```

It degrades quietly rather than failing, and `os` and `arch` are right because
they are compiled in. But `hostname` is empty, `cpus` is a lie (the 1 is the
documented fallback), and `distro`/`pm`/`init` carry no information. Every
field has a cheap Darwin source: `sysctl kern.osrelease / hw.ncpu /
hw.memsize`, `/System/Library/CoreServices/SystemVersion.plist` for the
product version, `/sbin/launchd` for init, `/opt/homebrew/bin/brew` for the
package manager. Note that `Facts::gather` currently reads only through
`Backend::read`/`stat` and runs no commands — the plist and the `has()` probes
fit that; `sysctl` does not, so `cpus` and `memory_mb` need either a small
`Backend` widening or an honest `None`.

> **Fixed (section 10).** All ten fields are now correct on a mac. Six came
> from reads and stats; the other four from one `sysctl -n` spawn, macOS only.
> Measured on `macbook`:
>
> ```
> FACTS os=Macos distro=Macos ver=26.3 kernel=25.3.0 host=MACBOOK
>       pm={Brew} init=Launchd cpus=12 mem=49152MB
> ```

`Pm` and `Init` would want `Brew` and `Launchd` variants — as enum variants,
added deliberately. (`DECISIONS` line 322 records me inventing `Os::Macos`,
`Pm::Yum` and `Init::SysV` from memory once. `Os::Other("macos")` is what
exists, and it is what the spike playbook matches on.)

### 5.5 Two macOS facts of life that did not bite here

- **TCC / Full Disk Access.** `sshd` is not granted access to `~/Documents`,
  `~/Desktop`, `~/Downloads` or `~/Library/Mail` by default, and a process it
  spawned inherits that, root or not. It did not bite on this machine —
  writes to all four succeeded — because this mac already grants sshd Full
  Disk Access (`sqlite3 ~/Library/Messages/chat.db` works over SSH). On a
  default mac, expect `Operation not permitted` from paths that look
  perfectly ordinary. Worth a named refusal rather than a raw `EPERM`.
- **Gatekeeper.** The binary arrives by SSH, so it carries no
  `com.apple.quarantine` xattr and the ad-hoc linker signature is enough. A
  managed fleet with a stricter policy is a different matter, untested.

---

## 6. How Ansible solves this, read from its source

Asked directly, and worth answering from the code rather than from memory.
Read at `ansible` 2.x, `…/site-packages/ansible/`.

Ansible has **three** mechanisms and picks between them per operation. It does
not have one answer.

**(a) One module, a subclass per platform.** `module_utils/common/sys_info.py`
exposes `get_platform_subclass(cls)`, which walks `cls`'s subclasses matching
on two class attributes, `platform` (from `platform.system()`) and
`distribution`, most specific first. The module's base class then does:

```python
class User(object):
    platform = 'Generic'
    distribution = None
    def __new__(cls, *args, **kwargs):
        new_cls = get_platform_subclass(User)
        return super(cls, new_cls).__new__(new_cls)
```

`modules/user.py` is **3,553 lines** of it: `User` 858, then `FreeBsdUser`
295, `SunOS` 302, **`DarwinUser` 359**, `AIX` 216, `HPUX` 145, `OpenBSDUser`
189, `NetBSDUser` 180, `BusyBox` 224 with `Alpine` and `Buildroot` bound to it.
`modules/group.py` has a `DarwinGroup`. `DarwinUser`'s own docstring is the
whole problem statement:

> Main differences are that Darwin:- Handles accounts in a database managed by
> `dscl(1)` - Has no `useradd`/`groupadd` - Does not create home directories -
> User password must be cleartext - UID must be given - System users must be
> under 500

**(b) One module, a *strategy* per platform.** `modules/hostname.py` is 891
lines: about fourteen strategy classes (`FileStrategy`, `SystemdStrategy`,
`RedHatStrategy`, `OpenRCStrategy`, `DarwinStrategy`, …) and roughly sixty
near-empty `Hostname` subclasses whose whole body binds a distribution to a
strategy. `DarwinStrategy` drives `scutil` over all three names macOS keeps,
and carries a `_scrub_hostname()` that reimplements the transformation the
Sharing preference pane applies to derive `LocalHostName`. 891 lines, to set a
hostname.

**(c) A different module entirely.** `ansible.builtin.service` has classes for
Linux, GNU/Hurd, FreeBSD, DragonFly, OpenBSD, NetBSD, SunOS and AIX — and
**none for Darwin**. launchd is not in `ansible.builtin` at all; it is
`community.general.launchd`, alongside `homebrew`, `homebrew_cask`,
`homebrew_services`, `homebrew_tap`, `macports`, `osx_defaults` and `pkgutil`.
Where the concepts do not line up, Ansible did not force them into one module.
It wrote new ones and named them after the tool.

Above all three sits a **facade that dispatches on a fact**:
`plugins/action/package.py` reads `ansible_facts['pkg_mgr']` and executes a
*different module* — `apt`, `dnf`, `homebrew` — gathering the fact first if it
is missing. It is a typed redirect, not a branch inside one implementation.

**Facts are the same story, one file per platform.**
`module_utils/facts/{hardware,network,virtual,system}/` holds `linux.py`,
`darwin.py`, `freebsd.py`, `sunos.py`, `aix.py`, …, each a subclass carrying
`platform = 'Darwin'` and selected the same way. `DarwinHardware` gets
processor, cores, memory and uptime from `sysctl` and `system_profiler`.
`system/pkg_mgr.py` probes `/opt/homebrew/bin/brew`, `/usr/local/bin/brew` and
`/opt/local/bin/port` by path — exactly the shape of Rustible's `Pm` probe,
just with the mac entries present. `system/service_mgr.py` returns `launchd`.

### The one line that matters

`modules/service.py`, base class:

```python
def get_service_tools(self):
    self.module.fail_json(msg="get_service_tools not implemented on target platform")
def service_enable(self):
    self.module.fail_json(msg="service_enable not implemented on target platform")
```

and `hostname.py` has an entire `UnimplementedStrategy` whose every method
raises.

**Ansible's generic base refuses, and a platform opts in by subclassing.
Rustible's generic base *is Linux*, and a platform opts out by not existing.**

That inversion is the whole of section 5.1. It also explains why the Rustible
ops that behave correctly on a mac do so: `apt`, `systemd` and `sysctl` check
`Facts::pm` / `Facts::init` / a path before acting, so an empty fact refused
them. The ops that misbehave — `user`, `group`, `authorized_keys`, `hostname`
— consult no fact at all, because on Linux they do not need to: their
implementation *is* reading a file, and that file exists on macOS too and
means something else. **A missing tool announces itself; a misleading file
does not.** That is the selection rule for which ops are dangerous, and it is
worth applying to any future platform, not just this one.

### The rule: one op or two?

Take the *choice*, not one mechanism — and the choice has a test. **Branch
inside one op if and only if all three of these hold:**

1. **Same sentence.** The desired state reads identically on both platforms.
2. **Same inputs.** One builder serves both, with no platform-only options,
   *and each argument means the same thing on both*.
3. **Same output.** The typed output can be filled honestly on both.

The implementation differing is not a reason to split — that is only code.
Conditions 2 and 3 decide, because they *are* the type, and the type is what
Rustible has over YAML. A divergence hidden inside `check` is a divergence the
compiler stopped policing.

Applied to what is actually on the machine:

| op | call | why |
|---|---|---|
| `file::*`, `shell`, `http`, `archive` | **already one** | portable POSIX; nothing to decide |
| `sysctl::Present` | **branch** | the cleanest candidate of the lot — see below |
| `group::Present` | **branch** | `Group { name, gid, members }` fills honestly from `dscl` |
| `hostname::Is` | **branch**, with a wrinkle | macOS keeps three names; condition 2 is borderline |
| `user::Present` | **split, or a very constrained branch** | condition 2 fails hard |
| `apt` / `brew` | **split** | already the house style |
| `systemd` / `launchd` | **split** | condition 2, not "different concept" |

**`sysctl::Present` is the best branch candidate, and section 5.2 put it on
the danger list.** Both are true and they are the same fact: it lies today
*because* it has no Darwin arm, and it deserves one more than any other op
here. macOS has a real, documented `/etc/sysctl.conf`, from `man 5
sysctl.conf` on the machine:

> The `/etc/sysctl.conf` file is read in when the system goes into multi-user
> mode to set default settings for the kernel.

Same sentence, same inputs, same output. Exactly two things differ: the
persistence file, and the live read (`/proc/sys/<key>` against `sysctl -n
<key>`). Nothing about the type moves.

**`user::Present` fails condition 2, which is the interesting failure.** The
implementation differing would be fine. The *builder* differing is not. From
`DarwinUser`'s docstring and `dscl` on this machine: UID must be given (no
auto-allocation), home directories are never created, the password is
cleartext rather than a hash, system users are under 500 rather than 1000, and
`IsHidden` has no Linux analogue. So

```rust
user::Present::new("x").password_hash(..).create_home(true)
```

typechecks and means nothing on a mac. That is the YAML failure mode with
extra steps: an error that used to be a compile error becomes a 2am error.

**`systemd` / `launchd` splits on condition 2, and my first reasoning for it
was wrong.** The verbs map better than "different concept" suggests —
`launchctl` offers `enable`, `disable`, `bootstrap`, `bootout` and
`kickstart`, checked on the machine, which covers `Enabled`/`Disabled`/
`Running`/`Stopped`/`Restart` without strain. What does not map is the
identifier: `systemd::Enabled::new("sshd")` against `launchctl print
system/com.openssh.sshd`. Two different grammars in the same `String`, so a
playbook naming a Linux unit typechecks and fails at run time. **The type is
what documents what the string is allowed to say**, and that is the reason to
keep two of them.

### However you answer it, the op declares where it runs

This is the part to copy verbatim from Ansible, and it is independent of
everything above. The shape that fits Rustible is a declaration on the `Op`
trait, beside `always_changes`:

```rust
/// The platforms this op has an implementation for. The default is Linux
/// alone: an op says nothing and is refused everywhere else, rather than
/// being run on a machine whose files it misreads.
fn platforms(&self) -> &[Os] {
    &[Os::Linux]
}
```

Three properties earn it over an `ensure!` inside each `check`:

- **It takes `&self`, so the answer can depend on the instance.** That is what
  `ssh::authorized_keys` needs (5.1): `Present` returns `&[Os::Linux,
  Os::Macos]` when its target is an `Account` and `&[Os::Linux]` when it is a
  name to look up in `/etc/passwd`. One op, one method, both answers correct.
  No separate gating layer, and nothing portable is lost.
- **`Ctx::step` enforces it before `check` runs**, so it is not something an
  op author can forget halfway down a function, and the refusal names the op,
  the platform and the reason rather than surfacing as a file that did not
  parse.
- **The default is the safe direction.** Today an op that says nothing runs
  everywhere and assumes Linux; with this, an op that says nothing runs on
  Linux and refuses elsewhere. That single inversion is the fix for section
  5 — and it is exactly what Ansible's `platform = 'Generic'` base does by
  making every method `fail_json`. Portable ops (`file::*`, `shell`, `http`,
  `archive`) opt out in one line, which is a line worth writing because it is
  a claim someone checked.

An op with one platform is then the same code as an op with two, minus a match
arm, so writing the declaration now costs nothing later — it *is* the skeleton
a branch fills in.

Do the branch itself as an internal trait rather than an `if`: `trait
UserDirectory` with `Passwd` and `OpenDirectory` implementations, selected in
`check` from `sys.facts().os`. That keeps the *planning* as pure functions
over text, per platform, and so at tier 1. It matters more here than it looks:
tier 3 cannot run macOS at all, and the `Fake` models commands badly by
design, so pure functions over `dscl`'s **output text** are the only Darwin
coverage available without a mac in the loop. It is also the one thing
Ansible's subclass-per-platform shape cannot do.

One coupling to write down rather than discover: `authorized_keys::
for_user_name` is a hidden call into `user`'s `/etc/passwd` parse
(`crate::user::lookup_user`). Its platform answer has to track
`user::Existing`'s, because it *is* `user::Existing` inlined. The explicit
composition — `user::Existing::named(..)` then `for_user(&account)` — already
exists and is the honest form.

### What not to copy

The facade. `package:` and `service:` exist because Ansible's unit of reuse is
a YAML task that has to be written once for a mixed fleet. Rustible's is a
Rust function taking `&mut Ctx`, so a mixed fleet is a `match
ctx.facts().os { … }` in one place the author wrote and can read — typed,
greppable, and unable to silently pick a manager the author did not intend.
Ansible needs the indirection because it has no `match`.

### Where this collides with Rustible's own rules

Three genuine costs, all of them documented constraints rather than surprises:

- **`check` does all the thinking; `apply` executes the diff.** On Linux
  `user::Present` diffs `/etc/passwd` *text*, which is why its `Fake` tests
  work. Darwin has no such file: state comes from `dscl -read`, so `check`
  becomes command-shaped — and `CLAUDE.md` is explicit that the `Fake` models
  commands badly (`with_cmd` consumes `self`, `spawn` returns the first
  match, so an op reading its state with a command cannot express
  changed-then-ok at tier 2 at all). A Darwin `user` op therefore has worse
  tier-2 coverage than the Linux one, by construction.
- **There is no macOS container**, so tier 3 cannot cover Darwin ops at all.
  The coverage has to come from tier 4, and the cheapest form is CI's macOS
  runner driving `connection="local"` against itself — proven in section 1,
  and the natural shape of a `Test: macOS target (local)` job.
- **`Facts` is a fixed struct of non-optional fields**, so a field macOS
  cannot answer has to lie: `cpus=1`, `memory_mb=0`. Ansible's facts are a
  dict, where absent means absent. Rustible has to choose — per-platform
  gatherers filling the same struct, or `Option` on the fields that are
  genuinely unknowable. The first keeps every call site; the second is
  honest. They are not exclusive: most fields have a Darwin source
  (`SystemVersion.plist`, `/sbin/launchd`, `/opt/homebrew/bin/brew` are all
  reads and stats, which is all `Facts::gather` is allowed today), and only
  `cpus` and `memory_mb` need a command and so need a decision.

And one type-level note: `Os` is the dispatch key for all of this, and it is
currently `Os::Other(String)` for everything that is not Linux — a stringly
key for the most important branch in the system. If macOS ships, `Os::Macos`
should become a real variant. (`DECISIONS` line 322 records me *inventing*
that variant once from memory, which is why it is proposed here explicitly
rather than assumed.)

---

## 7. What "macOS support" would actually cost

Three tiers, and they are separable — each is useful on its own.

**Tier A — targeting works, ops refuse honestly. ~1 day.**
The 209-line patch, plus `Op::platforms()` defaulting to Linux alone
(section 6), enforced in `Ctx::step`: one line on each portable op to opt out,
nothing on the Linux-only ones, and a per-instance answer for
`ssh::authorized_keys` so `for_account` keeps working. Plus the cancellation
fix in 5.3. Result: a mac is a first-class target for the file, shell, http
and archive ops, and everything else says "not on macOS" instead of lying. This is the part with a
real payoff-to-risk ratio, and the gate is worth doing **whether or not**
macOS targets ship — those ops are wrong on a mac today the moment anyone
points Rustible at one.

**Tier B — Darwin facts, the cheap branches, and a package manager. ~2–3 days.**
`Facts` from `sysctl`/plist/launchd, `Pm::Brew`, `Init::Launchd`, and a
`brew::{Present, Absent, Latest}` op. Homebrew is the mac's apt, it is where
essentially all of the interesting management lives, and it is a
well-behaved CLI to drive. Take `sysctl::Present` and `group::Present` here
too: both pass all three conditions in section 6, so each is a second arm
inside an existing op rather than a new type, and `sysctl` needs only
`/etc/sysctl.conf` and `sysctl -n` in place of `/etc/sysctl.d` and
`/proc/sys`.

**Tier C — the identity and service ops. ~1 week+, and the risky one.**
(Section 6 has Ansible's measured version of this bill: `DarwinUser` is 359
lines beside an 858-line base, and `hostname.py` is 891 lines in total.)
`user` against `dscl` and `sysadminctl`, a separate `launchd::{Enabled,
Disabled, Running, Stopped, Restart}` family against `launchctl` — separate
because the *identifier* grammar differs, not the verbs, which map cleanly
(section 6) — and `hostname` against `scutil`'s three names. `dscl` is the
unpleasant part: no `/etc/passwd` to diff, so
`check` has to compose its plan from `dscl -read` output, which is the thing
the architecture most wants to avoid ("the Fake models files well and commands
badly" — the tier-2 story for these ops would be poor). `user::Present` on
macOS also has to decide questions Linux does not pose: is this a hidden
service account or a login user, does it appear in the login window, is it an
admin.

**Testing.** Tiers 1 and 2 come free. Tier 3 does not: there is no macOS
container, so the container tier cannot cover Darwin ops at all. Tier 4 needs
a mac, and CI's macOS runner can drive `connection="local"` against itself —
which is already proven here, and would be the natural shape of a
`Test: macOS target (local)` job. Cross-building a Darwin binary in the Linux
CI jobs cannot happen at all without an SDK in the runner, which is not
something to put in a public repository.

---

## 8. Recommendation

Do Tier A. Keep macOS targets out of the vision's supported set until Tier B
lands, but **fix the lying ops now**.

Cadu settled this in the spike session, and as a rule wider than macOS: an
operation is gated by the platform it *can* run on, not by the platforms
someone remembered to exclude. Support being Linux-only is fine; an op
succeeding wrongly — a file left somewhere nothing reads — or crashing
arbitrarily is not. That is an allowlist, and it holds whether or not macOS is
ever a target.

The defect is latent on `main` rather than reachable — `triple_for` refuses
`Darwin arm64`, so nobody can point Rustible at a mac at all — but that
refusal is the *only* thing standing between a user and `user::Absent`
reporting `ok` about their own account, and a probe is a thin thing to be
relying on.

The declaration goes on the op, as `Op::platforms(&self)` (section 6). Taking
`&self` is what makes that fine-grained enough: `ssh::authorized_keys` answers
`Linux | macOS` for `for_account` and `Linux` for the `/etc/passwd` lookup, so
nothing portable is lost and no second gating layer is needed. Defaulting it
to Linux alone is the whole fix, because it inverts which way an op fails when
its author said nothing.

Tier C is a project, not a task, and it should wait for a reason: an actual
mac in the dogfood fleet with actual work to do on it. `my_infra` has one
(vision 6.9 lists "a macOS laptop" among the nine hosts and puts it out of MVP
scope). Porting that host's playbook is the test that tells you which Darwin
ops are real and which are imagined.

---

## 9. Amendments

Not applied. Per `CLAUDE.md`, the vision is not edited on a passer-by's
initiative, and the `DECISIONS` entries are written out here for the author to
place.

The first `DECISIONS` entry below is **decided**: Cadu settled the gating rule
in the spike session. The vision amendment and the second entry remain
proposals.

**Vision 5.3**, replacing the line "macOS and Windows targets are deferred.
They have their own toolchain and SDK requirements.":

> Windows targets are deferred. **macOS targets are a measured possibility,
> not a supported platform.** `docs/plan/reports/MACOS-TARGET-SPIKE.md`
> demonstrates the whole pipeline working against a real mac from both a Linux
> and a macOS controller: the Mach-O linker is the `rust-lld` rustup already
> ships, and the only addition is a macOS SDK the operator copies from a mac
> they own, named by `SDKROOT`, which Rustible cannot carry because Apple's
> SDK is not redistributable. The target still installs nothing. What is
> missing is the operations: `apt`, `systemd` and `/etc/passwd` are not the
> mac's, and the ops that read `/etc/passwd` answer *wrongly* there rather
> than refusing, because on macOS that file exists and describes nothing.

**`docs/plan/DECISIONS.md`**, two entries:

> - [SPIKE-macos] 2026-09-12 **An operation is gated by the platform it can
>   run on, not by the platforms someone remembered to exclude.** A
>   `platforms(&self) -> &[Os]` declaration on the `Op` trait, **defaulting to
>   Linux alone** and enforced by `Ctx::step` before `check` runs, so an op
>   whose author said nothing refuses off Linux instead of running there and
>   misreading its files. `&self` rather than an associated constant, because
>   the answer can depend on the instance: `ssh::authorized_keys::Present` is
>   `Linux | macOS` when built with `for_account`/`for_user` and `Linux` when
>   built with `for_user_name`, which reads `/etc/passwd` — measured working
>   on macOS 26.3 in the first form, so a blanket per-op gate would have
>   deleted it. Support being Linux-only is fine; an op succeeding *wrongly*
>   is not. Measured on macOS 26.3: `/etc/passwd` there
>   is a stub — Open Directory holds the accounts — so `user::Absent` reports
>   `ok` for the logged-in user and `user::Existing` reports that they do not
>   exist; `hostname::Is` and `sysctl::Present` report `changed` after writing
>   files macOS never reads. This is "refuse, do not invent" failing in the
>   one place the code cannot see it: a missing tool announces itself, a
>   misleading file does not. Latent on `main`, since `triple_for` refuses
>   Darwin, but a probe is a thin thing to be relying on. The declaration is
>   also the skeleton of a later Darwin arm, so it costs nothing twice.
>   Reverse: drop the method and accept wrong answers on any non-Linux host.
> - [SPIKE-macos] 2026-09-12 **`transport.rs::kill_script` is a no-op on any
>   host without `/proc`.** Its `/proc/<pid>/exe` guard is right on Linux and
>   skips every candidate elsewhere, so cancellation falls back to the SSH
>   teardown, which the module's own comment calls luck. Demonstrated against
>   a mac: the script ran, killed nothing, and the run executed the step that
>   must never run after `Cancel`. Reverse: none wanted; the fix is `ps -o
>   comm=` behind an `if [ -d /proc ]`.

---

## 10. What was built

Scope closed by Cadu in the spike session; this is what landed on the branch.
The governing rule, in his words: **an operation is gated by the platform it
*can* run on, not by the platforms someone remembered to exclude.** Support
being Linux-only is fine; an op succeeding wrongly is not.

### 10.1 Facts

`Os::Macos` and `Distro::Macos` are real variants, not `Other(String)`, so the
dispatch key for every gate is a type rather than a string comparison. `Os` also
gained `name()`, because `{:?}` prints `Other("freebsd")` and a refusal should not.

`Facts::package_manager: Pm` became **`package_managers: BTreeSet<Pm>`**, asked
with `has_pm(&Pm::Apt)`. The single value was a ranking nothing needed: Rustible
has no `package::Present` facade, so what an op wants is "is apt here", not "which
manager won". It was also about to become wrong — Homebrew runs on Linux, so a
Debian box with `/home/linuxbrew` would have reported `Pm::Apt` and told
`brew::Present` it had no brew. `Pm::Other` is gone; no manager is the empty set.
`Pm::Brew` (three paths) and `Init::Launchd` are new.

The four fields with no file behind them on macOS come from one spawn,
`sysctl -n hw.ncpu hw.memsize kern.osrelease kern.hostname`, macOS only —
Linux keeps its reads and pays nothing. **This needs a vision amendment**
(10.5): facts are currently specified as coming from reads alone.

`PROTOCOL_VERSION` 3 → 4, because `Facts` rides in `Event::Facts`.

### 10.2 The gate, on all 31 operations

| | how |
|---|---|
| `user` ×4, `group` ×2 | `require_passwd_db` at the top of `check` — one helper, six call sites, naming Open Directory |
| `ssh::authorized_keys` ×2 | inside `Target::User`'s arm only, so `for_account` is untouched and the message names it |
| `hostname::Is`, `sysctl::Present` | their own match, each naming what macOS uses instead (`scutil`'s three names, `/etc/sysctl.conf`) |
| `apt` ×3, `systemd` ×7 | an `Os` match **added alongside** the existing `Pm`/`Init` capability check |
| `file::*` ×7, `shell`, `http`, `archive`, `github::UserKeys` | an explicit `Os::Linux \| Os::Macos` match each — the supported set written down rather than inferred from the absence of a check |

`group::Tools::of`'s `_ => Tools::Shadow` fallback — the five lines that sent
`user::Present` looking for `useradd` on a mac — is now an explicit per-OS match.

### 10.3 `brew`

`brew::{Present, Absent}`, shaped like `apt`: `check` decides from
`brew list --formula --versions` and writes the decision into the diff,
`apply` reads it back out and executes exactly that.

Two things are inverted from every other package op, both Homebrew's doing:

- **It refuses root.** Verified rather than assumed — `sudo brew install`
  answers *"Running Homebrew as root is extremely dangerous and no longer
  supported."* So these ops require **not** being root, and the refusal quotes
  that and points at `ctx.as_user(..)`.
- **It is not a platform.** They gate on `has_pm(&Pm::Brew)`, never on
  `Os::Macos`, so Linuxbrew is served. There is a tier-2 test for that.
- The binary is found by probing the three install paths, not through `PATH`:
  the playbook binary runs with whatever environment `sshd` gave it.

### 10.4 Verified

`make` green on Linux. Existing tests: **no assertion changed**, with one
exception worth naming — the three tier-3 `assert_eq!(facts.package_manager,
Pm::Apt)` became `assert!(facts.has_pm(&Pm::Apt))`, the same claim against the
new shape. Everything else was mechanical field renaming that the compiler
found.

On `macbook`, `playbooks/macbrew.rs`, unescalated:

```
facts: Macos 26.3 Aarch64 {Brew} cpus=12 mem=49152MB user=cadu
ninvaders present ....... changed    ninvaders: absent -> installed
                                     installed [Formula { name: "ninvaders", version: "0.1.1" }]
ninvaders present ....... ok         already there [Formula { name: "ninvaders", version: "0.1.1" }]
--check ................. ok
--var present=false ..... changed    ninvaders: installed 0.1.1 -> absent
--var present=false ..... ok
```

And every gate, against the same machine:

```
user::Existing        => ERR: ... accounts live in Open Directory ... drive `dscl` through shell::Command
user::Absent          => ERR: ... a real account would be reported absent ...
group::Absent         => ERR: ... /etc/passwd and /etc/group ... not this host's account database
hostname::Is          => ERR: ... macOS ... holds three separate names through scutil ...
sysctl::Present       => ERR: ... macOS ... reads /etc/sysctl.conf ... a file nothing reads
systemd::Enabled      => ERR: systemd::Enabled manages systemd units and runs on Linux only; this host is macos
authorized_keys(name) => ERR: ... Pass the account directly with `for_account(home, uid, gid)`, which works here
```

### 10.5 Still open

- **The vision amendment for the facts spawn.** Vision's facts section says
  they are gathered "from a handful of reads"; one of them is now a command,
  on macOS only. Prose is Cadu's to write or approve; not edited here.
- **5.3, cancellation.** `kill_script`'s `/proc` guard is still a no-op off
  Linux. Deliberately out of the closed scope.
- **5.5, TCC.** Untested beyond this machine, which grants `sshd` Full Disk
  Access.
- **No Darwin *implementations*.** `sysctl` and `group` are the two that pass
  all three of section 6's conditions and would be cheap second arms. `user`
  fails condition 2 and is a project.

---

## Appendix A — reproducing it

As the tree stands after M8 — no SDK, nothing to copy; `rustible` fetches zig
on the first build:

```sh
cargo install --path crates/rustible-cli

cat > /tmp/hosts.mac.kdl <<'KDL'
defaults escalate="sudo"
group "mac" {
    host "macbook" addr="macbook.example" ssh_user="youruser"
}
KDL

rustible --workspace examples/workspace --inventory /tmp/hosts.mac.kdl \
         playbook run mac -v
rustible --workspace examples/workspace --inventory /tmp/hosts.mac.kdl \
         playbook run macdeep
rustible --workspace examples/workspace --inventory /tmp/hosts.mac.kdl \
         playbook run macbrew
```

The same commands work from a mac. (The spike as originally run needed an
Apple SDK copied off a mac and named by `SDKROOT`; that recipe is gone with
the code that read it, see the status note at the top.)

## Appendix B — what was installed

**Linux controller:** nothing. `clang` (already required by the dependency
rule) and `rsync` were present; the linker is rustup's own `rust-lld`. Two
rustup *targets* were added, which is `rustup target add` and therefore inside
the rule — and in a real run `Cargo::build` adds them itself:

```
rustup target add aarch64-apple-darwin   # was already installed here
rustup target add x86_64-apple-darwin    # added by this spike
```

Plus the SDK copy at `~/.local/share/rustible/MacOSX.sdk` (94 MB), which is
data, not a program.

**The mac:** nothing. It already had rustup (rustc 1.95.0, the project's MSRV)
and the Command Line Tools. The spike left a checkout at `~/rustible-spike`
and a `rustible` binary in `~/.cargo/bin`; both are disposable.

**Cleaned up afterwards on the mac:** `/etc/rustible-spike*`, the
`/etc/hostname` that `hostname::Is` created, `/tmp/spike*`, and the kernel
hostname, which was restored to `MACBOOK`. `~/.cache/rustible` (18 MB of
shipped binaries) was left, since that is Rustible's normal cache.
