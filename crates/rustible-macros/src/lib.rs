//! Proc macros for Rustible. Not used directly: the `rustible` facade
//! re-exports them as `rustible::playbook`, `rustible::vars`,
//! `rustible::template` and `rustible::integration_test`.
//!
//! Everything the expansions reference lives under `::rustible::sdk`, so a
//! workspace only needs the `rustible` crate as a dependency.

#![deny(missing_docs)]
// `template!` reads template files at compile time, on the operator's
// machine, as `include_str!` does. Vision 7.2's `sys` rule is about
// operations running on a target; see clippy.toml.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod template;

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::{Error, FnArg, ItemFn, ItemStruct, LitBool, LitStr, Type};

/// Marks a playbook's `main` (vision doc sections 6.1, 9, 10.3).
///
/// ```no_run
/// # use rustible::prelude::*;
/// # #[rustible::vars]
/// # struct Vars { greeting: String }
/// #[rustible::playbook(hosts = "web", vars = Vars, escalate = true)]
/// fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> {
///     ctx.log(vars.greeting);
///     Ok(())
/// }
/// # fn main() {}
/// ```
///
/// `hosts` is required. `vars` names a `#[rustible::vars]` struct and adds a
/// second parameter to `main`. `escalate` (Ansible's `become`) defaults to
/// false. `ssh_user = "<account>"` logs in as that account instead of the
/// host's `ssh_user` from the inventory. Expands to a private renamed
/// `main`, a `__rustible_entry` that deserializes the vars, and a
/// `__RUSTIBLE_PLAYBOOK` static the build script's registry points at.
#[proc_macro_attribute]
pub fn playbook(attr: TokenStream, item: TokenStream) -> TokenStream {
    match playbook_impl(attr.into(), item.into()) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

struct PlaybookArgs {
    hosts: Option<LitStr>,
    vars: Option<Type>,
    escalate: Option<LitBool>,
    ssh_user: Option<LitStr>,
}

/// Reject a `ssh_user` that cannot name an account, at compile time and on
/// the literal.
///
/// The same rule as `rustible-cli`'s `validate_account_name` (which checks
/// the inventory's `ssh_user`) and `rustible_std::group::validate_name`. The
/// three crates cannot share one function, so they are kept in step by these
/// notes: a stricter rule here would refuse an account the inventory accepts.
fn validate_ssh_user(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("is empty".to_string());
    }
    if name.starts_with('-') {
        return Err(format!(
            "is `{}`, which starts with `-` and would be read as a flag",
            name.escape_default()
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| *c == ':' || *c == ',' || c.is_whitespace() || c.is_control())
    {
        return Err(format!(
            "is `{}`, which contains `{}`; an account name cannot contain `:`, `,`, whitespace or control characters",
            name.escape_default(),
            bad.escape_default()
        ));
    }
    Ok(())
}

fn playbook_impl(
    attr: proc_macro2::TokenStream,
    item: proc_macro2::TokenStream,
) -> syn::Result<proc_macro2::TokenStream> {
    let mut args = PlaybookArgs {
        hosts: None,
        vars: None,
        escalate: None,
        ssh_user: None,
    };
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("hosts") {
            args.hosts = Some(meta.value()?.parse()?);
            Ok(())
        } else if meta.path.is_ident("vars") {
            args.vars = Some(meta.value()?.parse()?);
            Ok(())
        } else if meta.path.is_ident("escalate") {
            args.escalate = Some(meta.value()?.parse()?);
            Ok(())
        } else if meta.path.is_ident("ssh_user") {
            let lit: LitStr = meta.value()?.parse()?;
            if let Err(why) = validate_ssh_user(&lit.value()) {
                return Err(Error::new_spanned(
                    &lit,
                    format!("playbook option `ssh_user` {why}"),
                ));
            }
            args.ssh_user = Some(lit);
            Ok(())
        } else {
            let key = meta
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_else(|| "?".into());
            Err(meta.error(format!(
                "unknown playbook option `{key}`; expected `hosts`, `vars`, `escalate`, or `ssh_user`"
            )))
        }
    });
    parser.parse2(attr)?;

    let hosts = args.hosts.ok_or_else(|| {
        Error::new(
            proc_macro2::Span::call_site(),
            "`#[rustible::playbook]` needs `hosts = \"<host or group>\"`",
        )
    })?;
    let escalate = args.escalate.map(|b| b.value).unwrap_or(false);
    let ssh_user = match &args.ssh_user {
        Some(u) => quote! { ::core::option::Option::Some(#u) },
        None => quote! { ::core::option::Option::None },
    };

    let mut f: ItemFn = syn::parse2(item)?;
    if f.sig.ident != "main" {
        return Err(Error::new_spanned(
            &f.sig.ident,
            "`#[rustible::playbook]` goes on `fn main`",
        ));
    }
    let n_args = f.sig.inputs.len();
    let expected = if args.vars.is_some() { 2 } else { 1 };
    if n_args != expected {
        let want = if expected == 2 {
            "fn main(ctx: &mut Ctx, vars: <Vars>) -> Result<()>"
        } else {
            "fn main(ctx: &mut Ctx) -> Result<()>  (add `vars = <Type>` to the attribute to take vars)"
        };
        return Err(Error::new_spanned(
            &f.sig.inputs,
            format!("expected `{want}`, found {n_args} parameter(s)"),
        ));
    }
    if f.sig.output == syn::ReturnType::Default {
        return Err(Error::new_spanned(
            &f.sig,
            "playbook `main` must return `Result<()>`",
        ));
    }
    if let Some(FnArg::Receiver(r)) = f.sig.inputs.first() {
        return Err(Error::new_spanned(r, "playbook `main` is a free function"));
    }

    let user_fn = format_ident!("__rustible_main");
    f.sig.ident = user_fn.clone();
    f.attrs
        .push(syn::parse_quote!(#[allow(clippy::needless_pass_by_ref_mut)]));

    let (entry_body, schema_fn, check_body) = match &args.vars {
        Some(ty) => (
            quote! {
                let vars: #ty = ::rustible::sdk::vars::from_value::<#ty>(raw)?;
                #user_fn(ctx, vars)
            },
            quote! { ::rustible::sdk::vars::schema_for::<#ty> },
            quote! {
                ::rustible::sdk::vars::from_value::<#ty>(raw).map(|_| ())
            },
        ),
        None => (
            quote! {
                let _ = raw;
                #user_fn(ctx)
            },
            quote! { ::rustible::sdk::vars::no_schema },
            quote! {
                let _ = raw;
                Ok(())
            },
        ),
    };

    Ok(quote! {
        #f

        #[doc(hidden)]
        pub fn __rustible_entry(
            ctx: &mut ::rustible::sdk::Ctx,
            raw: ::rustible::sdk::__private::serde_json::Value,
        ) -> ::rustible::sdk::Result<()> {
            #entry_body
        }

        #[doc(hidden)]
        pub fn __rustible_check_vars(
            raw: ::rustible::sdk::__private::serde_json::Value,
        ) -> ::rustible::sdk::Result<()> {
            #check_body
        }

        #[doc(hidden)]
        pub static __RUSTIBLE_PLAYBOOK: ::rustible::sdk::registry::Playbook = ::rustible::sdk::registry::Playbook {
            hosts: #hosts,
            escalate: #escalate,
            ssh_user: #ssh_user,
            schema: #schema_fn,
            entry: __rustible_entry,
            check_vars: __rustible_check_vars,
        };
    })
}

/// Marks a playbook's vars struct (vision doc section 10.3).
///
/// Derives `Deserialize` and `JsonSchema` through the SDK's re-exports (the
/// workspace needs no serde dependency), supports `#[default = <expr>]` on
/// fields, and rejects map and tuple fields: vars are flat. `Option<T>` fields
/// are optional; fields without a default are required.
#[proc_macro_attribute]
pub fn vars(attr: TokenStream, item: TokenStream) -> TokenStream {
    match vars_impl(attr.into(), item.into()) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn vars_impl(
    attr: proc_macro2::TokenStream,
    item: proc_macro2::TokenStream,
) -> syn::Result<proc_macro2::TokenStream> {
    if !attr.is_empty() {
        return Err(Error::new_spanned(
            attr,
            "`#[rustible::vars]` takes no options",
        ));
    }
    let mut st: ItemStruct = syn::parse2(item)?;
    let struct_ident = st.ident.clone();
    let mut default_fns = Vec::new();

    let fields = match &mut st.fields {
        syn::Fields::Named(named) => &mut named.named,
        _ => {
            return Err(Error::new_spanned(
                &st,
                "`#[rustible::vars]` needs a struct with named fields",
            ));
        }
    };

    for field in fields.iter_mut() {
        let ident = field.ident.clone().expect("named");
        reject_non_flat(&field.ty, &ident)?;

        let mut kept = Vec::with_capacity(field.attrs.len());
        for attr in field.attrs.drain(..) {
            if attr.path().is_ident("default") {
                let expr: syn::Expr = match &attr.meta {
                    syn::Meta::NameValue(nv) => nv.value.clone(),
                    syn::Meta::List(l) => syn::parse2(l.tokens.clone())?,
                    syn::Meta::Path(p) => {
                        return Err(Error::new_spanned(
                            p,
                            "use `#[default = <expr>]` on a vars field",
                        ));
                    }
                };
                let fn_ident = format_ident!("__rustible_default_{}_{}", struct_ident, ident);
                let fn_name = fn_ident.to_string();
                let ty = &field.ty;
                let body = default_body(&expr, ty);
                default_fns.push(quote! {
                    #[doc(hidden)]
                    #[allow(non_snake_case, clippy::useless_conversion, clippy::unnecessary_literal_unwrap)]
                    fn #fn_ident() -> #ty { #body }
                });
                kept.push(syn::parse_quote!(#[serde(default = #fn_name)]));
            } else {
                kept.push(attr);
            }
        }
        field.attrs = kept;
    }

    st.attrs.push(syn::parse_quote!(#[derive(
        ::rustible::sdk::__private::serde::Deserialize,
        ::rustible::sdk::__private::schemars::JsonSchema
    )]));
    st.attrs
        .push(syn::parse_quote!(#[serde(crate = "::rustible::sdk::__private::serde")]));
    st.attrs
        .push(syn::parse_quote!(#[schemars(crate = "::rustible::sdk::__private::schemars")]));

    Ok(quote! {
        #st
        #(#default_fns)*
    })
}

/// Make `#[default = <literal>]` do what people expect: a string literal
/// becomes an owned `String` via `Into`, and a bare value for an `Option<T>`
/// field is wrapped in `Some`. Anything else is spliced as written.
fn default_body(expr: &syn::Expr, ty: &Type) -> proc_macro2::TokenStream {
    let is_str_lit = matches!(expr, syn::Expr::Lit(l) if matches!(l.lit, syn::Lit::Str(_)));
    let inner = if is_str_lit {
        quote! { ::core::convert::Into::into(#expr) }
    } else {
        quote! { #expr }
    };
    let is_option =
        matches!(ty, Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "Option"));
    let already_wrapped = match expr {
        syn::Expr::Path(p) => p.path.is_ident("None"),
        syn::Expr::Call(c) => matches!(&*c.func, syn::Expr::Path(p) if p.path.is_ident("Some")),
        _ => false,
    };
    if is_option && !already_wrapped {
        quote! { ::core::option::Option::Some(#inner) }
    } else {
        inner
    }
}

/// Vars are one level deep. Maps and tuples cannot be expressed in an
/// inventory `vars` block; nested structs are caught by the orchestrator's
/// schema check (the macro cannot tell a struct from an enum by name).
fn reject_non_flat(ty: &Type, field: &syn::Ident) -> syn::Result<()> {
    match ty {
        Type::Tuple(t) if !t.elems.is_empty() => Err(Error::new_spanned(
            ty,
            format!("vars field `{field}` is a tuple; vars are flat scalars, lists, or enums"),
        )),
        Type::Path(p) => {
            if let Some(seg) = p.path.segments.last() {
                let name = seg.ident.to_string();
                if matches!(name.as_str(), "HashMap" | "BTreeMap" | "IndexMap") {
                    return Err(Error::new_spanned(
                        ty,
                        format!("vars field `{field}` is a map; vars are flat (vision doc 10.3)"),
                    ));
                }
                if let syn::PathArguments::AngleBracketed(ab) = &seg.arguments {
                    for a in &ab.args {
                        if let syn::GenericArgument::Type(inner) = a {
                            reject_non_flat(inner, field)?;
                        }
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Marks a Docker integration test (vision doc section 8, T2).
///
/// ```no_run
/// # use rustible::prelude::*;
/// # use rustible::sdk::testing::changed_then_ok;
/// # use rustible::std_ops::file::Line;
/// #[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
/// fn line_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
///     changed_then_ok(ctx, "add line", || Line::in_path("/etc/x").create(true).set("hi"))?;
///     Ok(())
/// }
/// # fn main() {}
/// ```
///
/// Expands to a `#[test]` that calls `rustible::sdk::testing::run`: skipped
/// unless `RUSTIBLE_INTEGRATION=1` and docker work; otherwise the test binary
/// is built for musl and the body runs inside each image as root over the
/// real `Local` backend. `images` are stock images run as-is;
/// `systemd_images` (the `SystemdImage` variant, for the systemd ops) are
/// booted with systemd as PID 1 first, see `testing::Image::Systemd` for the
/// images that work. At least one of the two lists is required. The function
/// takes exactly `ctx: &mut Ctx` and returns `Result<()>`.
///
/// The `rustible` facade re-exports this attribute only with its `testing`
/// feature, which a test crate enables in `[dev-dependencies]`.
#[proc_macro_attribute]
pub fn integration_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    match integration_test_impl(attr.into(), item.into()) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn integration_test_impl(
    attr: proc_macro2::TokenStream,
    item: proc_macro2::TokenStream,
) -> syn::Result<proc_macro2::TokenStream> {
    // (image literal, is systemd) in attribute order.
    let mut images: Vec<(LitStr, bool)> = Vec::new();
    let parser = syn::meta::parser(|meta| {
        let systemd = if meta.path.is_ident("images") {
            false
        } else if meta.path.is_ident("systemd_images") {
            true
        } else {
            return Err(meta.error(
                "unknown option; expected `images = [\"debian:12\", ...]` \
                 or `systemd_images = [\"jrei/systemd-debian:12\", ...]`",
            ));
        };
        let arr: syn::ExprArray = meta.value()?.parse()?;
        for elem in arr.elems {
            match elem {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) => images.push((s, systemd)),
                other => {
                    return Err(Error::new_spanned(
                        other,
                        "image lists take string literals like \"debian:12\"",
                    ));
                }
            }
        }
        Ok(())
    });
    parser.parse2(attr)?;
    if images.is_empty() {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            "`#[rustible::integration_test]` needs `images = [\"debian:12\", ...]` \
             and/or `systemd_images = [\"jrei/systemd-debian:12\", ...]`",
        ));
    }
    let images = images.iter().map(|(lit, systemd)| {
        if *systemd {
            quote!(::rustible::sdk::testing::Image::Systemd(#lit))
        } else {
            quote!(::rustible::sdk::testing::Image::Plain(#lit))
        }
    });

    let mut f: ItemFn = syn::parse2(item)?;
    let bad_signature = f.sig.inputs.len() != 1
        || matches!(f.sig.inputs.first(), Some(FnArg::Receiver(_)))
        || f.sig.output == syn::ReturnType::Default;
    if bad_signature {
        return Err(Error::new_spanned(
            &f.sig,
            "expected `fn name(ctx: &mut Ctx) -> Result<()>`",
        ));
    }

    let name = f.sig.ident.clone();
    let inner = format_ident!("__rustible_integration_{}", name);
    f.sig.ident = inner.clone();
    // The author's attributes (`#[ignore]`, `#[should_panic]`, `#[cfg(..)]`,
    // doc comments) belong on the test libtest runs, not on the inner body.
    let outer_attrs: Vec<syn::Attribute> = std::mem::take(&mut f.attrs);
    f.attrs
        .push(syn::parse_quote!(#[allow(clippy::needless_pass_by_ref_mut)]));

    Ok(quote! {
        #f

        #(#outer_attrs)*
        #[::core::prelude::v1::test]
        fn #name() {
            ::rustible::sdk::testing::run(
                &::rustible::sdk::testing::Spec {
                    name: stringify!(#name),
                    module_path: module_path!(),
                    crate_name: env!("CARGO_CRATE_NAME"),
                    manifest_dir: env!("CARGO_MANIFEST_DIR"),
                    images: &[#(#images),*],
                },
                #inner,
            )
        }
    })
}

/// Renders a Jinja2 template to a `String`, on the target.
///
/// ```no_run
/// # use rustible::prelude::*;
/// # use rustible::std_ops::file;
/// # fn f(ctx: &mut Ctx) -> Result<()> {
/// let opts = rustible::template!(
///     "JAVA_OPTS=-Xmx{{ heap }}{% for f in flags %} {{ f }}{% endfor %}",
///     heap = "2G",
///     flags = &["-XX:+UseG1GC"],
/// )?;
/// ctx.step(
///     "java opts",
///     file::Line::in_path("/etc/default/app").matching("^JAVA_OPTS=").set(&opts),
/// )?;
/// # Ok(())
/// # }
/// # fn main() {}
/// ```
///
/// A template is a value, not an operation: its `String` goes into any op
/// (`file::Copy`, `file::Line`, `file::Block`, a command's stdin, an HTTP
/// body), which does the diffing, the dry run, the mode and the owner as it
/// does for any other text. Plain files are never templated:
/// `file::Copy::from_str(include_str!("files/motd"))` copies `{{ }}` as it
/// is. Only `template!` renders.
///
/// The language is Jinja2, as [minijinja](https://docs.rs/minijinja/3.0.0)
/// implements it. Compute in Rust, render in the template: the playbook
/// prepares the data, typed and testable, and the template lays it out. A
/// template that grows logic is a sign the logic belongs in the playbook.
///
/// # The template
///
/// The first argument says where the text comes from:
///
/// - **A string literal**: `template!("Hello {{ name }}", name = "x")`.
/// - **`include_str!("files/Caddyfile.j2")`**, a file, with the path
///   relative to the file the call is in, as `include_str!` resolves it. The
///   macro recognises `include_str!` and reads the file itself, so the path
///   must be a single string literal. (One difference: inside a
///   `macro_rules!` defined in another file, the path resolves from the file
///   that defines the macro, where `include_str!` would use the caller's.)
/// - **Any other expression**: a `&str` (or anything `AsRef<str>`) known only
///   at run time, such as a var or a fetched file. This is a *dynamic*
///   template.
///
/// A file that is rendered keeps its real name plus `.j2` (`Caddyfile.j2`,
/// `script.sh.j2`), so a reader knows before opening it that `{{ }}` in it
/// means something. That is a convention, not a rule: any name works.
///
/// # The variables
///
/// `name = <value>` passes one variable; the value is anything that
/// implements `serde::Serialize`, borrowed, and the template sees what it
/// serializes to: a struct is a map of its fields, in declaration order, a
/// `Vec` or a slice is a list, an array a tuple (both iterate the same), an
/// `Option` is the value or `none`. Any name can be passed, a Rust keyword
/// too (`type = "x"`): no argument name is reserved. A value that fails to
/// serialize, at any depth, fails the render naming the argument.
///
/// `..<value>` spreads a map (a struct, a `HashMap`, a `json!` object) into
/// the variables, for names known only at run time. A named argument wins
/// over the spread's key of the same name, wherever it is written. There is
/// at most one spread.
///
/// ```no_run
/// # use rustible::prelude::*;
/// # fn f() -> Result<()> {
/// let settings = rustible::json!({ "listen": "0.0.0.0:80", "env": "dev" });
/// let app = rustible::template!("{{ listen }} {{ env }}", ..&settings, env = "prod")?;
/// assert_eq!(app, "0.0.0.0:80 prod");
/// # Ok(())
/// # }
/// # fn main() {}
/// ```
///
/// Typed data derives `Serialize` through the re-export, which needs serde's
/// `crate` attribute to find it (`#[rustible::vars]` does the same for
/// `Deserialize`); a workspace that has `serde` as its own dependency can
/// derive it directly instead. Ad hoc nested data needs no type:
/// `rustible::json!` builds it.
///
/// ```no_run
/// # use rustible::prelude::*;
/// #[derive(rustible::serde::Serialize)]
/// #[serde(crate = "rustible::serde")]
/// struct Vhost {
///     host: String,
///     upstream: String,
/// }
///
/// # fn f() -> Result<()> {
/// let vhosts = vec![Vhost { host: "a.example.com".into(), upstream: "127.0.0.1:8080".into() }];
/// let ops = [rustible::json!({ "name": "alice", "level": 4 })];
/// let text = rustible::template!(
///     "{% for v in vhosts %}{{ v.host }} -> {{ v.upstream }}\n{% endfor %}\
///      {% for op in ops %}{{ op.name }} ({{ op.level }})\n{% endfor %}",
///     vhosts = &vhosts,
///     ops = &ops,
/// )?;
/// # Ok(())
/// # }
/// # fn main() {}
/// ```
///
/// A `Secret` (`rustible::prelude::Secret`) passed in renders as its
/// plaintext, and the run masks it in every event, diffs
/// included, as it masks a secret anywhere. A filter that rewrites it
/// (`upper`, `replace`) produces text the masker does not know.
///
/// # When a template is checked
///
/// **When the playbook compiles**, for a literal or an `include_str!`
/// template:
///
/// - the syntax of the template and of every file it includes, extends or
///   imports, at any depth;
/// - that every name the top-level template reads is passed (unless there
///   is a spread, which may hold it), a name in an `include`'s inline `if`
///   included. A name the template assigns itself (`set`, `for`, `with`, a
///   macro, an import) is not required, so `{% set port = port |
///   default(80) %}` works with `port` passed or not. Names in an included,
///   extended or imported file are not checked against the arguments: they
///   can come from the scope at the include site (a `for` loop's variable,
///   a `set`, a macro's arguments), which only the render knows. A name a
///   parent template (`extends`) assigns is not required of the child
///   either, since minijinja runs a block in the scope it is called from;
/// - that every name passed is read somewhere: by the top-level template or
///   by a file it reaches;
/// - that every `include`, `extends` and `import` target exists.
///
/// **When it renders**, on the target, where the macro is called: everything
/// else. A name used but missing in an included file, a field that is not
/// there (`{{ v.upstrem }}`), an attribute of `none`, a filter given the
/// wrong type, an unknown filter, and all of a dynamic template. A render
/// error returns from `template!` before the step that would write the text
/// runs, so nothing is written. `--check` renders every template the dry
/// run reaches, so it finds their errors without touching the machine; a
/// template whose data is a would-change step's output is not reached,
/// because reading that output ends its `ctx.block` under `--check`. The
/// error names the template (its file relative to the playbook crate, or
/// `inline template at <file>:<line>`, or `dynamic template at
/// <file>:<line>`) and the line, the undefined expression (except inside a
/// macro, where minijinja gives only the line), and for an error inside an
/// included file or a parent's block (`{{ super() }}`), the sites it was
/// reached through.
///
/// **Strict undefined**: a missing name or attribute is an error, never
/// empty text. A literal or `include_str!` template must be passed every
/// name it reads, so an optional value is an `Option`: `None` is `none`,
/// `{% if x %}` is false for it, and `{{ x | default("off", true) }}`
/// replaces it (`default("off")` alone keeps the `none`). `is defined` and
/// `default(..)` are for names that may be absent: a spread's, an included
/// file's, a dynamic template's.
///
/// Values print as Jinja prints them, in Python's spelling: `true` as
/// `True`, `None` as `None`, a list as `['a', 'b']`, an array as `('a',
/// 'b')`. A config file that wants `true` or JSON says `{{ x | lower }}` or
/// `{{ x | tojson }}`.
///
/// # Whitespace and escaping
///
/// `trim_blocks` is on, as in Ansible's `template` module: the newline after
/// a `{% … %}` tag is removed, so a line holding only a tag leaves no blank
/// line. A template keeps its trailing newlines, and so does an included
/// file, so `{% include "vhost.j2" %}` on a line of its own renders exactly
/// the file's lines; a fragment meant to sit mid-line is written without a
/// final newline. `{%-` and `-%}` strip the whitespace before or after a
/// tag. There is no autoescaping, whatever the file is called: these are
/// config files.
///
/// # Includes, inheritance and imports
///
/// `{% include "x.j2" %}`, `{% extends "base.j2" %}`, `{% import "m.j2" as
/// m %}` and `{% from "m.j2" import f %}` work in a compile-time template:
///
/// - the target must be a string literal, or for `include` a list of them
///   (`{% include ["a.j2", "b.j2"] %}`, the first that exists), or an inline
///   `if` between literals with an `else` (`"a.j2" if x else "b.j2"`).
///   Anything else is a compile error;
/// - a target resolves **relative to the file that contains the
///   statement**, at any depth; a literal's targets resolve relative to the
///   playbook file. `..` works, an absolute path is read on the machine that
///   compiles, and a symlinked template resolves its own targets from where
///   the link points. No folder layout is imposed;
/// - every file reached is embedded in the playbook binary, once, and cargo
///   rebuilds the playbook when any of them changes. A file an `ignore
///   missing` (or an include list) names but that does not exist is not
///   watched: after creating it, touch the playbook so it rebuilds;
/// - a target that does not exist is a compile error, unless the include
///   says `ignore missing`;
/// - every target is embedded, including both branches of a conditional
///   `extends` and a statement in a branch that never runs;
/// - a template may include or import itself, or another that comes back
///   to it (a tree rendered by its data); an infinite recursion fails the
///   render at minijinja's recursion limit. A chain of `extends` that comes
///   back to a template it started from is a compile error.
///
/// What they do, as minijinja implements Jinja:
///
/// - In a child template that `extends`, only `{% block %}` contents are
///   output; text outside the blocks is dropped. `{{ super() }}` in a block
///   renders the parent's version of it.
/// - An included file sees the variables in scope where it is included (a
///   loop's variable, a `set`). Blocks defined in an included file override
///   nothing.
/// - Unlike Jinja2, an included file's `{% set %}` replaces the includer's
///   variable of the same name, a passed argument included, unless the
///   include sits inside a `for` loop, whose scope keeps it. Give what an
///   included file sets a name nothing else uses.
/// - Unlike Jinja2, an imported file's macros see the variables the
///   importer was rendered with: minijinja accepts `with context` and
///   `without context` and ignores both.
///
/// **A dynamic template reads no file.** Any `include`, `extends`, `import`
/// or `from … import` in it fails the render with "includes are only
/// available for templates known at compile time", before anything is read
/// and wherever the statement is (an untaken branch, `ignore missing`): a
/// template whose text comes from data could otherwise read the target's
/// files, as root, into the step. Read what it needs in Rust
/// (`ctx.sys().read_to_string(..)`) and pass it in as a variable.
///
/// # Filters, tests and functions
///
/// Every built-in minijinja ships, and nothing else. Ansible's own filters
/// (`to_nice_json`, `regex_replace`, `b64encode`, …) are not provided; a
/// template that uses one fails with "unknown filter", and the logic usually
/// belongs in the playbook anyway.
///
/// - **Filters**: `abs`, `attr`, `batch`, `bool`, `capitalize`, `chain`,
///   `count`, `d`, `default`, `dictsort`, `e`, `escape`, `first`, `float`,
///   `format`, `groupby`, `indent`, `int`, `items`, `join`, `last`,
///   `length`, `lines`, `list`, `lower`, `map`, `max`, `min`, `pprint`,
///   `reject`, `rejectattr`, `replace`, `reverse`, `round`, `safe`,
///   `select`, `selectattr`, `slice`, `sort`, `split`, `string`, `sum`,
///   `title`, `tojson`, `trim`, `unique`, `upper`, `urlencode`, `zip`.
/// - **Tests**: `boolean`, `defined`, `divisibleby`, `endingwith`, `eq`
///   (`equalto`, `==`), `escaped`, `even`, `false`, `filter`, `float`, `ge`
///   (`>=`), `gt` (`greaterthan`, `>`), `in`, `int`, `integer`, `iterable`,
///   `le` (`<=`), `lower`, `lt` (`lessthan`, `<`), `mapping`, `ne` (`!=`),
///   `none`, `number`, `odd`, `safe`, `sameas`, `sequence`, `startingwith`,
///   `string`, `test`, `true`, `undefined`, `upper`. The operator spellings
///   are for `select` and `reject`: `xs | select(">", 2)`.
/// - **Functions**: `range`, `dict`, `namespace`, `debug`.
/// - **Tags**: `for` (with `loop`, `loop.previtem`, `loop.nextitem`, and
///   `break` and `continue`), `if`, `set`, `with`, `filter`, `macro`,
///   `call`, `do`, `autoescape`, `raw`, `block`, and the loads above.
///
/// minijinja's documentation describes each:
/// <https://docs.rs/minijinja/3.0.0/minijinja/filters/index.html>,
/// <https://docs.rs/minijinja/3.0.0/minijinja/tests/index.html>,
/// <https://docs.rs/minijinja/3.0.0/minijinja/functions/index.html>, and
/// the syntax at
/// <https://docs.rs/minijinja/3.0.0/minijinja/syntax/index.html>.
///
/// # Reuse
///
/// A template is used where it is written, so the step shows the template,
/// its variables and where the text goes. To reuse one, write a `fn` that
/// returns `template!(..)`, and move it to `src/lib.rs` once a second
/// playbook needs it.
#[proc_macro]
pub fn template(input: TokenStream) -> TokenStream {
    match template::expand(input.into()) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}
