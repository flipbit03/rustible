# Vendored musl libc headers

**musl 1.2.5**, from <https://musl.libc.org/releases/musl-1.2.5.tar.gz>.
**MIT licensed**, attribution only; the upstream licence text is `COPYRIGHT`
in this directory, unchanged.

## Why these are here

Rustible's TLS crypto provider is `ring`, which compiles a little C. Ring's
build script passes `-nostdlibinc` and ships its own fallbacks for the two
libc functions it uses, but only for non-`x86_64` musl targets and only when
the compiler is clang-like. On `x86_64-unknown-linux-musl` it needs real libc
headers: `include/ring-core/check.h` includes `<assert.h>` unguarded, and
clang's own `immintrin.h` pulls `mm_malloc.h`, which pulls `<stdlib.h>`.

So a cross-build of a playbook binary for `x86_64-unknown-linux-musl` needs a
musl sysroot on the operator's machine, and asking every operator to install
one would be a second toolchain requirement on top of clang. Instead the
`rustible` binary carries these headers, unpacks them into the workspace cache
on first use and points the compiler at them, so `cargo install rustible-cli`
is enough and nothing has to be fetched. See `crates/rustible-cli/build.rs` and
`crates/rustible-cli/src/toolchain.rs`, and
`docs/plan/reports/C-TOOLCHAIN-SPIKE.md` for the measurements.

Only `x86_64` is vendored. `aarch64-unknown-linux-musl` takes ring's
`-nostdlibinc` path and needs no headers at all, which is why there is no
`aarch64/` here.

## How to regenerate

`make install-headers` compiles nothing; it is `sed` over `.h.in` templates.
`configure` still insists on finding a C compiler, hence the explicit `CC`.

```sh
curl -LO https://musl.libc.org/releases/musl-1.2.5.tar.gz
tar xzf musl-1.2.5.tar.gz
mkdir build-x86_64 && cd build-x86_64
CC=cc ../musl-1.2.5/configure --target=x86_64-linux-musl --prefix="$PWD/out"
make install-headers
# then: out/include -> crates/rustible-cli/vendor/musl-headers/x86_64/include
```

Nothing in this directory is edited. It is upstream's output as produced.
