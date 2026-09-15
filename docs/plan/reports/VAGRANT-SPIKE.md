# Vagrant as the machine tier

Spike, 2026-09-09. Asked for on PR #29: replace the two personal VMs the
project had been tested against with machines that live in the repository, on
both architectures, on both platforms this is developed on.

Everything below was run. Where a number is measured it says on what; where
something is inferred it says so.

---

## Verdict

**It works on all four combinations, and the machine tier is now `make vm-test`.**
Two Debian 12 guests, `x86` and `arm`, from one multi-architecture box, driven
by vagrant-libvirt on Linux and vagrant-qemu on macOS. Both architectures come
up on both platforms; the cross-architecture pairs are emulated and slow, and
they work.

The spike found one bug that had nothing to do with Rustible and would have
silently wasted a developer's afternoon: **vagrant-libvirt collides
multi-architecture boxes in libvirt's storage pool**, so the second
architecture you bring up boots the first one's disk with no error anywhere.
Section 3.

---

## 1. What was proven

| controller | guest | provider | acceleration | `vagrant up` from destroyed | playbook |
|---|---|---|---|---|---|
| Linux x86_64 (no `/dev/kvm`) | x86_64 | libvirt | TCG | 63 s | 3 changed, then 4 ok |
| Linux x86_64 (no `/dev/kvm`) | aarch64 | libvirt | TCG | 103 s | 2 changed, then 4 ok |
| macOS aarch64 | aarch64 | qemu | HVF | 17 s | 3 changed, then 4 ok |
| macOS aarch64 | x86_64 | qemu | TCG | 37 s | 3 changed, then 4 ok |

Two things in that table are worth saying out loud, because both contradict
what this spike assumed going in.

**Cross-architecture emulation is not "minutes".** The worst cell is 103
seconds, and that one includes a 415 MB box upload. The Vagrantfile used to
warn that a cross-architecture boot "takes many minutes"; that text came from
the two 15- and 20-minute hangs in §3, which were not emulation at all but the
guest booting the wrong architecture's disk. The warning was measuring a bug.
It now says 40 s to 2 min.

**The host matters more than the emulation.** The emulated x86_64 guest on
Apple silicon (37 s) beats the *native*-architecture guest on the Linux host
(63 s), because that Linux host is itself a Hyper-V guest with no nested
virtualisation and is interpreting too, on slower hardware.

That Linux host is the pessimistic case, and it is usable.

The playbook is `examples/workspace/playbooks/vagrant.rs`, chosen to exercise
the four things a machine buys over the container harness:

- **apt over the real SSH transport**, not `docker exec`.
- **a file written as root through real `sudo`**, not a container's uid 0.
- **a live `/proc/sys` write** (`vm.swappiness` 60 → 61). A container shares
  the host kernel: this is either refused or it changes the *host*.
- **systemd** (`systemd-timesyncd` enabled). A container has no pid 1 to ask.

Verified inside the x86 guest by hand: `/proc/sys/vm/swappiness` reads 61,
`/etc/sysctl.d/99-rustible.conf` is on disk, `mc` is at `/usr/bin/mc`, and the
timer unit is enabled.

Each Linux row was measured with only that machine created, for the reason in
§3. The macOS rows were measured with both machines up at once.

Every row was also re-run after undoing the playbook's changes in the guest
(`rm /etc/rustible-vagrant`, `sysctl vm.swappiness=60`), so the `changed` then
`ok` pair is a real convergence and not a first-boot artifact.

## 2. Shape

One `Vagrantfile` with two machines. It detects the host's OS and architecture
and whether it has hardware virtualisation (`/dev/kvm` on Linux,
`kern.hv_support` on macOS), picks libvirt or qemu accordingly, and **prints
which of the four rows above you are about to get before it starts**. A slow
boot should be a number you were told, not a mysterious hang.

Both providers consume the libvirt-format box, so the two platforms run the
same disk image.

`vagrant up` writes `dev/vagrant/hosts.vagrant.kdl`, a complete Rustible
inventory of whichever machines are running, and rewrites it on every `up`,
`halt` and `destroy`, one fragment per machine so bringing up one cannot lose
the other's entry. A machine that is down leaves the inventory, because a
host that is listed and unreachable fails at connect with a worse error than
one that is absent.

That file is not committed: it names a key path on one developer's disk.

**This is why `--inventory` was added to the CLI.** The first shape had the
developer paste the generated `group "vagrant"` block into
`examples/workspace/hosts.kdl` by hand. That is a step people get wrong — I
got it wrong myself while writing this, with a script that matched the group
name inside the file's own comment text — and it made a committed file carry
one machine's addresses. `--inventory` is a flag the workspace config already
had as a key (`inventory = "..."` in `rustible.toml`), so this is the same
setting, spelled on the command line. `make vm-test` is then a one-liner and
nothing is hand-edited.

A side effect worth noting: the generated inventory declares no workspace-wide
vars, so the run stopped printing the two `var ... is not declared by this
playbook` warnings it had been inheriting from the example inventory. The
warnings were correct; the playbook was simply being run against an inventory
that was not about it.

## 3. The bug: multi-architecture boxes collide in libvirt's storage pool

`vagrant up arm` on the Linux host hung for fifteen minutes at
`Waiting for domain to get an IP address...`, then again for twenty. No error,
on any stream.

What it was: qemu was burning 2% of one core. A guest booting under TCG pegs a
core. This one was not booting at all.

