# Spike: build.rs playbook discovery

- **Date:** 2026-09-07
- **Tests:** section 9 of `01_VISION.md`, the bullet "A playbook is any `.rs` file under `playbooks/` that carries `#[rustible::playbook]`. A build script finds them."
- **Scratch workspace (left in place):** `/tmp/claude-1000/-home-cadu-w-cadu-rustible/5e9fe8ac-d62a-43bd-93c7-43bd950ce259/scratchpad/discovery-spike`
  - raw command logs per hypothesis: `logs/NN.txt`; LSP probe scripts: `lsp_probe.py`, `lsp_probe2.py`, `lsp_probe3.py`
- **Toolchain:** rustc 1.97.1 (8bab26f4f 2026-07-14), cargo 1.97.1, clippy (rustup component), rust-analyzer 1.97.1 (8bab26f 2026-07-14). Edition 2024.
  - Note: the `rust-analyzer` rustup component was **not** installed on this machine at the start (only the rustup proxy and `rust-analyzer-proc-macro-srv` were). Installed with `rustup component add rust-analyzer` for this spike.

## Result in one paragraph

All 20 hypotheses hold, with two corrections to section 9 and one hazard the section does not mention. Correction 1: rustc treats every `#[path]`-loaded file as a `mod.rs`, so `mod helpers;` inside `playbooks/cadu/x.rs` resolves to `playbooks/cadu/helpers.rs` (a sibling), not `playbooks/cadu/x/helpers.rs`. Correction 2: rust-analyzer only picks up a newly created playbook file automatically because check-on-save (on by default) re-runs the build script; with check-on-save off, the file stays "unlinked" until the "Rebuild proc macros and build scripts" command. Hazard: `cargo build` and rust-analyzer's `cargo check` share the build script's `OUT_DIR`, so a CLI run with `RUSTIBLE_PLAYBOOK=x` overwrites the registry the IDE reads and unlinks every other playbook in the editor until the next plain check. A Cargo feature enabled only by the CLI (`--features selected`) gives the selected build its own `OUT_DIR` and removes the hazard entirely, verified below.

## Scratch workspace layout

```
discovery-spike/
  Cargo.toml                      # [workspace] members = crates/rustible-macros, crates/rustible, workspace-demo
  crates/rustible-macros/         # proc-macro crate: #[playbook] renames `fn main` -> `pub fn __rustible_entry`
  crates/rustible/                # facade: `pub use rustible_macros::playbook;` + `prelude::{Ctx, playbook}`
  workspace-demo/                 # what `rustible init` would generate
    Cargo.toml
    build.rs
    src/main.rs                   # include!(OUT_DIR/playbooks.rs) + dispatcher
    src/lib.rs                    # pub fn helper() -> &'static str   (shared code)
    playbooks/
      top.rs                      # #[rustible::playbook(hosts = "all")], has its own `struct Vars`
      bare.rs                     # `use rustible::playbook;` + bare `#[playbook]`
      cadu/a.rs                   # #[rustible::playbook(hosts = "web")], `mod helpers;`, own `struct Vars`,
                                  #   calls workspace_demo::helper(), has #[cfg(test)] mod tests
      cadu/helpers.rs             # helper module pulled in by cadu/a.rs (see H3 for why it is a sibling)
      cadu/util.rs                # unmarked, never referenced
      cadu/fake.rs                # mentions #[rustible::playbook] only in a comment and a string
      ops/deploy/b.rs             # #[rustible::playbook]
```

`workspace-demo/Cargo.toml`:

```toml
[package]
name = "workspace-demo"
version = "0.1.0"
edition = "2024"
autobins = false
build = "build.rs"

[lib]
path = "src/lib.rs"

[[bin]]
name = "workspace-demo"
path = "src/main.rs"

[dependencies]
rustible = { path = "../crates/rustible" }

[build-dependencies]
syn = { version = "2", features = ["full", "visit"] }
proc-macro2 = { version = "1", features = ["span-locations"] }   # needed for line:col in parse errors (H8)

[features]
# Enabled only by the CLI together with RUSTIBLE_PLAYBOOK; gives the selected build its own build-script OUT_DIR.
selected = []                                                     # added late, see Findings F3
```

`workspace-demo/build.rs`:

```rust
//! Generated once by `rustible init`. Discovers playbooks under `playbooks/`.
use std::path::{Path, PathBuf};
use std::{env, fs};

use syn::visit::Visit;

struct Marked {
    fns: Vec<String>,
}

impl<'ast> Visit<'ast> for Marked {
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        let marked = f.attrs.iter().any(|a| {
            let p = a.path();
            let segs: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
            segs == ["rustible", "playbook"] || segs == ["playbook"]
        });
        if marked {
            self.fns.push(f.sig.ident.to_string());
        }
        // Do not descend: nested fns inside a playbook body are not playbooks.
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let playbooks_dir = manifest_dir.join("playbooks");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed=playbooks");
    println!("cargo:rerun-if-env-changed=RUSTIBLE_PLAYBOOK");

