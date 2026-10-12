//! `template!`: the compile-time half. Parses the call, finds every file a
//! compile-time template can reach, checks what can be checked before the
//! playbook runs, and expands to a call into `rustible_sdk::template`, which
//! renders on the target.
//!
//! minijinja does the parsing (`machinery::parse`, the same parser the
//! target renders with), the syntax check and the name analysis
//! (`Template::undeclared_variables`). The one thing written here is the walk
//! over minijinja's syntax tree that collects `include`, `extends` and
//! `import` targets and the names a template assigns. The tree is
//! minijinja's unstable API, which is why the workspace pins minijinja to
//! one version: re-check [`walk`] against `minijinja::machinery::ast` on
//! every upgrade, since a new statement kind that holds a body must be
//! walked or the loads inside it go unembedded.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use minijinja::machinery::ast::{Expr, Stmt};
use minijinja::syntax::SyntaxConfig;
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::{Error, Ident, LitStr, Token};

/// `template!(<source>, name = <expr>, .., ..<spread>)`.
struct Input {
    source: syn::Expr,
    args: Vec<Arg>,
}

enum Arg {
    Named(Ident, syn::Expr),
    Spread(syn::Expr),
}

impl Parse for Input {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.is_empty() {
            return Err(input.error(
                "`template!` needs a template: a string literal, `include_str!(\"file.j2\")`, \
                 or a `&str`",
            ));
        }
        let source: syn::Expr = input.parse()?;
        let mut args = Vec::new();
        while !input.is_empty() {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            if input.peek(Token![..]) {
                input.parse::<Token![..]>()?;
                args.push(Arg::Spread(input.parse()?));
            } else {
                // Any identifier, keywords included, so a template variable
                // may be called `type` or `match`: no name is reserved.
                let name = Ident::parse_any(input)?;
                input.parse::<Token![=]>().map_err(|e| {
                    Error::new(
                        e.span(),
                        "expected `name = <value>` or `..<spread>` after the template",
                    )
                })?;
                args.push(Arg::Named(name, input.parse()?));
            }
        }
        Ok(Input { source, args })
    }
}

/// Where the template's text comes from.
enum Source {
    /// A string literal at the call site.
    Inline(LitStr),
    /// `include_str!("<path>")`.
    File(LitStr),
    /// Anything else: a `&str` known at run time.
    Dynamic(syn::Expr),
}

fn classify(expr: &syn::Expr) -> syn::Result<Source> {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => Ok(Source::Inline(s.clone())),
        syn::Expr::Group(g) => classify(&g.expr),
        syn::Expr::Macro(m)
            if m.mac
                .path
                .segments
                .last()
                .is_some_and(|s| s.ident == "include_str") =>
        {
            let path: LitStr = m.mac.parse_body().map_err(|_| {
                Error::new_spanned(
                    &m.mac,
                    "`template!` reads an `include_str!` template itself, to check it and to \
                     embed the files it includes, so the path must be one string literal: \
                     `include_str!(\"files/app.conf.j2\")`",
                )
            })?;
            Ok(Source::File(path))
        }
        other => Ok(Source::Dynamic(other.clone())),
    }
}