`cloud-image/debian-12` publishes amd64 and arm64 under one box name and one
version. vagrant-libvirt 0.12.2 builds the name of the volume it uploads into
libvirt's storage pool from the box name and version alone:

```ruby
# action/handle_box_image.rb, get_volume_name
vol_name = box.name.to_s.dup.gsub('/', '-VAGRANTSLASH-')
vol_name << "_vagrant_box_image_#{version}_#{name.dup.gsub('/', '-SLASH-')}.img"
```

No architecture. So both machines resolve to one volume, the first `up` fills
it, and every later `up` of the *other* architecture finds it already there
and boots it. Confirmed by mounting the pool volume the aarch64 domain was
booting and finding `EFI/BOOT/BOOTX64.EFI` in it. An aarch64 UEFI finds no
`BOOTAA64.EFI`, sits at its own prompt, and vagrant waits for a DHCP lease
from a machine that never started.

The fix is in the Vagrantfile: record which architecture the pool volume
holds, in `.vagrant/rustible/box-arch`, and replace the volume before `up`
when the other architecture is wanted. The next `up` re-uploads from the box
already unpacked under `~/.vagrant.d`, a local copy of ~415 MB. Switching
architectures costs that copy; staying on one costs nothing. A volume with no
stamp beside it is treated as unknown, which is the same as wrong, and
replaced — re-uploading a disk we might not have needed to is the cheap error.

After the fix, `vagrant up arm` reached `SSH address: 192.168.121.118:22` and
booted.

### The first version of that fix was worse than the bug

Replacing the pool volume is only safe when no other machine is standing on
it. Each machine's own disk is a **copy-on-write overlay whose backing file is
that pool volume**:

```
$ qemu-img info -U --backing-chain /var/lib/libvirt/images/vagrant_x86.img
image: /var/lib/libvirt/images/vagrant_x86.img
backing file: .../cloud-image-VAGRANTSLASH-debian-12_vagrant_box_image_...box.img
```

So the first version, which deleted the volume unconditionally, left the
existing `x86` machine's overlay sitting on arm64 blocks. It did not *look*
broken: the running domain held the deleted inode open and kept working, and
`make vm-test` passed against both machines at once. It would have come apart
at the next `vagrant halt` — silent disk corruption instead of a silent hang,
which is worse, not better.

So the check now **refuses** when a machine of the other architecture still
exists, naming the command to destroy it:

```
cannot bring up 'arm' (arm64) while 'x86' exists.
vagrant-libvirt keeps one box volume per box name and version, with no
architecture in it, so both machines share it and their disks are
copy-on-write overlays on top of it. Giving 'arm' the arm64 image
would leave 'x86' reading the wrong architecture.

    cd dev/vagrant && vagrant destroy -f x86

Under libvirt it is one architecture at a time. See docs/DEVELOPING.md.
```

**Under libvirt it is therefore one architecture at a time.** The qemu
provider on macOS has no shared pool — each machine's disk lives in its own
`.vagrant` directory — so both machines coexist there, and both were up
together on the mac while `make vm-test` drove them.

That restriction is not costly in practice: the second architecture is the
emulated one, you go to it deliberately, and `vagrant destroy` plus `vagrant
up` is a minute of the time you were already going to spend.

Alternatives rejected: a second box from another publisher (diverges the two
machines' Debian lineage, and the arm64 candidates have double-digit download
counts); a per-architecture libvirt storage pool (`handle_storage_pool.rb`
refuses any non-`default` pool that does not already exist, so it becomes host
setup in the developer docs).

## 4. What `make vm-test` actually asserts

It runs the playbook **twice** and fails unless the second run reports nothing
changed and nothing failed.

The second run is the test. A first run reporting `changed` proves only that
the operation did something; an operation that rewrites a correct file on
every pass also reports `changed`, and every operation in this repository
claims not to. The assertion is exercised in both directions — it was checked
against synthetic summary tables with a nonzero `changed`, a nonzero `failed`,
a clean table, a two-host table with only the second bad, and empty input.
Empty input fails: no rows in the summary means nothing ran, which is not a
pass.

## 5. A bug the second platform found

`vm-test.sh` ran on Linux and failed on the mac at its first line of real work:

```
dev/vagrant/vm-test.sh: line 42: limit[@]: unbound variable
```

macOS ships bash **3.2.57**, where expanding an empty array under `set -u` is
an unbound-variable error; bash 5 on Linux allows it. The script builds an
empty `limit` array when no `HOSTS=` is given, which is the default, so the
default invocation was broken on exactly half the platforms this spike exists
to cover, and green on the half I wrote it on. Fixed with
`${limit[@]+"${limit[@]}"}`.

Worth recording because it is the machine tier's own argument in miniature: the
thing worked everywhere I had tested it, and the second real system found it in
one run.

## 6. Cost

- Box: ~841 MB unpacked under `~/.vagrant.d/boxes`, ~415 MB of it uploaded
  into libvirt's pool.
- Machine disks: copy-on-write overlays over the box. The x86 guest was 319 MB
  after a full playbook run; a freshly created one is under 1 MB.
- `make vm-destroy` removes the machines; `vagrant box prune` removes boxes.

## 7. Left undone

- **No CI job.** The machine tier is deliberately local: a nested-virtualisation
  runner is a different problem, and the tier's value is a developer watching a
  real machine, not a green tick.
- **Debian only.** Fedora would be worth adding when an operation grows
  behaviour that differs there; today nothing does.