    let mut files = Vec::new();
    walk(&playbooks_dir, &mut files);

    // (name, abs path, module ident)
    let mut found: Vec<(String, PathBuf, String)> = Vec::new();
    for path in files {
        let src = fs::read_to_string(&path)
            .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", path.display())));
        let ast = match syn::parse_file(&src) {
            Ok(ast) => ast,
            Err(e) => {
                let (l, c) = (e.span().start().line, e.span().start().column + 1);
                fail(&format!(
                    "playbook file {}:{l}:{c} does not parse: {e}",
                    path.display()
                ))
            }
        };
        let mut v = Marked { fns: Vec::new() };
        v.visit_file(&ast);
        match v.fns.len() {
            0 => continue,
            1 => {}
            n => fail(&format!(
                "playbook file {} has {n} functions marked #[rustible::playbook] ({}); exactly one is allowed",
                path.display(),
                v.fns.join(", ")
            )),
        }
        let rel = path.strip_prefix(&playbooks_dir).unwrap().with_extension("");
        let name = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let ident = format!(
            "__pb_{}",
            name.chars()
                .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
                .collect::<String>()
        );
        found.push((name, path, ident));
    }

    let selected: Vec<&(String, PathBuf, String)> = match env::var("RUSTIBLE_PLAYBOOK") {
        Ok(want) if !want.is_empty() => {
            let hit: Vec<_> = found.iter().filter(|(n, _, _)| *n == want).collect();
            if hit.is_empty() {
                let mut names: Vec<&str> = found.iter().map(|(n, _, _)| n.as_str()).collect();
                names.sort();
                fail(&format!(
                    "RUSTIBLE_PLAYBOOK={want}: no such playbook under {}.\navailable playbooks:\n  {}",
                    playbooks_dir.display(),
                    names.join("\n  ")
                ));
            }
            hit
        }
        _ => found.iter().collect(),
    };

    let mut code = String::new();
    code.push_str("// @generated by build.rs; do not edit.\n");
    for (_, path, ident) in &selected {
        code.push_str(&format!(
            "#[allow(non_snake_case)]\n#[path = {:?}]\npub mod {ident};\n",
            path.display().to_string()
        ));
    }
    code.push_str("#[allow(clippy::type_complexity)]\npub const PLAYBOOKS: &[(&str, fn(&rustible::prelude::Ctx))] = &[\n");
    for (name, _, ident) in &selected {
        code.push_str(&format!("    ({name:?}, {ident}::__rustible_entry),\n"));
    }
    code.push_str("];\n");
    fs::write(out_dir.join("playbooks.rs"), code).unwrap();

    eprintln!(
        "rustible build.rs: {} playbook(s) selected of {} discovered",
        selected.len(),
        found.len()
    );
}

fn fail(msg: &str) -> ! {
    eprintln!("\nerror: {msg}\n");
    std::process::exit(1);
}
```

`workspace-demo/src/main.rs`:

```rust
// Written once by `rustible init`; never edited by the user.
include!(concat!(env!("OUT_DIR"), "/playbooks.rs"));

fn main() {
    let wanted: Option<String> = std::env::args().nth(1);
    let ctx_for = |name: &str| rustible::prelude::Ctx { playbook: name.to_string() };

    match wanted {
        Some(name) => match PLAYBOOKS.iter().find(|(n, _)| *n == name) {
            Some((n, entry)) => {
                println!("running playbook: {n}");
                entry(&ctx_for(n));
            }
            None => {
                eprintln!("playbook not found: {name}");
                eprintln!("available: {:?}", PLAYBOOKS.iter().map(|(n, _)| *n).collect::<Vec<_>>());
                std::process::exit(2);
            }
        },
        None => {
            for (n, entry) in PLAYBOOKS {
                println!("running playbook: {n}");
                entry(&ctx_for(n));
            }
        }
    }
}
```

The proc macro (`crates/rustible-macros/src/lib.rs`) parses the item as `syn::ItemFn`, errors unless it is named `main`, renames it to `__rustible_entry`, and makes it `pub`. Attribute arguments are ignored. The facade's `prelude` has `Ctx { playbook: String }` with `fn log(&self, &str)`.

Generated `$OUT_DIR/playbooks.rs` with everything present:

```rust
// @generated by build.rs; do not edit.
#[allow(non_snake_case)]
#[path = "/tmp/.../workspace-demo/playbooks/bare.rs"]
pub mod __pb_bare;
#[allow(non_snake_case)]
#[path = "/tmp/.../workspace-demo/playbooks/cadu/a.rs"]
pub mod __pb_cadu_a;
#[allow(non_snake_case)]
#[path = "/tmp/.../workspace-demo/playbooks/ops/deploy/b.rs"]
pub mod __pb_ops_deploy_b;
#[allow(non_snake_case)]
#[path = "/tmp/.../workspace-demo/playbooks/top.rs"]
pub mod __pb_top;
#[allow(clippy::type_complexity)]
pub const PLAYBOOKS: &[(&str, fn(&rustible::prelude::Ctx))] = &[
    ("bare", __pb_bare::__rustible_entry),
    ("cadu/a", __pb_cadu_a::__rustible_entry),
    ("ops/deploy/b", __pb_ops_deploy_b::__rustible_entry),
    ("top", __pb_top::__rustible_entry),
];
```

In the outputs below, the long scratch path prefix is abbreviated to `.../`.

## Discovery

### H1: nested files are discovered with names `cadu/a`, `ops/deploy/b`, `top` — PASS

```
$ cargo run -q
running playbook: bare
[bare] bare #[playbook] attribute detected
running playbook: cadu/a
[cadu/a] cadu/a says 6
[cadu/a] hello from workspace_demo::helper
running playbook: ops/deploy/b
[ops/deploy/b] ops/deploy/b deploying
running playbook: top
[top] hi from top
```

The registry above lists exactly `bare`, `cadu/a`, `ops/deploy/b`, `top`.

### H2: unmarked `playbooks/cadu/util.rs` is not registered — PASS

`util.rs` exists on disk (`find playbooks -name '*.rs'` lists it) and does not appear in the generated registry or in the run output above.

### H3: `mod helpers;` next to a playbook — PASS, but the resolved location differs from section 9

Section 9 says `mod helpers;` inside `playbooks/cadu/x.rs` resolves to `playbooks/cadu/x/helpers.rs`. It does not. rustc treats every file loaded through `#[path]` as a `mod.rs`-style file (rustc source, `rustc_expand/src/module.rs`: "All `#[path]` files are treated as though they are a `mod.rs` file"), so nested `mod` declarations resolve relative to the file's own directory.

