# Developing Rustible

Most work here needs nothing but a Rust toolchain and `clang`. Two extra tiers
exist for testing against real systems, and this file is how to turn them on.

```sh
make            # fmt, clippy, test, rustdoc, and the example workspace
make integration    # the container tier: needs docker
make vm-up          # a virtual machine
make vm-test        # the machine tier: needs vagrant
```

## The three tiers, and when the machine tier is required

1. **Pure functions.** Parsers and planners. Always run.
2. **Containers** (`make integration`). Real distributions, real package
   managers, real `useradd`. They caught that `useradd` refuses to create a
   private group when one already carries the name, and that `chown` clears
   setuid.
3. **Machines** (`make vm-test`). A real SSH transport, a real `sudo`, a live
   `/proc/sys`, and a real init system. A container has none of those: it
   shares the host kernel, so `sysctl` writes are refused or leak to the host,
   and it has no pid 1 to ask about a unit.

Tiers 1 and 2 run in CI. **Tier 3 does not**, and it is optional day to day.

**Writing a new operation is where it stops being optional.** An operation is
not proven by a fake that returns what the operation asked for. Bring up a
machine, run the operation against it, run it again and watch it report `ok`,
and say in the pull request which architecture you did that on. It is slower
than you would like. It is also the only tier that has ever found the bugs
that mattered.

## What it costs

A guest whose architecture matches the host, on a host with hardware
virtualisation, is accelerated. Everything else is qemu's TCG interpreter,
which is an order of magnitude slower but works everywhere. Measured on the
two machines this was developed on:

| host | guest | acceleration | `vagrant up` from destroyed |
|---|---|---|---|
| macOS, Apple silicon | aarch64 | HVF | 17 s |
| macOS, Apple silicon | x86_64 | TCG | 37 s |
| Linux x86_64, no `/dev/kvm` | x86_64 | TCG | 63 s |
| Linux x86_64, no `/dev/kvm` | aarch64 | TCG | 103 s |

A Linux host with `/dev/kvm` boots its own architecture in seconds. Whether
you have one is `test -e /dev/kvm`; a VM that is itself a guest usually does
not, unless nested virtualisation is enabled on the hypervisor. The Linux
machine those numbers come from is one of those, which is why even its own
architecture is interpreted, and why emulated x86_64 on a Mac beats it.

Also install `qemu-efi-aarch64` before reaching for the aarch64 guest on
Linux; `vagrant up` says so by name if it is missing.

`vagrant up` prints which of these you are getting before it starts, so a slow
boot is a number you were told rather than a mysterious hang.

Disk: the box is ~841 MB unpacked under `~/.vagrant.d/boxes`, plus ~415 MB of
it uploaded into libvirt's storage pool. Each machine's own disk is a
copy-on-write overlay that starts near zero and grows with what you install.
`make vm-destroy` takes the machines away; `vagrant box prune` takes the boxes.

## Setup: Linux

The libvirt provider. On Debian or Ubuntu:

```sh
sudo apt-get install -y \
    vagrant vagrant-libvirt \
    libvirt-daemon-system libvirt-clients \
    qemu-system-x86 qemu-system-arm qemu-utils \
    qemu-efi-aarch64 ovmf
sudo usermod -aG libvirt "$USER"
newgrp libvirt        # or log out and back in
```

- `qemu-system-arm` and `qemu-efi-aarch64` are what make the aarch64 machine
  possible; without them `make vm-up-arm` fails at `up`.
- `libvirt-clients` provides `virsh`, which the Vagrantfile calls (see
  "Switching architectures" below).
- `newgrp libvirt` matters: the group is granted but not active in a shell
  that was already open, and `vagrant up` fails to connect to libvirt with a
  permission error that does not mention groups.

Check it:

```sh
virsh -c qemu:///system list --all     # should print an empty table, not an error
make vm-up && make vm-test
```

## Setup: macOS

The qemu provider, since libvirt is a Linux hypervisor stack.

```sh
brew trust --cask hashicorp/tap/hashicorp-vagrant
brew install --cask hashicorp-vagrant
brew install qemu
vagrant plugin install vagrant-qemu
```

- `brew trust` first: the cask is in HashiCorp's own tap, and `brew install`
  refuses an untrusted third-party cask before it will run it.
- The cask puts the binary at `/usr/local/bin/vagrant`, which is on an
  interactive shell's PATH but not necessarily on a non-interactive one. If a
  script cannot find `vagrant`, that is why.
- The aarch64 machine is the accelerated one here (HVF), and it is the one to
  reach for. The x86_64 machine works and is slow.

Check it:

