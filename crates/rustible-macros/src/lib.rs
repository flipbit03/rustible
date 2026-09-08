//! Proc macros for Rustible. Not used directly: the `rustible` facade
//! re-exports them as `rustible::playbook` and `rustible::vars`.
//!
//! Everything the expansions reference lives under `::rustible::sdk`, so a
//! workspace only needs the `rustible` crate as a dependency.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::{Error, FnArg, ItemFn, ItemStruct, LitBool, LitStr, Type};

/// Marks a playbook's `main` (vision doc sections 6.1, 9, 10.3).
///
/// ```ignore
/// #[rustible::playbook(hosts = "web", vars = Vars, escalate = true)]
/// fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> { .. }
/// ```
///
/// `hosts` is required. `vars` names a `#[rustible::vars]` struct and adds a
/// second parameter to `main`. `escalate` (Ansible's `become`) defaults to
/// false. Expands to a private renamed `main`, a `__rustible_entry` that
/// deserializes the vars, and a `__RUSTIBLE_PLAYBOOK` static the build
/// script's registry points at.
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
}

fn playbook_impl(
    attr: proc_macro2::TokenStream,
    item: proc_macro2::TokenStream,
) -> syn::Result<proc_macro2::TokenStream> {
    let mut args = PlaybookArgs {
        hosts: None,
        vars: None,
        escalate: None,
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
        } else {
            let key = meta
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_else(|| "?".into());
            Err(meta.error(format!(
                "unknown playbook option `{key}`; expected `hosts`, `vars`, or `escalate`"
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

    let (entry_body, schema_fn) = match &args.vars {
        Some(ty) => (
            quote! {
                let vars: #ty = ::rustible::sdk::vars::from_value::<#ty>(raw)?;
                #user_fn(ctx, vars)
            },
            quote! { ::rustible::sdk::vars::schema_for::<#ty> },
        ),
        None => (
            quote! {
                let _ = raw;
                #user_fn(ctx)
            },
            quote! { ::rustible::sdk::vars::no_schema },
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
        pub static __RUSTIBLE_PLAYBOOK: ::rustible::sdk::registry::Playbook = ::rustible::sdk::registry::Playbook {
            hosts: #hosts,
            escalate: #escalate,
            schema: #schema_fn,
            entry: __rustible_entry,
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
                default_fns.push(quote! {
                    #[doc(hidden)]
                    #[allow(non_snake_case)]
                    fn #fn_ident() -> #ty { #expr }
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