(a) `mod helpers;` with `playbooks/cadu/a/helpers.rs` — FAIL:

```
$ cargo build
error[E0583]: file not found for module `helpers`
 --> .../workspace-demo/playbooks/cadu/a.rs:3:1
  |
3 | mod helpers;
  | ^^^^^^^^^^^^
  = help: to create the module `helpers`, create file ".../playbooks/cadu/helpers.rs" or ".../playbooks/cadu/helpers/mod.rs"
```

(b) `mod helpers;` with `playbooks/cadu/helpers.rs` (sibling of `a.rs`) — PASS:

```
$ cargo run -q -- cadu/a
running playbook: cadu/a
[cadu/a] cadu/a says 6
[cadu/a] hello from workspace_demo::helper
```

(c) `#[path = "a/helpers.rs"] mod helpers;` with `playbooks/cadu/a/helpers.rs` — PASS (same output as b).

The workspace keeps layout (b). See Findings F1.

### H4: attribute only inside a `//` comment and a string literal is not registered — PASS

`playbooks/cadu/fake.rs`:

```rust
// This file mentions #[rustible::playbook] only in a comment and in a string.
// #[rustible::playbook]
pub fn not_a_playbook() -> &'static str {
    "#[rustible::playbook] fn main() {}"
}
```

Not in the registry, not in the run output (H1 listing).

### H5: bare `#[playbook]` after `use rustible::playbook;` is detected — PASS

`playbooks/bare.rs` uses `use rustible::playbook;` and `#[playbook]`; the run output shows `running playbook: bare` and `[bare] bare #[playbook] attribute detected`. Note: the build script matches the attribute path textually (`rustible::playbook` or `playbook`); it does not resolve imports. A user-defined attribute macro also called `playbook` would be picked up. Acceptable for a marker.

### H6: add in a new subdirectory, rename, delete, with no manifest edits — PASS

```
$ mkdir -p playbooks/newteam && cat > playbooks/newteam/fresh.rs   # new subdir
$ cargo run -q 2>&1 | grep -E "running|fresh"
running playbook: bare
running playbook: cadu/a
running playbook: newteam/fresh
[newteam/fresh] fresh playbook in a brand-new directory
running playbook: ops/deploy/b
running playbook: top
$ mv playbooks/newteam/fresh.rs playbooks/newteam/renamed.rs && cargo run -q | grep running
running playbook: bare
running playbook: cadu/a
running playbook: newteam/renamed
running playbook: ops/deploy/b
running playbook: top
$ rm -r playbooks/newteam && cargo run -q | grep running
running playbook: bare
running playbook: cadu/a
running playbook: ops/deploy/b
running playbook: top
```

`cargo build -v` reports the cause as `the file "workspace-demo/playbooks" has changed`, confirming `rerun-if-changed` on a directory rescans the tree (creation in a new subdirectory, rename, and deletion all detected).

### H7: two marked functions in one file fail naming the file — PASS

```
$ cargo build      # with playbooks/ops/two.rs holding `main` and `main2` both marked
error: failed to run custom build command for `workspace-demo v0.1.0 (.../workspace-demo)`
  --- stderr
  error: playbook file .../workspace-demo/playbooks/ops/two.rs has 2 functions marked #[rustible::playbook] (main, main2); exactly one is allowed
```