```sh
vagrant --version
make vm-up && make vm-test
```

## The machines

Defined in `dev/vagrant/Vagrantfile`: `x86` and `arm`, both Debian 12 from
`cloud-image/debian-12`, 4 CPUs and 2 GB each. Same distribution on both, so a
difference between them is a difference in architecture and nothing else.

```sh
make vm-up          # the one matching this host's architecture
make vm-up-x86
make vm-up-arm
make vm-status      # what is up, and the inventory that names it
make vm-ssh         # a shell inside it (make vm-ssh M=arm to pick one)
make vm-test        # run the playbook against whatever is up, twice
make vm-halt        # stop, keep the disks
make vm-destroy     # delete them
make vm-orphans     # domains left behind by a deleted checkout
```

`make vm-up` with no suffix brings up **the guest whose architecture matches
this host** — `x86` on an x86_64 machine, `arm` on Apple silicon — and only
that one. It is the fastest guest available to you, though not necessarily an
accelerated one: a host without hardware virtualisation interprets its own
architecture too (63 s in the table above). It is also the machine `vagrant
ssh` and `make vm-ssh` reach without being told which.

The other architecture is always emulated, so it never starts by accident:
ask for it by name with `make vm-up-arm` or `make vm-up-x86`.

Once one is up, a shell in it is `make vm-ssh`, and you have passwordless
`sudo` there. It is an ordinary Debian box: install things, break things,
`make vm-destroy` and start again.

`vagrant up` writes `dev/vagrant/hosts.vagrant.kdl`, a complete inventory of
the machines that are running, and rewrites it on every `up`, `halt` and
`destroy`. It is not committed: it names the key path on one developer's disk.
`make vm-test` points a run at it with `--inventory`, so nothing has to be
pasted anywhere.

To limit a run to one machine:

```sh
make vm-test HOSTS=vagrant-arm
```

To drive the machines with `rustible` directly — a dry run, more verbosity, a
playbook of your own — point it at that inventory rather than editing a
tracked file:

```sh
rustible --workspace examples/workspace \
         --inventory dev/vagrant/hosts.vagrant.kdl \
         playbook run vagrant --check -v
```

`make vm-test` runs the playbook twice and fails unless the second run reports
nothing changed. That second run is the test. A first run that reports
`changed` proves only that the operation did something; an operation that
rewrites a correct file every time also reports `changed`.

## Troubleshooting

**On Linux it is one architecture at a time.** vagrant-libvirt 0.12.2 names
the box volume in libvirt's storage pool from the box name and version alone,
with no architecture, and `cloud-image/debian-12` publishes amd64 and arm64
under one name and one version. Both machines therefore share one pool volume,
and each machine's disk is a copy-on-write overlay on top of it. So:

```sh
make vm-destroy      # the machine you have
make vm-up-arm       # the one you want
```

`vagrant up` refuses, naming that command, rather than swapping the volume
under a machine that already exists — which would leave it reading the other
architecture's blocks, working until its next `halt` and then not. Switching
costs one local re-upload of ~415 MB; staying on one architecture costs
nothing, and `removing it so the <arch> disk is uploaded` is that re-upload.

**macOS has no such restriction**: the qemu provider gives each machine its
own disk, so both can be up together.

Left unhandled, this bug has no error in it at all: an aarch64 guest handed an
amd64 disk finds no `BOOTAA64.EFI`, sits in UEFI forever, and `vagrant up`
waits for an IP address that never arrives.

**`vagrant up` waits forever for an IP.** On an emulated guest that may simply
be TCG: check the table above, and give it time. If it is the architecture you
did *not* bring up last, see the paragraph above.

**Port already in use.** The machines forward SSH on fixed ports (50022 and
50023) so the inventory can name them. A Vagrant machine left over in another
checkout holds them: `vagrant global-status --prune` finds it, and
`vagrant destroy <id>` releases it.

**A machine is running but Vagrant says `not created`.** The state tying a
libvirt domain to Vagrant lives in `dev/vagrant/.vagrant/`, so deleting a
checkout — or removing a git worktree — before destroying its machines leaves
a domain running that nothing tracks. It keeps its disk and `vagrant destroy`
can no longer see it. `make vm-orphans` lists any and prints the `virsh`
commands to remove them. **Destroy the machines before deleting a checkout.**

**Locale complaints on `vagrant ssh`.** Fixed: the Vagrantfile pins `LC_ALL`
and `LANG` for the connection, because OpenSSH forwards them by default and
the box generates only `C.UTF-8`. If you see them anyway, something is
overriding `config.ssh.extra_args`.