/// The macro's entry point.
pub(crate) fn expand(input: TokenStream) -> syn::Result<TokenStream> {
    let Input { source, args } = syn::parse2(input)?;

    let mut spread = None;
    let mut named: Vec<(String, &Ident, &syn::Expr)> = Vec::new();
    for arg in &args {
        match arg {
            Arg::Spread(e) => {
                if spread.is_some() {
                    return Err(Error::new_spanned(
                        e,
                        "`template!` takes one `..` spread; merge the maps in Rust first",
                    ));
                }
                spread = Some(e);
            }
            Arg::Named(ident, e) => {
                let name = ident.unraw().to_string();
                if named.iter().any(|(n, _, _)| *n == name) {
                    return Err(Error::new_spanned(
                        ident,
                        format!("`{name}` is passed to `template!` twice"),
                    ));
                }
                named.push((name, ident, e));
            }
        }
    }

    let vars = {
        let mut chain = quote! { ::rustible::sdk::template::__Vars::new() };
        for arg in &args {
            chain = match arg {
                Arg::Spread(e) => quote! { #chain.spread(&(#e)) },
                Arg::Named(ident, e) => {
                    let name = ident.unraw().to_string();
                    quote! { #chain.named(#name, &(#e)) }
                }
            };
        }
        chain
    };

    let base = manifest_dir();
    match classify(&source)? {
        Source::Dynamic(expr) => {
            let name = match call_location(Span::call_site(), base.as_deref()) {
                Some(at) => format!("dynamic template at {at}"),
                None => "dynamic template".to_string(),
            };
            Ok(quote! {
                ::rustible::sdk::template::__render_dynamic(
                    #name,
                    ::core::convert::AsRef::<str>::as_ref(&(#expr)),
                    #vars,
                )
            })
        }
        Source::Inline(lit) => {
            let name = match call_location(lit.span(), base.as_deref()) {
                Some(at) => format!("inline template at {at}"),
                None => "inline template".to_string(),
            };
            let dir = local_file(lit.span()).and_then(|f| f.parent().map(Path::to_path_buf));
            let root = Root {
                name,
                source: lit.value(),
                dir,
                path: None,
            };
            let tree =
                Tree::resolve(root, base.as_deref()).map_err(|m| Error::new_spanned(&lit, m))?;
            tree.check_names(spread.is_some(), &named, &source)?;
            Ok(tree.expand(quote! { #lit }, vars))
        }
        Source::File(path) => {
            let Some(here) = local_file(path.span()) else {
                return Err(Error::new_spanned(
                    &path,
                    "`template!` cannot tell which file this call is in, so it cannot resolve \
                     the `include_str!` path relative to it",
                ));
            };
            let dir = here.parent().map(Path::to_path_buf).unwrap_or_default();
            let wanted = dir.join(path.value());
            let canon = wanted.canonicalize().map_err(|e| {
                Error::new_spanned(
                    &path,
                    format!("`template!` cannot read `{}`: {e}", wanted.display()),
                )
            })?;
            let text = std::fs::read_to_string(&canon).map_err(|e| {
                Error::new_spanned(
                    &path,
                    format!("`template!` cannot read `{}`: {e}", canon.display()),
                )
            })?;
            let root = Root {
                name: display(&canon, base.as_deref()),
                source: text,
                dir: canon.parent().map(Path::to_path_buf),
                path: Some(canon.clone()),
            };
            let tree =
                Tree::resolve(root, base.as_deref()).map_err(|m| Error::new_spanned(&source, m))?;
            tree.check_names(spread.is_some(), &named, &source)?;
            let abs = canon.to_string_lossy().into_owned();
            Ok(tree.expand(quote! { ::core::include_str!(#abs) }, vars))
        }
    }
}

/// The playbook crate's directory, canonical: template names are shown
/// relative to it.
fn manifest_dir() -> Option<PathBuf> {
    std::env::var_os("CARGO_MANIFEST_DIR").and_then(|d| PathBuf::from(d).canonicalize().ok())
}

/// The file a span is in, as the compiler names it (possibly relative to
/// its working directory), when it knows. Not canonical: a playbook that is a
/// symlink resolves `include_str!` next to the link, so its targets do too.
fn local_file(span: Span) -> Option<PathBuf> {
    span.unwrap().local_file()
}

/// `<file>:<line>` of a span, the file relative to the crate.
fn call_location(span: Span, base: Option<&Path>) -> Option<String> {
    let file = local_file(span)?.canonicalize().ok()?;
    Some(format!(
        "{}:{}",
        display(&file, base),
        span.unwrap().start().line()
    ))
}

/// A template's name: its path relative to the crate, `/`-separated, or
/// the absolute path of a file outside it.
fn display(path: &Path, base: Option<&Path>) -> String {
    base.and_then(|b| path.strip_prefix(b).ok())
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// The top-level template.
struct Root {
    name: String,
    source: String,
    /// The directory its includes resolve from: the template file's own,
    /// or for a literal, the playbook file's.
    dir: Option<PathBuf>,
    /// Its file, for a template read from one.
    path: Option<PathBuf>,
}

/// Every template a compile-time template reaches.
struct Tree {
    files: Vec<File>,
    /// `(from, as written, to)`, by index into `files`.
    links: Vec<(usize, String, usize)>,
    /// `(from, to)` for every `extends` target, by index into `files`.
    extends: Vec<(usize, usize)>,
}

struct File {
    name: String,
    source: String,
    /// The file on disk; `None` for an inline literal.
    path: Option<PathBuf>,
    dir: Option<PathBuf>,
    /// The template that first reached this one, and the line of the
    /// statement that did.
    reached_from: Option<(usize, usize)>,
    /// What [`walk`] collected: the names it assigns and reads.
    walked: Walk,
    /// Names the template looks up without assigning them first, by
    /// minijinja's own analysis.
    undeclared: HashSet<String>,
}

/// One `include`, `extends`, `import` or `from … import`.
struct Load {
    statement: &'static str,
    targets: Vec<String>,
    /// `ignore missing`, or an include list (whose entries Jinja tries in
    /// order, so only all of them missing fails).
    may_miss: bool,
    list: bool,
    line: usize,
}

impl Tree {
    /// Read and parse every template `root` reaches, resolving each target
    /// relative to the file that names it. A file reached twice is read
    /// once.
    fn resolve(root: Root, base: Option<&Path>) -> Result<Tree, String> {
        let mut tree = Tree {
            files: vec![File {
                name: root.name,
                source: root.source,
                path: root.path,
                dir: root.dir,
                reached_from: None,
                walked: Walk::default(),
                undeclared: HashSet::new(),
            }],
            links: Vec::new(),
            extends: Vec::new(),
        };
        let mut by_path: HashMap<PathBuf, usize> = HashMap::new();
        if let Some(p) = &tree.files[0].path {
            by_path.insert(p.clone(), 0);
        }

        let mut next = 0;
        while next < tree.files.len() {
            let index = next;
            next += 1;
            let (mut walked, undeclared) = {
                let file = &tree.files[index];
                let ast = minijinja::machinery::parse(&file.source, &file.name, syntax())
                    .map_err(|e| tree.syntax_error(index, &e))?;
                let mut walked = Walk::default();
                walk(&ast, &mut walked).map_err(|(line, why)| tree.at(index, line, &why))?;
                let env = environment();
                let template = env
                    .template_from_named_str(&file.name, &file.source)
                    .map_err(|e| tree.syntax_error(index, &e))?;
                (walked, template.undeclared_variables(false))
            };
            let loads = std::mem::take(&mut walked.loads);
            tree.files[index].walked = walked;
            tree.files[index].undeclared = undeclared;

            for load in loads {
                let Some(dir) = tree.files[index].dir.clone() else {
                    return Err(tree.at(
                        index,
                        load.line,
                        &format!(
                            "`{{% {} %}}` needs to know which file this template is in, to \
                             resolve its target relative to it, and the compiler does not say",
                            load.statement
                        ),
                    ));
                };
                let mut found = 0;
                let mut missing = Vec::new();
                for target in &load.targets {
                    let wanted = dir.join(target);
                    let Ok(canon) = wanted.canonicalize() else {
                        missing.push(wanted);
                        continue;
                    };
                    found += 1;
                    let to = match by_path.get(&canon) {
                        Some(&to) => to,
                        None => {
                            let source = std::fs::read_to_string(&canon).map_err(|e| {
                                tree.at(
                                    index,
                                    load.line,
                                    &format!("cannot read `{}`: {e}", canon.display()),
                                )
                            })?;
                            tree.files.push(File {
                                name: display(&canon, base),
                                source,
                                dir: canon.parent().map(Path::to_path_buf),
                                path: Some(canon.clone()),
                                reached_from: Some((index, load.line)),
                                walked: Walk::default(),
                                undeclared: HashSet::new(),
                            });
                            by_path.insert(canon, tree.files.len() - 1);
                            tree.files.len() - 1
                        }
                    };
                    if !tree
                        .links
                        .iter()
                        .any(|(f, w, _)| *f == index && w == target)
                    {
                        tree.links.push((index, target.clone(), to));
                    }
                    if load.statement == "extends" && !tree.extends.contains(&(index, to)) {
                        tree.extends.push((index, to));
                    }
                }
                let fails = if load.list {
                    found == 0 && !load.may_miss
                } else {
                    !missing.is_empty() && !load.may_miss
                };
                if fails {
                    let names: Vec<String> = missing
                        .iter()
                        .map(|p| format!("`{}`", p.display()))
                        .collect();
                    return Err(tree.at(
                        index,
                        load.line,
                        &format!(
                            "`{{% {} %}}`: no such file: {} (a target resolves relative to the \
                             file that names it; add `ignore missing` to an `include` that may \
                             be absent)",
                            load.statement,
                            names.join(", ")
                        ),
                    ));
                }
            }
        }
        Ok(tree)
    }

    /// A message about line `line` of template `index`, with the chain of
    /// includes that reached it.
    fn at(&self, index: usize, line: usize, why: &str) -> String {
        format!(
            "{}: {why}{}",
            located(&self.files[index].name, line),
            self.reached(index)
        )
    }

    fn syntax_error(&self, index: usize, e: &minijinja::Error) -> String {
        let detail = e.detail().unwrap_or("invalid template");
        format!(
            "{}: {}: {detail}{}",
            located(&self.files[index].name, e.line().unwrap_or(0)),
            e.kind(),
            self.reached(index)
        )
    }

    /// ` (included from `a` line 3, from `b` line 1)`, or nothing for the
    /// top-level template.
    fn reached(&self, mut index: usize) -> String {
        let mut sites = Vec::new();
        while let Some((from, line)) = self.files[index].reached_from {
            sites.push(located(&self.files[from].name, line));
            index = from;
        }
        if sites.is_empty() {
            String::new()
        } else {
            format!(" (reached from {})", sites.join(", from "))
        }
    }

    /// The compile-time name checks: every name the top-level template
    /// uses is passed (unless a spread may hold it), and every name passed
    /// is used somewhere in the tree.
    fn check_names(
        &self,
        has_spread: bool,
        named: &[(String, &Ident, &syn::Expr)],
        source: &syn::Expr,
    ) -> syn::Result<()> {
        let passed: HashSet<&str> = named.iter().map(|(n, _, _)| n.as_str()).collect();
        let globals: HashSet<String> = environment()
            .globals()
            .map(|(n, _)| n.to_string())
            .collect();

        // Names left to the render rather than required of the call:
        // - one the top-level template assigns anywhere (`set`, `for`,
        //   `with`, a macro or an import): minijinja's analysis takes a name
        //   assigned in one branch for undeclared in the next, and the
        //   `{% set port = port | default(80) %}` idiom reads a name it also
        //   assigns;
        // - one a parent assigns (any file reached through `extends`,
        //   transitively): minijinja runs a child's block in the scope it is
        //   called from, so the block can use the parent's loop variable.
        let root = &self.files[0];
        let mut parents = vec![0];
        let mut reached = HashSet::from([0]);
        while let Some(child) = parents.pop() {
            for &(from, to) in &self.extends {
                if from == child && reached.insert(to) {
                    parents.push(to);
                }
            }
        }
        let assigned: HashSet<&str> = reached
            .iter()
            .flat_map(|&i| self.files[i].walked.assigned.iter().map(String::as_str))
            .collect();

        if !has_spread {
            // minijinja's analysis plus the condition of a load target
            // (`{% include "a.j2" if flag else "b.j2" %}`), which it does not
            // look at.
            let missing: BTreeSet<&str> = root
                .undeclared
                .iter()
                .chain(&root.walked.target_vars)
                .map(String::as_str)
                .filter(|n| !passed.contains(n) && !globals.contains(*n) && !assigned.contains(n))
                .collect();
            if !missing.is_empty() {
                let list: Vec<String> = missing.iter().map(|n| format!("`{n}`")).collect();
                let first = missing.iter().next().expect("not empty");
                return Err(Error::new_spanned(
                    source,
                    format!(
                        "{} uses {}, not passed to `template!`: add `{first} = <value>`, \
                         or a `..` spread that holds {}",
                        shown(&root.name),
                        list.join(", "),
                        if missing.len() == 1 { "it" } else { "them" }
                    ),
                ));
            }
        }

        // Every variable any file of the tree reads, wherever: a name in
        // here may be the argument's, so the argument is used.
        let used: HashSet<&str> = self
            .files
            .iter()
            .flat_map(|f| f.walked.vars.iter().map(String::as_str))
            .collect();
        for (name, ident, _) in named {
            if !used.contains(name.as_str()) {
                let scope = if self.files.len() > 1 {
                    " or by any template it includes, extends or imports"
                } else {
                    ""
                };
                return Err(Error::new_spanned(
                    ident,
                    format!(
                        "`{name}` is passed to `template!` but not used by {}{scope}",
                        shown(&self.files[0].name)
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The expansion: every file embedded with `include_str!` (so cargo
    /// rebuilds when one changes), the links, and the render call.
    fn expand(&self, root_source: TokenStream, vars: TokenStream) -> TokenStream {
        let root_name = &self.files[0].name;
        let files = self.files.iter().enumerate().map(|(i, f)| {
            let name = &f.name;
            if i == 0 {
                quote! { (#name, #root_source) }
            } else {
                let abs = f
                    .path
                    .as_ref()
                    .expect("every reached template is a file")
                    .to_string_lossy()
                    .into_owned();
                quote! { (#name, ::core::include_str!(#abs)) }
            }
        });
        let links = self.links.iter().map(|(from, written, to)| {
            let from = &self.files[*from].name;
            let to = &self.files[*to].name;
            quote! { (#from, #written, #to) }
        });
        quote! {
            {
                static __RUSTIBLE_TEMPLATE: ::rustible::sdk::template::__Embedded =
                    ::rustible::sdk::template::__Embedded {
                        name: #root_name,
                        files: &[#(#files),*],
                        links: &[#(#links),*],
                    };
                ::rustible::sdk::template::__render_embedded(&__RUSTIBLE_TEMPLATE, #vars)
            }
        }
    }
}

/// A template's name as a message shows it, the way the target's render
/// errors do (`rustible_sdk::template`): ``template `files/x.j2` `` for a
/// file, the name itself for a literal, which is named by its call.
fn shown(name: &str) -> String {
    if name.starts_with("inline template") {
        name.to_string()
    } else {
        format!("template `{name}`")
    }
}

/// [`shown`], at a line.
fn located(name: &str, line: usize) -> String {
    if name.starts_with("inline template") {
        format!("{name}, line {line}")
    } else {
        format!("{} line {line}", shown(name))
    }
}

/// The environment the target renders with, as far as compiling a template
/// goes: its syntax, and its globals for the name check.
fn environment<'s>() -> minijinja::Environment<'s> {
    let mut env = minijinja::Environment::new();
    env.set_syntax(syntax());
    env
}

/// The target's syntax (`rustible_sdk::template`): minijinja's delimiters,
/// `trim_blocks` and `keep_trailing_newline`, neither of which changes what
/// parses.
fn syntax() -> SyntaxConfig {
    SyntaxConfig::builder()
        .trim_blocks(true)
        .keep_trailing_newline(true)
        .build()
        .expect("the default delimiters are valid")
}

/// A walk's failure: the line, and why.
type WalkResult = Result<(), (usize, String)>;

/// What a walk over one template collects.
#[derive(Default)]
struct Walk {
    loads: Vec<Load>,
    /// Names the template assigns: `set`, `for`, `with`, macro and import
    /// names.
    assigned: HashSet<String>,
    /// Every variable the template reads anywhere, a load target's
    /// condition included: a name in here is used.
    vars: HashSet<String>,
    /// The variables a load target's condition reads (`{% include "a.j2"
    /// if flag else "b.j2" %}`), which minijinja's own name analysis does
    /// not look at.
    target_vars: HashSet<String>,
}

/// Collect the loads, assigned names and read variables of a statement and
/// everything inside it. Every statement that holds a body is walked, so a
/// load in an untaken branch, a macro or a block is found too: all of them
/// are embedded. The match is exhaustive on purpose: a statement kind a
/// minijinja upgrade adds fails to compile here until it is walked.
fn walk(stmt: &Stmt, w: &mut Walk) -> WalkResult {
    let body = |stmts: &[Stmt], w: &mut Walk| stmts.iter().try_for_each(|s| walk(s, w));
    match stmt {
        Stmt::Template(t) => body(&t.children, w),
        Stmt::EmitExpr(e) => {
            vars(&e.expr, &mut w.vars);
            Ok(())
        }
        Stmt::EmitRaw(_) | Stmt::Continue(_) | Stmt::Break(_) => Ok(()),
        Stmt::Do(d) => {
            call_vars(&d.call, &mut w.vars);
            Ok(())
        }
        Stmt::ForLoop(f) => {
            assign(&f.target, &mut w.assigned);
            w.assigned.insert("loop".to_string());
            vars(&f.iter, &mut w.vars);
            if let Some(e) = &f.filter_expr {
                vars(e, &mut w.vars);
            }
            body(&f.body, w)?;
            body(&f.else_body, w)
        }
        Stmt::IfCond(i) => {
            vars(&i.expr, &mut w.vars);
            body(&i.true_body, w)?;
            body(&i.false_body, w)
        }
        Stmt::WithBlock(b) => {
            for (target, value) in &b.assignments {
                assign(target, &mut w.assigned);
                vars(value, &mut w.vars);
            }
            body(&b.body, w)
        }
        Stmt::Set(s) => {
            assign(&s.target, &mut w.assigned);
            vars(&s.expr, &mut w.vars);
            Ok(())
        }
        Stmt::SetBlock(s) => {
            assign(&s.target, &mut w.assigned);
            if let Some(f) = &s.filter {
                vars(f, &mut w.vars);
            }
            body(&s.body, w)
        }
        Stmt::AutoEscape(a) => {
            vars(&a.enabled, &mut w.vars);
            body(&a.body, w)
        }
        Stmt::FilterBlock(f) => {
            vars(&f.filter, &mut w.vars);
            body(&f.body, w)
        }
        Stmt::Block(b) => body(&b.body, w),
        Stmt::Macro(m) => {
            w.assigned.insert(m.name.to_string());
            m.args.iter().for_each(|a| assign(a, &mut w.assigned));
            m.defaults.iter().for_each(|d| vars(d, &mut w.vars));
            body(&m.body, w)
        }
        Stmt::CallBlock(c) => {
            call_vars(&c.call, &mut w.vars);
            w.assigned.insert("caller".to_string());
            c.macro_decl
                .args
                .iter()
                .for_each(|a| assign(a, &mut w.assigned));
            c.macro_decl
                .defaults
                .iter()
                .for_each(|d| vars(d, &mut w.vars));
            body(&c.macro_decl.body, w)
        }
        Stmt::Extends(e) => load("extends", &e.name, false, false, line_of(e.span()), w),
        Stmt::Include(i) => load(
            "include",
            &i.name,
            i.ignore_missing,
            true,
            line_of(i.span()),
            w,
        ),
        Stmt::Import(i) => {
            assign(&i.name, &mut w.assigned);
            load("import", &i.expr, false, false, line_of(i.span()), w)
        }
        Stmt::FromImport(f) => {
            for (name, alias) in &f.names {
                assign(alias.as_ref().unwrap_or(name), &mut w.assigned);
            }
            load("from", &f.expr, false, false, line_of(f.span()), w)
        }
    }
}

fn line_of(span: minijinja::machinery::Span) -> usize {
    usize::from(span.start_line)
}

/// A load's targets, which must be string literals: the files are embedded
/// when the playbook compiles, so the name must be known then.
fn load(
    statement: &'static str,
    target: &Expr,
    ignore_missing: bool,
    list_allowed: bool,
    line: usize,
    w: &mut Walk,
) -> WalkResult {
    let mut targets = Vec::new();
    let list = matches!(target, Expr::List(_) | Expr::Tuple(_));
    if !literals(target, list_allowed, &mut targets) {
        let what = if list_allowed {
            "a string literal, a list of them, or `\"a\" if <test> else \"b\"`"
        } else {
            "a string literal, or `\"a\" if <test> else \"b\"`"
        };
        return Err((
            line,
            format!(
                "the target of `{{% {statement} %}}` must be {what}: `template!` embeds the \
                 files a template reaches when the playbook compiles, so it must know their \
                 names then"
            ),
        ));
    }
    let mut condition = HashSet::new();
    vars(target, &mut condition);
    w.vars.extend(condition.iter().cloned());
    w.target_vars.extend(condition);
    w.loads.push(Load {
        statement,
        targets,
        may_miss: ignore_missing,
        list,
        line,
    });
    Ok(())
}

/// Collect the string literals an expression can evaluate to: a literal,
/// a list of them (an include list), or both branches of an inline `if`,
/// which must have an `else`: without one a false test yields no name,
/// which fails the render even under `ignore missing`.
fn literals(expr: &Expr, list_allowed: bool, out: &mut Vec<String>) -> bool {
    match expr {
        Expr::Const(c) => match c.value.as_str() {
            Some(s) => {
                out.push(s.to_string());
                true
            }
            None => false,
        },
        Expr::List(l) if list_allowed => l.items.iter().all(|i| literals(i, false, out)),
        Expr::Tuple(t) if list_allowed => t.items.iter().all(|i| literals(i, false, out)),
        Expr::IfExpr(i) => match &i.false_expr {
            Some(otherwise) => {
                literals(&i.true_expr, list_allowed, out) && literals(otherwise, list_allowed, out)
            }
            None => false,
        },
        _ => false,
    }
}

/// The names an assignment target binds: `x`, `(a, b)`, `[a, b]`.
fn assign(target: &Expr, assigned: &mut HashSet<String>) {
    match target {
        Expr::Var(v) => {
            assigned.insert(v.id.to_string());
        }
        Expr::List(l) => l.items.iter().for_each(|i| assign(i, assigned)),
        Expr::Tuple(t) => t.items.iter().for_each(|i| assign(i, assigned)),
        _ => {}
    }
}

/// Every variable an expression reads. Exhaustive, like [`walk`].
fn vars(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Var(v) => {
            out.insert(v.id.to_string());
        }
        Expr::Const(_) => {}
        Expr::Slice(s) => {
            vars(&s.expr, out);
            for e in [&s.start, &s.stop, &s.step].into_iter().flatten() {
                vars(e, out);
            }
        }
        Expr::UnaryOp(u) => vars(&u.expr, out),
        Expr::BinOp(b) => {
            vars(&b.left, out);
            vars(&b.right, out);
        }
        Expr::Compare(c) => {
            vars(&c.expr, out);
            c.ops.iter().for_each(|op| vars(&op.expr, out));
        }
        Expr::IfExpr(i) => {
            vars(&i.test_expr, out);
            vars(&i.true_expr, out);
            if let Some(e) = &i.false_expr {
                vars(e, out);
            }
        }
        Expr::Filter(f) => {
            if let Some(e) = &f.expr {
                vars(e, out);
            }
            f.args.iter().for_each(|a| arg_vars(a, out));
        }
        Expr::Test(t) => {
            vars(&t.expr, out);
            t.args.iter().for_each(|a| arg_vars(a, out));
        }
        Expr::GetAttr(g) => vars(&g.expr, out),
        Expr::GetItem(g) => {
            vars(&g.expr, out);
            vars(&g.subscript_expr, out);
        }
        Expr::Call(c) => call_vars(c, out),
        Expr::List(l) => l.items.iter().for_each(|i| vars(i, out)),
        Expr::Tuple(t) => t.items.iter().for_each(|i| vars(i, out)),
        Expr::Map(m) => {
            m.keys.iter().for_each(|k| vars(k, out));
            m.values.iter().for_each(|v| vars(v, out));
        }
    }
}

fn call_vars(call: &minijinja::machinery::ast::Call, out: &mut HashSet<String>) {
    vars(&call.expr, out);
    call.args.iter().for_each(|a| arg_vars(a, out));
}

fn arg_vars(arg: &minijinja::machinery::ast::CallArg, out: &mut HashSet<String>) {
    use minijinja::machinery::ast::CallArg;
    match arg {
        CallArg::Pos(e) | CallArg::Kwarg(_, e) | CallArg::PosSplat(e) | CallArg::KwargSplat(e) => {
            vars(e, out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay `files` out in a temporary directory and resolve the tree of
    /// the one named `root`, as `template!(include_str!(root))` would.
    fn resolve(files: &[(&str, &str)], root: &str) -> (tempfile::TempDir, Result<Tree, String>) {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let base = dir.path().canonicalize().unwrap();
        let tree = resolve_in(&base, root);
        (dir, tree)
    }

    fn resolve_in(base: &Path, root: &str) -> Result<Tree, String> {
        let path = base.join(root).canonicalize().unwrap();
        Tree::resolve(
            Root {
                name: display(&path, Some(base)),
                source: std::fs::read_to_string(&path).unwrap(),
                dir: path.parent().map(Path::to_path_buf),
                path: Some(path),
            },
            Some(base),
        )
    }

    fn names(tree: &Tree) -> Vec<&str> {
        tree.files.iter().map(|f| f.name.as_str()).collect()
    }

    fn links(tree: &Tree) -> Vec<(&str, &str, &str)> {
        tree.links
            .iter()
            .map(|(from, written, to)| {
                (
                    tree.files[*from].name.as_str(),
                    written.as_str(),
                    tree.files[*to].name.as_str(),
                )
            })
            .collect()
    }

    fn check(tree: &Tree, spread: bool, passed: &[&str]) -> Result<(), String> {
        let idents: Vec<Ident> = passed.iter().map(|n| syn::parse_str(n).unwrap()).collect();
        let value: syn::Expr = syn::parse_str("1").unwrap();
        let named: Vec<(String, &Ident, &syn::Expr)> =
            idents.iter().map(|i| (i.to_string(), i, &value)).collect();
        tree.check_names(spread, &named, &value)
            .map_err(|e| e.to_string())
    }

    // ---- resolution ----

    #[test]
    fn targets_resolve_from_the_including_file_at_every_depth() {
        let (_d, tree) = resolve(
            &[
                ("t/one.j2", "{% include \"two/two.j2\" %}"),
                ("t/two/two.j2", "{% include \"three/three.j2\" %}"),
                (
                    "t/two/three/three.j2",
                    "{% include \"../../../shared/leaf.j2\" %}",
                ),
                ("shared/leaf.j2", "leaf"),
            ],
            "t/one.j2",
        );
        let tree = tree.unwrap();
        assert_eq!(
            names(&tree),
            [
                "t/one.j2",
                "t/two/two.j2",
                "t/two/three/three.j2",
                "shared/leaf.j2"
            ]
        );
        assert_eq!(
            links(&tree),
            [
                ("t/one.j2", "two/two.j2", "t/two/two.j2"),
                ("t/two/two.j2", "three/three.j2", "t/two/three/three.j2"),
                (
                    "t/two/three/three.j2",
                    "../../../shared/leaf.j2",
                    "shared/leaf.j2"
                ),
            ]
        );
    }

    #[test]
    fn a_cycle_and_a_self_include_are_embedded_once() {
        let (_d, tree) = resolve(
            &[
                ("a.j2", "{% include \"b.j2\" %}{% include \"a.j2\" %}"),
                ("b.j2", "{% include \"a.j2\" %}{% include \"./a.j2\" %}"),
            ],
            "a.j2",
        );
        let tree = tree.unwrap();
        assert_eq!(names(&tree), ["a.j2", "b.j2"]);
        assert_eq!(
            links(&tree),
            [
                ("a.j2", "b.j2", "b.j2"),
                ("a.j2", "a.j2", "a.j2"),
                ("b.j2", "a.j2", "a.j2"),
                ("b.j2", "./a.j2", "a.j2"),
            ]
        );
    }

    #[test]
    fn an_include_list_embeds_every_entry_that_exists() {
        let (_d, tree) = resolve(
            &[
                ("top.j2", "{% include [\"x.j2\", \"gone.j2\", \"y.j2\"] %}"),
                ("x.j2", "x"),
                ("y.j2", "y"),
            ],
            "top.j2",
        );
        assert_eq!(names(&tree.unwrap()), ["top.j2", "x.j2", "y.j2"]);

        // Only all of them missing fails, and not under `ignore missing`.
        let (_d, tree) = resolve(
            &[("top.j2", "{% include [\"a.j2\", \"b.j2\"] %}")],
            "top.j2",
        );
        let err = tree.err().unwrap();
        assert!(err.contains("`{% include %}`: no such file"), "{err}");
        assert!(err.contains("a.j2`, `") && err.contains("b.j2`"), "{err}");
        let (_d, tree) = resolve(
            &[(
                "top.j2",
                "{% include [\"a.j2\", \"b.j2\"] ignore missing %}",
            )],
            "top.j2",
        );
        assert_eq!(names(&tree.unwrap()), ["top.j2"]);
    }

    #[test]
    fn every_target_of_a_conditional_is_embedded() {
        let (_d, tree) = resolve(
            &[
                (
                    "child.j2",
                    "{% if alt %}{% extends \"a.j2\" %}{% else %}{% extends \"b.j2\" %}{% endif %}\
                     {% include \"c.j2\" if x else \"d.j2\" %}\
                     {% if false %}{% macro m() %}{% import \"e.j2\" as e %}{% endmacro %}{% endif %}",
                ),
                ("a.j2", "a"),
                ("b.j2", "b"),
                ("c.j2", "c"),
                ("d.j2", "d"),
                ("e.j2", "e"),
            ],
            "child.j2",
        );
        assert_eq!(
            names(&tree.unwrap()),
            ["child.j2", "a.j2", "b.j2", "c.j2", "d.j2", "e.j2"]
        );
    }

    #[test]
    fn a_symlinked_template_resolves_its_targets_from_where_the_link_points() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("shared")).unwrap();
        std::fs::create_dir_all(base.join("pb")).unwrap();
        std::fs::write(base.join("shared/t.j2"), "{% include \"inc.j2\" %}").unwrap();
        std::fs::write(base.join("shared/inc.j2"), "shared inc").unwrap();
        std::fs::write(base.join("pb/inc.j2"), "wrong inc").unwrap();
        std::os::unix::fs::symlink(base.join("shared/t.j2"), base.join("pb/link.j2")).unwrap();
        // The link to the include, too: one file, embedded once.
        std::os::unix::fs::symlink(base.join("shared/inc.j2"), base.join("pb/inc-link.j2"))
            .unwrap();
        std::fs::write(
            base.join("pb/top.j2"),
            "{% include \"link.j2\" %}{% include \"inc-link.j2\" %}",
        )
        .unwrap();
        let tree = resolve_in(&base, "pb/top.j2").unwrap();
        assert_eq!(names(&tree), ["pb/top.j2", "shared/t.j2", "shared/inc.j2"]);
    }

    #[test]
    fn a_missing_target_is_an_error_naming_the_file_and_how_it_was_reached() {
        let (_d, tree) = resolve(
            &[
                ("one.j2", "{% include \"two.j2\" %}"),
                ("two.j2", "\n{% include \"sub/three.j2\" %}"),
                ("sub/three.j2", "a\nb\n{% extends \"nope.j2\" %}"),
            ],
            "one.j2",
        );
        let err = tree.err().unwrap();
        assert!(
            err.starts_with("template `sub/three.j2` line 3: `{% extends %}`: no such file: `"),
            "{err}"
        );
        assert!(err.contains("sub/nope.j2`"), "{err}");
        assert!(
            err.ends_with("(reached from template `two.j2` line 2, from template `one.j2` line 1)"),
            "{err}"
        );
    }

    #[test]
    fn ignore_missing_tolerates_a_missing_include() {
        let (_d, tree) = resolve(
            &[("t.j2", "{% include \"gone.j2\" ignore missing %}")],
            "t.j2",
        );
        let tree = tree.unwrap();
        assert_eq!(names(&tree), ["t.j2"]);
        assert!(tree.links.is_empty());
    }

    #[test]
    fn a_target_that_is_not_a_literal_is_an_error() {
        for (text, statement) in [
            ("{% include name %}", "include"),
            ("{% include \"a\" ~ name %}", "include"),
            ("{% include [\"a.j2\", name] %}", "include"),
            ("{% extends name %}", "extends"),
            ("{% extends [\"a.j2\"] %}", "extends"),
            ("{% import name as m %}", "import"),
            ("{% from name import m %}", "from"),
        ] {
            let (_d, tree) = resolve(&[("t.j2", text), ("a.j2", "")], "t.j2");
            let err = tree.err().unwrap();
            assert!(
                err.starts_with(&format!(
                    "template `t.j2` line 1: the target of `{{% {statement} %}}` must be"
                )),
                "{text}: {err}"
            );
        }
    }

    #[test]
    fn a_syntax_error_names_its_file_line_and_how_it_was_reached() {
        let (_d, tree) = resolve(
            &[
                ("one.j2", "{% include \"a/two.j2\" %}"),
                ("a/two.j2", "{% include \"b/three.j2\" %}"),
                ("a/b/three.j2", "{% include \"four.j2\" %}"),
                ("a/b/four.j2", "fine\n{% if %}"),
            ],
            "one.j2",
        );
        assert_eq!(
            tree.err().unwrap(),
            "template `a/b/four.j2` line 2: syntax error: unexpected end of block (reached from \
             template `a/b/three.j2` line 1, from template `a/two.j2` line 1, from template \
             `one.j2` line 1)"
        );
        let (_d, tree) = resolve(&[("t.j2", "{{ x }")], "t.j2");
        assert!(
            tree.err()
                .unwrap()
                .starts_with("template `t.j2` line 1: syntax error: ")
        );
    }

    #[test]
    fn a_literal_with_no_file_cannot_include() {
        let tree = Tree::resolve(
            Root {
                name: "inline template".into(),
                source: "{% include \"x.j2\" %}".into(),
                dir: None,
                path: None,
            },
            None,
        );
        let err = tree.err().unwrap();
        assert!(
            err.starts_with("inline template, line 1: `{% include %}` needs to know"),
            "{err}"
        );
    }

    // ---- names ----

    #[test]
    fn every_name_the_top_level_uses_must_be_passed_unless_there_is_a_spread() {
        let (_d, tree) = resolve(
            &[(
                "t.j2",
                "{{ a }}{{ b.c }}{% for x in xs %}{{ x }}{{ loop.index }}{% endfor %}\
                 {% set s = 1 %}{{ s }}{{ range(2) }}{{ namespace() }}",
            )],
            "t.j2",
        );
        let tree = tree.unwrap();
        assert_eq!(check(&tree, false, &["a", "b", "xs"]), Ok(()));
        assert_eq!(
            check(&tree, false, &["a"]).unwrap_err(),
            "template `t.j2` uses `b`, `xs`, not passed to `template!`: add `b = <value>`, or a \
             `..` spread that holds them"
        );
        assert_eq!(check(&tree, true, &["a"]), Ok(()));
    }

    #[test]
    fn a_name_passed_must_be_used_somewhere_in_the_tree_spread_or_not() {
        let (_d, tree) = resolve(
            &[
                ("t.j2", "{{ a }}{% include \"inc.j2\" %}"),
                ("inc.j2", "{{ deep }}"),
            ],
            "t.j2",
        );
        let tree = tree.unwrap();
        // `deep` is used only by the included file, which is enough.
        assert_eq!(check(&tree, false, &["a", "deep"]), Ok(()));
        for spread in [false, true] {
            assert_eq!(
                check(&tree, spread, &["a", "deep", "extra"]).unwrap_err(),
                "`extra` is passed to `template!` but not used by template `t.j2` or by any \
                 template it includes, extends or imports"
            );
        }
    }

    #[test]
    fn names_in_included_files_are_left_to_the_render() {
        let (_d, tree) = resolve(
            &[
                ("t.j2", "{% include \"inc.j2\" %}"),
                ("inc.j2", "{{ v.host }}{{ anything }}"),
            ],
            "t.j2",
        );
        assert_eq!(check(&tree.unwrap(), false, &[]), Ok(()));
    }

    #[test]
    fn a_name_the_template_assigns_itself_is_never_required_and_never_unused() {
        for (text, passed) in [
            // The default idiom: reads the name it assigns.
            (
                "{% set port = port | default(80) %}{{ port }}",
                &["port"][..],
            ),
            ("{% set port = port | default(80) %}{{ port }}", &[][..]),
            // A macro's default and a `with` read the outer name.
            (
                "{% macro m(x=x) %}{{ x }}{% endmacro %}{{ m() }}",
                &["x"][..],
            ),
            ("{% with x = x + 1 %}{{ x }}{% endwith %}", &["x"][..]),
            // Assigned in both branches, read after.
            (
                "{% if c %}{% set y = 1 %}{% else %}{% set y = 2 %}{% endif %}{{ y }}",
                &["c"][..],
            ),
        ] {
            let (_d, tree) = resolve(&[("t.j2", text)], "t.j2");
            assert_eq!(
                check(&tree.unwrap(), false, passed),
                Ok(()),
                "{text} with {passed:?}"
            );
        }
    }

    #[test]
    fn a_load_target_s_condition_is_a_name_the_template_uses() {
        let files = [
            ("t.j2", "{% include \"a.j2\" if flag else \"b.j2\" %}"),
            ("a.j2", "a"),
            ("b.j2", "b"),
        ];
        let (_d, tree) = resolve(&files, "t.j2");
        let tree = tree.unwrap();
        assert_eq!(names(&tree), ["t.j2", "a.j2", "b.j2"]);
        assert_eq!(check(&tree, false, &["flag"]), Ok(()));
        assert!(
            check(&tree, false, &[])
                .unwrap_err()
                .contains("uses `flag`")
        );

        // Without an `else`, a false test names no template at all.
        let (_d, tree) = resolve(
            &[("t.j2", "{% include \"a.j2\" if flag %}"), ("a.j2", "")],
            "t.j2",
        );
        let err = tree.err().unwrap();
        assert!(
            err.contains(
                "must be a string literal, a list of them, or `\"a\" if <test> else \"b\"`"
            ),
            "{err}"
        );
    }

    #[test]
    fn an_include_tuple_is_a_list() {
        let (_d, tree) = resolve(
            &[
                ("t.j2", "{% include (\"gone.j2\", \"x.j2\") %}"),
                ("x.j2", "x"),
            ],
            "t.j2",
        );
        assert_eq!(names(&tree.unwrap()), ["t.j2", "x.j2"]);
    }

    #[test]
    fn a_parent_reached_by_include_first_still_lends_its_names() {
        // `gp.j2` is included before `p.j2` extends it, and its loop
        // variable still reaches the top-level template's block.
        let (_d, tree) = resolve(
            &[
                (
                    "t.j2",
                    "{% include \"gp.j2\" %}{% extends \"p.j2\" %}{% block b %}{{ item }}{% endblock %}",
                ),
                ("p.j2", "{% extends \"gp.j2\" %}"),
                (
                    "gp.j2",
                    "{% for item in items %}{% block b %}{% endblock %}{% endfor %}",
                ),
            ],
            "t.j2",
        );
        assert_eq!(check(&tree.unwrap(), false, &["items"]), Ok(()));
    }

    #[test]
    fn a_name_a_parent_assigns_is_not_required_of_the_child() {
        // minijinja runs the child's block inside the parent's loop.
        let (_d, tree) = resolve(
            &[
                (
                    "child.j2",
                    "{% extends \"base.j2\" %}{% block row %}{{ item }}{{ title }}{% endblock %}",
                ),
                (
                    "base.j2",
                    "{% for item in items %}{% block row %}{% endblock %}{% endfor %}",
                ),
            ],
            "child.j2",
        );
        let tree = tree.unwrap();
        assert_eq!(check(&tree, false, &["items", "title"]), Ok(()));
        assert!(
            check(&tree, false, &["items"])
                .unwrap_err()
                .contains("uses `title`")
        );
    }
}