### H8: syntax error in a playbook file fails naming the file, not a raw panic — PASS

```
$ cargo build      # with playbooks/ops/syntax.rs containing `let x = ;`
error: failed to run custom build command for `workspace-demo v0.1.0 (.../workspace-demo)`
  --- stderr
  error: playbook file .../workspace-demo/playbooks/ops/syntax.rs:4:13 does not parse: expected an expression
```

Line and column come from `proc-macro2` with the `span-locations` feature; without it `Span::start()` does not exist in a build script (first build attempt failed on exactly that).

## Isolation via RUSTIBLE_PLAYBOOK

`playbooks/ops/broken.rs` for H9 to H13:

```rust
use rustible::prelude::*;
#[rustible::playbook]
fn main(ctx: &Ctx) {
    let x: u32 = "no";
    ctx.log("never");
}
```

### H9: with a broken playbook present, plain `cargo build` fails — PASS (expected failure)

```
$ cargo build
error[E0308]: mismatched types
 --> .../workspace-demo/playbooks/ops/broken.rs:4:18
  |
4 |     let x: u32 = "no";
  |            ---   ^^^^ expected `u32`, found `&str`
error: could not compile `workspace-demo` (bin "workspace-demo") due to 1 previous error
exit=101
```

### H10: `RUSTIBLE_PLAYBOOK=cadu/a cargo run -- cadu/a` succeeds with the broken file present — PASS

```
$ RUSTIBLE_PLAYBOOK=cadu/a cargo run -q -- cadu/a
running playbook: cadu/a
[cadu/a] cadu/a says 6
[cadu/a] hello from workspace_demo::helper
exit=0
```

### H11: the selected binary does not contain the other playbooks — PASS

```
$ cat $OUT_DIR/playbooks.rs      # after the RUSTIBLE_PLAYBOOK=cadu/a build
#[path = ".../playbooks/cadu/a.rs"]
pub mod __pb_cadu_a;
pub const PLAYBOOKS: &[(&str, fn(&rustible::prelude::Ctx))] = &[
    ("cadu/a", __pb_cadu_a::__rustible_entry),
];
$ ../target/debug/workspace-demo top
playbook not found: top
available: ["cadu/a"]
exit=2
$ strings ../target/debug/workspace-demo | grep -c "hi from top"
0
$ strings ../target/debug/workspace-demo | grep -c "cadu/a says"
1
```

### H12: `RUSTIBLE_PLAYBOOK=does/not/exist` fails listing available playbooks — PASS

```
$ RUSTIBLE_PLAYBOOK=does/not/exist cargo build
error: failed to run custom build command for `workspace-demo v0.1.0 (.../workspace-demo)`
  --- stderr
  error: RUSTIBLE_PLAYBOOK=does/not/exist: no such playbook under .../workspace-demo/playbooks.
  available playbooks:
    bare
    cadu/a
    ops/broken
    ops/deploy/b
    top
exit=101
```

### H13: switching `RUSTIBLE_PLAYBOOK` re-runs the build script and rebuilds — PASS

```
$ time RUSTIBLE_PLAYBOOK=top cargo build -v          # previous build was cadu/a
       Dirty workspace-demo v0.1.0 (.../workspace-demo): the env variable RUSTIBLE_PLAYBOOK changed
     Running `.../build/workspace-demo-66cfb6a327fd9652/build-script-build`
     Running `rustc --crate-name workspace_demo ... src/lib.rs --crate-type lib ...`
     Running `rustc --crate-name workspace_demo ... src/main.rs --crate-type bin ...`
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.13s
0.148s total
$ ../target/debug/workspace-demo
running playbook: top
[top] hi from top
$ time RUSTIBLE_PLAYBOOK=cadu/a cargo build -v
       Dirty workspace-demo v0.1.0 (...): the env variable RUSTIBLE_PLAYBOOK changed
     Running `.../build-script-build`
     Running `rustc ... src/lib.rs ...`
     Running `rustc ... src/main.rs ...`
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.14s
0.154s total
$ ../target/debug/workspace-demo
running playbook: cadu/a
$ time RUSTIBLE_PLAYBOOK=cadu/a cargo build -v      # same value again
       Fresh workspace-demo v0.1.0 (...)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.00s
0.020s total
```

Rebuild time for this toy project: about 0.15 s. Note that the env change marks the whole *package* dirty, so `src/lib.rs` (the lib target) is recompiled too, not only the bin. Dependencies (`rustible`, `syn`, ...) stay Fresh.

### H14: unsetting `RUSTIBLE_PLAYBOOK` returns to all playbooks — PASS

`broken.rs` was removed first (otherwise the all-playbooks build cannot succeed, per H9).

```
$ cargo build -v          # RUSTIBLE_PLAYBOOK unset
       Dirty workspace-demo v0.1.0 (...): the env variable RUSTIBLE_PLAYBOOK changed
     Running `.../build-script-build`
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.11s
$ ../target/debug/workspace-demo | grep running
running playbook: bare
running playbook: cadu/a
running playbook: ops/deploy/b
running playbook: top
```

