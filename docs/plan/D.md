# D: Dogfood in `my_infra`

**Governing sections:** 6.9.
**Depends on:** M3, M4, and the ops each port needs from M6.
**NOT for unattended runs:** this milestone applies changes to real machines
(Cadu's home servers). A human runs it.

1. `rustible init /home/cadu/w/cadu/my_infra/rustible --path-deps <checkout>`.
2. Translate `inventory.yaml` to `hosts.kdl` (mapping in vision 6.9); exclude
   `cadumac` (macOS).
3. Port `playbooks/ourserver/subtasks/00_basic.yaml` to
   `playbooks/ourserver/basic.rs`; run with `--check` first, compare with an
   Ansible `--check` run, then for real.
4. Port the `ssh_keys_from_github` role to a function in `src/lib.rs` using
   `rustible-github`; use it from `playbooks/outpost/cadu_user.rs`.
5. Port one whole host. Keep Ansible files in place until parity.
