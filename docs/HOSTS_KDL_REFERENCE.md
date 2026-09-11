# `hosts.kdl` reference

The machines Rustible manages, written in [KDL](https://kdl.dev). One file per
workspace, checked before a run touches anything.

KDL rather than YAML because nesting is braces. A misplaced space cannot
silently reparent a host into another group.

Validate it any time with:

```sh
rustible inventory check          # the file, and every playbook's vars against it
rustible inventory show web1      # one host fully resolved, with the source of each value
```

## The four top-level nodes

```
vars { ... }          // variables for every host
defaults ...          // parameters for every host
host "name" ...       // one machine
group "name" { ... }  // a set of machines
```

## A minimal file

What `rustible init` writes:

```kdl
host "local" connection="local"
```

## A real one

```kdl
vars {                                   // every host sees these
    timezone "Europe/Berlin"
}

defaults ssh_user="deploy" escalate="sudo"

host "laptop" connection="local"         // no ssh, runs here

group "web" {
    vars { nginx_workers 4 }
    host "web1" addr="10.0.1.11"
    host "web2" addr="10.0.1.12" {
        // host beats group
        vars { nginx_workers 8 }
    }
}

group "db" {
    host "db1" addr="10.0.2.11" ssh_user="pgadmin" port=2222
}

group "production" {
    members "web" "db"                   // a group of groups
}
```

## Parameters

Seven, and no others. Set them on a `host`, on a `group`, or on `defaults`;
the nearest one wins, and `rustible inventory show` prints where each came
from.

| parameter | meaning | default |
|---|---|---|
| `addr` | hostname or address ssh connects to | none |
| `connection` | `"ssh"` or `"local"` | `ssh` |
| `ssh_user` | account ssh logs in as | your username |
| `port` | ssh port | `22` |
| `escalate` | `"sudo"`, `"doas"` or `"none"` | `sudo` |
| `escalate_user` | account to escalate to | `root` |
| `ssh_args` | extra arguments for `ssh` | none |

Two rules worth knowing:

- **`addr` is host-only.** An address names one machine, so it cannot be
  inherited; setting it on a group or on `defaults` is a load error. Every
  host whose resolved `connection` is `ssh` must have one.
- **`ssh_args` does not merge.** The nearest level that sets it wins whole.
  Write one argument as a property, several as a child node:

```kdl
host "gate" addr="10.0.0.1" ssh_args="-4"

host "jump" addr="10.0.0.2" {
    ssh_args "-o" "StrictHostKeyChecking=no" "-o" "ProxyJump=bastion"
}
```

Using both spellings on one node is a load error.

## Variables

Three levels, nearest wins: workspace `vars`, then group, then host.

```kdl
// every host
vars { timezone "Europe/Berlin" }

group "web" {
    // this group
    vars { nginx_workers 4 }

    host "web2" addr="10.0.1.12" {
        // this host
        vars { nginx_workers 8 }
    }
}
```

Values are typed: strings, integers, booleans, and lists.

```kdl
vars {
    package "nginx"                      // string
    workers 4                            // integer
    tls #true                            // boolean: #true / #false
    allowed_ports 22 80 443              // list, from positional arguments
}
```

A playbook declares what it needs with `#[rustible::vars]`, and **every**
target host is validated against that struct before anything is built or
shipped, so a missing or mistyped var fails in a second.
`--var name=value` on the command line overrides the inventory.

## Groups of groups, and cherry-picking

`members` takes group names, host names, or both:

```kdl
group "web" {
    host "web1" addr="10.0.1.11"
}
group "db" {
    host "db1" addr="10.0.2.11"
}

group "production" {
    members "web" "db"                   // two groups
    vars { env "production" }
}

group "monitored" {
    members "web1" "db1"                 // individual hosts
}
```

A host in several groups inherits from the nearest; a cycle is a load error.

## Commenting a host out

KDL's `/-` disables the next node, children included:

```kdl
/-host "web3" addr="10.0.1.13" {
    vars { nginx_workers 2 }
}
```

`//` and `/* */` work as you expect.

## Pointing at a different file

The workspace's `rustible.toml` names the inventory:

```toml
inventory = "hosts.kdl"
```

`--inventory <FILE>` overrides it for one run, which is how you drive machines
that are not in the committed file. A relative path is relative to the current
directory.

## Errors

Load errors are reported together, one per line, with a position:

```
hosts.kdl:12:5: error: `addr` cannot be set on a group
hosts.kdl:20:1: error: group "prod" has no member named "web9"
```

Nothing is built or connected until the file is clean.