## IDE / tooling

### H15: `cargo clippy --all-targets` reports a lint inside a playbook file with the right path and line — PASS

`playbooks/ops/lint.rs` had `let x = 5; let x = x;` (and a clone, which fired nothing: `redundant_clone` is a nursery lint).

```
$ cargo clippy --all-targets
warning: redundant redefinition of a binding `x`
 --> .../workspace-demo/playbooks/ops/lint.rs:5:5
  |
5 |     let x = x;
  |     ^^^^^^^^^^
help: `x` is initially defined here
 --> .../workspace-demo/playbooks/ops/lint.rs:4:9
  = note: `#[warn(clippy::redundant_locals)]` on by default

warning: very complex type used. Consider factoring parts into `type` definitions
  --> .../target/debug/build/workspace-demo-49c1a955bb7f5666/out/playbooks.rs:17:22
   |
17 | pub const PLAYBOOKS: &[(&str, fn(&rustible::prelude::Ctx))] = &[
   = note: `#[warn(clippy::type_complexity)]` on by default
```

The second warning is clippy linting the *generated* registry. Fixed by having the generator emit `#[allow(clippy::type_complexity)]` (the build.rs above already does; after that `cargo clippy --all-targets` printed 0 warnings). The real SDK generator should carry a blanket `#[allow(clippy::all)]` on generated items.

### H16: `cargo test` runs a `#[cfg(test)] mod tests` inside a playbook file — PASS

```
$ cargo test
     Running unittests src/main.rs (.../target/debug/deps/workspace_demo-f5874fd95c2b33ea)
running 1 test
test __pb_cadu_a::tests::helper_twice_works ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

The test path is `__pb_cadu_a::tests::helper_twice_works`, i.e. the generated module identifier shows in test names. Cosmetic; a nicer ident scheme (`cadu::a`) via nested inline modules would fix it if wanted.

### H17: rust-analyzer loads playbooks as crate members, reports errors at the right path, resolves the prelude — PASS

**Subcommands and flags.** `rust-analyzer --help` lists `analysis-stats`, `diagnostics`, `unresolved-references`, `prime-caches`, `run-tests`, `ssr`, `search`, `lsif`, `scip`. The relevant options on `diagnostics` / `unresolved-references` / `analysis-stats`:

```
--disable-build-scripts   Don't run build scripts or load `OUT_DIR` values by running `cargo check` before analysis.
--disable-proc-macros     Don't expand proc macros.
--proc-macro-srv <path>   Run the proc-macro-srv binary at the specified path.
```

So the CLI runs build scripts and proc macros **by default**; there is no `--run-build-scripts` flag because nothing needs enabling. Editor defaults, from `rust-analyzer --print-config-schema`:

```
rust-analyzer.cargo.buildScripts.enable        default: true   Run build scripts (`build.rs`) for more precise code analysis.
rust-analyzer.cargo.buildScripts.rebuildOnSave default: true   Rerun proc-macros building/build-scripts running when proc-macro or build-script sources change and are saved.
rust-analyzer.procMacro.enable                 default: true   Enable support for procedural macros, implies buildScripts.enable.
```

**(a) Playbook files are crate members, not detached.** With `broken.rs` planted:

```
$ rust-analyzer diagnostics .      # defaults
processing workspace-demo/playbooks/top.rs
processing workspace-demo/playbooks/ops/deploy/b.rs
processing workspace-demo/playbooks/ops/broken.rs
  at crate workspace_demo, file workspace-demo/playbooks/ops/broken.rs: Error RustcHardError("E0308") from LineCol { line: 3, col: 17 } to LineCol { line: 3, col: 21 }: expected u32, found &'static str
processing workspace-demo/playbooks/cadu/a.rs
  at crate workspace_demo, file workspace-demo/playbooks/cadu/a.rs: WeakWarning Ra("inactive-code") ... code is inactive due to #[cfg] directives: test is disabled
processing workspace-demo/playbooks/cadu/helpers.rs
processing workspace-demo/playbooks/bare.rs
processing workspace-demo/src/lib.rs
...
diagnostic scan complete
```

Every playbook file is walked as part of `crate workspace_demo`. `util.rs` and `fake.rs` are not walked (not in any crate).

**(b) Type error at the correct path and line.** `LineCol { line: 3, col: 17 }` is 0-based, i.e. `broken.rs:4:18`, matching rustc's `4:18` in H9. Over LSP (pull diagnostics, `lsp_probe.py`): `workspace-demo/playbooks/ops/broken.rs:4:18 sev=1 code=E0308 :: expected u32, found &'static str`.

**(c) `rustible::prelude::Ctx` resolves.** `rust-analyzer unresolved-references .` printed nothing. Over LSP, hover on `Ctx` in `fn main(ctx: &Ctx)` of `playbooks/cadu/a.rs` returned:

```
rustible::prelude

pub struct Ctx {
    pub playbook: String,
}
```

and `playbooks/cadu/a.rs` had 0 diagnostics.

**Negative control, build scripts disabled.** Playbook files disappear from the crate and `main.rs` breaks:

```
$ rust-analyzer diagnostics --disable-build-scripts .
at crate workspace_demo, file workspace-demo/src/main.rs: Error Ra("macro-error") ... `OUT_DIR` not set, build scripts may have failed to run
at crate workspace_demo, file workspace-demo/src/main.rs: Error RustcHardError("E0425") ... no such value in this scope   (PLAYBOOKS)
```

and only 6 files are processed instead of 12, none under `playbooks/`. So IDE support for playbooks depends entirely on build scripts being enabled, which is the default. `--disable-proc-macros` changed nothing visible in the CLI walk (the registry file under `target/` is not one of the walked files), so it is not a meaningful control here.

**Unmarked file in the editor.** Over LSP, `playbooks/cadu/util.rs` gets `1:1 sev=4 code=unlinked-file :: This file is not included in any crates, so rust-analyzer can't offer IDE services.` This is the "greyed out" behaviour section 9 predicts.

**New playbook file while the editor is open (beyond the brief, but it is the core UX promise).** `lsp_probe2.py` starts rust-analyzer, waits until quiescent, creates `playbooks/newteam/fresh.rs` on disk with a planted type error, then opens it, saves it, and finally sends the reload / rebuild commands. It reads back rust-analyzer's own `OUT_DIR/playbooks.rs` (from `rust-analyzer/analyzerStatus`) at each step.

With `checkOnSave = true` (the default):

```
1. quiescent after 2.1s
   RA OUT_DIR registry: mtime=20:29:31 names=['bare', 'cadu/a', 'ops/deploy/b', 'top']
2. created playbooks/newteam/fresh.rs on disk; sent nothing; waiting 5s
   RA OUT_DIR registry: mtime=20:29:31 names=['bare', 'cadu/a', 'ops/deploy/b', 'top']
  [3. after didOpen, no save] diagnostics: 4:18 E0308
   RA OUT_DIR registry: mtime=20:31:28 names=['bare', 'cadu/a', 'newteam/fresh', 'ops/deploy/b', 'top']
```

Opening the file triggered rust-analyzer's flycheck (`cargo check`), which re-ran the build script (the `playbooks` directory changed), rewrote the registry, and rust-analyzer, which watches `OUT_DIR` files, relinked the new file within seconds. No manual action.

With `checkOnSave = false`:

```
  [3. after didOpen, no save] diagnostics: 1:1 unlinked-file
  [4. after didSave] diagnostics: 1:1 unlinked-file
   reloadWorkspace response: {'result': None}
  [5. after rust-analyzer/reloadWorkspace] diagnostics: 1:1 unlinked-file          (registry untouched)
   rebuildProcMacros response: {'result': None}
  [6. after rust-analyzer/rebuildProcMacros ("Rebuild proc macros and build scripts")] diagnostics: 4:18 E0308
   RA OUT_DIR registry: mtime=20:50:03 names=['bare', 'cadu/a', 'newteam/fresh', 'ops/deploy/b', 'top']
```

The server log confirms only two build-script runs in that session: startup and the explicit rebuild request. `rebuildOnSave` does not help, because it only fires for changes to `build.rs` or proc-macro sources, and "Reload workspace" re-fetches `cargo metadata` but does not re-run build scripts. So the "just create the file" experience holds under editor defaults, and degrades to "run Rebuild proc macros and build scripts" (or any `cargo check`/`cargo build` in a terminal) when check-on-save is off. See Findings F2.

### H18: shared code from `src/lib.rs` is `workspace_demo::helper()`, not `crate::helper()` — PASS

```
$ cargo build      # playbooks/ops/shared.rs calls crate::helper()
error[E0425]: cannot find function `helper` in the crate root
 --> .../workspace-demo/playbooks/ops/shared.rs:4:20
  |
4 |     ctx.log(crate::helper());
  |                    ^^^^^^ not found in the crate root
$ cargo run -q -- ops/shared      # after changing to workspace_demo::helper()
running playbook: ops/shared
[ops/shared] hello from workspace_demo::helper
```

Playbooks are modules of the **bin** crate; the lib target is a separate crate reachable by the package name with dashes turned to underscores. `rustible init` should write a comment in `src/lib.rs` saying so, or the facade could re-export it under a fixed name.

### H19: two playbooks each define `struct Vars` — PASS

```
$ grep -n "struct Vars" playbooks/top.rs playbooks/cadu/a.rs && cargo build
playbooks/top.rs:3:struct Vars {
playbooks/cadu/a.rs:5:struct Vars {
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.12s
```

## Performance

### H20: 50 generated playbooks — PASS

50 files `playbooks/perf/p00.rs` to `p49.rs`, each with a `struct Vars`, a helper fn and a marked `main`.

```
$ time cargo build                        # (a1) 50 new files: build script + lib + bin recompile
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.27s
0.289s total
$ time cargo build -v                     # (b) no-op, nothing changed
       Fresh workspace-demo v0.1.0 (...)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.01s
0.021s total                              # build script NOT re-run
$ touch playbooks/perf/p07.rs && time cargo build -v      # (a2) one file touched
       Dirty workspace-demo v0.1.0 (...): the file `workspace-demo/playbooks` has changed
     Running `.../build-script-build`
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.16s
0.179s total
$ time <build-script-build run directly with OUT_DIR/CARGO_MANIFEST_DIR set>   # (a3) script alone, 57 files scanned, 3 runs
0.012s / 0.010s / 0.010s total
$ ../target/debug/workspace-demo | grep -c "running playbook"
54
$ rm -r playbooks/perf && cargo build && ../target/debug/workspace-demo | grep -c "running playbook"
4
```

The build script itself (walk + `syn` parse of 57 files + codegen) takes about 10 ms. Whole-build numbers are dominated by rustc on trivial playbooks and will scale with real playbook size, not with discovery.

## Extra: CLI builds versus the IDE's registry (not in the brief; found while testing H17)

`lsp_probe3.py`: rust-analyzer running with defaults, `playbooks/top.rs` open and healthy, then external cargo invocations from a terminal.

```
1. quiescent; RA OUT_DIR registry: names=['bare', 'cadu/a', 'ops/deploy/b', 'top']
  [2. top.rs opened] diagnostics: 0 diagnostics
  [3. after external `RUSTIBLE_PLAYBOOK=cadu/a cargo build`] diagnostics: 1:1 unlinked-file
   RA OUT_DIR registry: mtime=20:52:24 names=['cadu/a']
  [4. after external `RUSTIBLE_PLAYBOOK=cadu/a cargo check`] diagnostics: 1:1 unlinked-file
  [5. after external plain `cargo check`] diagnostics: 0 diagnostics
   RA OUT_DIR registry: names=['bare', 'cadu/a', 'ops/deploy/b', 'top']
```

`cargo build` and rust-analyzer's `cargo check` share the build script's run directory (`target/debug/build/workspace-demo-6b2e80bdba24b10f/out`), so a selected build from the CLI rewrites the registry the IDE is reading, and every playbook except the selected one goes `unlinked-file` in the editor until the next plain check (the next save with check-on-save on). Each flip also re-runs the build script and recompiles the bin crate on both sides, because the env var fingerprint changed.

**Mitigation, verified (`logs/21-feature-isolation.txt`):** add `[features] selected = []` to the generated `Cargo.toml` and have the CLI build with `--features selected` together with `RUSTIBLE_PLAYBOOK`. A different feature set gives the build script run and the bin a different metadata hash, so they get their own `OUT_DIR` and artifacts, while dependencies keep the same hash and stay shared.

```
$ cargo check                                             # T1 what the IDE runs
    workspace-demo-6b2e80bdba24b10f 20:53:52 ['bare', 'cadu/a', 'ops/deploy/b', 'top']
$ RUSTIBLE_PLAYBOOK=cadu/a cargo build --features selected -v      # T2 CLI-style
       Fresh syn v2.0.119 / Fresh rustible-macros / Fresh rustible ...   (deps shared)
     Running `target/debug/build/workspace-demo-3ddc8fd8af1f5529/build-script-build`
    workspace-demo-1d558c09a383cf90 20:53:53 ['cadu/a']                 (new, CLI-only OUT_DIR)
    workspace-demo-6b2e80bdba24b10f 20:53:52 ['bare', 'cadu/a', 'ops/deploy/b', 'top']   (IDE's, untouched)
$ ../target/debug/workspace-demo top
playbook not found: top
available: ["cadu/a"]
$ cargo check -v                                          # T3 IDE check again
       Fresh workspace-demo v0.1.0 (...)   Finished in 0.00s               (no ping-pong)
$ RUSTIBLE_PLAYBOOK=top cargo build --features selected -v          # T4 switch playbook
       Dirty workspace-demo v0.1.0: the env variable RUSTIBLE_PLAYBOOK changed
    workspace-demo-1d558c09a383cf90 20:53:53 ['top']
    workspace-demo-6b2e80bdba24b10f 20:53:52 ['bare', 'cadu/a', 'ops/deploy/b', 'top']
$ cargo check -v                                          # T5
       Fresh workspace-demo v0.1.0 (...)   Finished in 0.00s
```

Alternatives that also work but cost more: `--target-dir target/rustible` for CLI builds (duplicates every dependency's artifacts), or a dedicated `[profile.rustible]` (also a separate artifact tree).

## Findings

Summary table:

| # | Hypothesis | Result |
|---|---|---|
| 1 | Nested discovery, names `cadu/a`, `ops/deploy/b`, `top` | PASS |
| 2 | Unmarked file not registered | PASS |
| 3 | `mod helpers;` next to a playbook | PASS, but resolves to a **sibling** file (`cadu/helpers.rs`), not `cadu/a/helpers.rs` |
| 4 | Attribute in comment/string ignored | PASS |
| 5 | Bare `#[playbook]` detected | PASS |
| 6 | Add in new subdir / rename / delete, no manifest edits | PASS |
| 7 | Two marked fns: error names the file | PASS |
| 8 | Syntax error: error names file:line:col, no panic | PASS |
| 9 | Broken playbook fails plain `cargo build` | PASS (expected failure) |
| 10 | `RUSTIBLE_PLAYBOOK=cadu/a` runs despite broken sibling | PASS |
| 11 | Selected binary contains only that playbook | PASS |
| 12 | Unknown selection lists available playbooks | PASS |
| 13 | Switching selection re-runs build script, ~0.15 s here | PASS |
| 14 | Unsetting returns to all | PASS |
| 15 | clippy lint inside playbook at right path/line | PASS (generator needs a clippy allow on the registry) |
| 16 | `cargo test` runs tests inside a playbook | PASS |
| 17 | rust-analyzer: linked, correct error location, prelude resolves | PASS (defaults); new-file pickup depends on check-on-save |
| 18 | Shared code via `workspace_demo::helper()`; `crate::` fails | PASS |
| 19 | Per-playbook `struct Vars` | PASS |
| 20 | 50 playbooks: script ~10 ms, no-op build 0.02 s | PASS |

**F1. Deviation from section 9: helper module location.** `#[path]`-loaded files are `mod.rs`-style, so `mod helpers;` in `playbooks/cadu/x.rs` loads `playbooks/cadu/helpers.rs`, not `playbooks/cadu/x/helpers.rs`. Consequences: helpers sit next to playbooks as siblings (unmarked files, ignored by the scanner, which is consistent with the rest of the design); two playbooks in the same directory that both write `mod helpers;` compile the same file twice as two private modules (fine, but duplicated); a playbook that wants the `x/helpers.rs` layout must write `#[path = "x/helpers.rs"] mod helpers;`. Recommendation: change the sentence in section 9 to the sibling rule and mention the `#[path]` escape hatch. There is no way to get non-`mod.rs` semantics for a `#[path]` module from the generator side.

**F2. Deviation from section 9: "rust-analyzer runs build scripts" is true, but new-file pickup rides on check-on-save.** rust-analyzer runs build scripts once at load and again only for `build.rs`/proc-macro source changes or the explicit "Rebuild proc macros and build scripts" command. A newly created playbook file becomes linked because check-on-save (default on) runs `cargo check`, which re-runs the build script, and rust-analyzer watches the `OUT_DIR` registry. Under editor defaults the experience is exactly what section 9 promises, within seconds of opening the new file. With check-on-save disabled the file shows `unlinked-file` until a rebuild command or any terminal `cargo` invocation. Recommendation: state this dependency in section 9 and in the `rustible init` README, and have `rustible playbook create` print the hint.

**F3. Hazard not in section 9: the CLI's selected build clobbers the IDE's registry.** Same `OUT_DIR` for `cargo build` and rust-analyzer's `cargo check` means `rustible playbook run x` from a terminal unlinks all other playbooks in the open editor and forces build-script re-runs on both sides at every switch. Verified fix: `[features] selected = []` in the generated manifest and `--features selected` on every CLI build that sets `RUSTIBLE_PLAYBOOK`. Separate OUT_DIR and bin artifacts, shared dependencies, IDE check stays Fresh. Recommendation: adopt this in the design of `rustible init` and the CLI; it also removes the "switching playbooks recompiles the IDE's view" cost. The build script could additionally refuse `RUSTIBLE_PLAYBOOK` without `CARGO_FEATURE_SELECTED` to stop a stray exported variable from silently narrowing an IDE or CI build.

**F4. Smaller observations.**
- The env-var fingerprint marks the whole package dirty, so the lib target recompiles along with the bin on every selection switch (H13). Negligible here; worth knowing if `src/lib.rs` grows.
- Generated module identifiers leak into test names (`__pb_cadu_a::tests::...`, H16) and into panics/backtraces. Nested inline modules (`pub mod cadu { #[path=...] pub mod a; }`) would give `cadu::a::tests::...` at no cost.
- The scanner matches the attribute path textually; `#[playbook]` from any crate is accepted (H5). Acceptable for a marker; document it.
- The generator must emit clippy allows on generated items (H15).
- `proc-macro2` needs the `span-locations` feature in `[build-dependencies]` to report line:col on parse errors (H8).
- The `rust-analyzer` rustup component was missing on this machine; only the proxy existed. `rustible doctor`, if it ever exists, could check `rust-analyzer --version`.

**F5. What was not tested.** `rustible playbook list` reusing the scan (no CLI exists yet, but the scan is the same function). Cross-compilation of the selected bin. Behaviour when `playbooks/` does not exist (the walk silently yields zero playbooks; the registry is empty and the bin builds). Symlinked playbook directories.
