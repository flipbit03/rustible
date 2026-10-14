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
use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};
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
    let call = shape(input.clone());
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
            Ok(unchecked(
                &name,
                quote! { ::core::convert::AsRef::<str>::as_ref(&(#expr)) },
                vars,
            ))
        }
        Source::Inline(lit) => {
            let name = match call_location(lit.span(), base.as_deref()) {
                Some(at) => format!("inline template at {at}"),
                None => "inline template".to_string(),
            };
            let from = |dir: Option<PathBuf>| -> syn::Result<TokenStream> {
                let root = Root {
                    name: name.clone(),
                    source: lit.value(),
                    dir,
                    path: None,
                };
                let tree = Tree::resolve(root, base.as_deref())
                    .map_err(|m| Error::new_spanned(&lit, m))?;
                if let Some(why) = tree.unresolved() {
                    return Err(Error::new_spanned(&lit, why));
                }
                tree.check_names(spread.is_some(), &named, &lit)?;
                let at = call_location(lit.span(), base.as_deref()).unwrap_or_default();
                Ok(tree.expand(quote! { #lit }, vars.clone(), &at))
            };
            let here = local_file(lit.span()).and_then(|f| f.parent().map(Path::to_path_buf));
            if here.is_some() || !names_no_files(lit.span()) {
                return from(here);
            }
            // Where no file is named, the template is checked as it is, and
            // only one that loads another needs the call's directory.
            let root = Root {
                name: name.clone(),
                source: lit.value(),
                dir: None,
                path: None,
            };
            let tree =
                Tree::resolve(root, base.as_deref()).map_err(|m| Error::new_spanned(&lit, m))?;
            if tree.unresolved.is_none() {
                return from(None);
            }
            let dirs = caller_dirs(&lit, &call, base.as_deref());
            match dirs.as_slice() {
                [dir] => from(Some(dir.clone())),
                several => {
                    match same_failure(several, base.as_deref(), |d| from(Some(d.to_path_buf()))) {
                        Some(e) => Err(e),
                        None => Ok(unchecked(&name, quote! { #lit }, vars)),
                    }
                }
            }
        }
        Source::File(path) => {
            let dirs = match local_file(path.span()) {
                Some(here) => vec![here.parent().map(Path::to_path_buf).unwrap_or_default()],
                None if names_no_files(path.span()) => caller_dirs(&path, &call, base.as_deref()),
                None => {
                    return Err(Error::new_spanned(
                        &path,
                        "`template!` cannot tell which file this call is in, so it cannot \
                         resolve the `include_str!` path relative to it",
                    ));
                }
            };
            let from = |dir: &Path| -> syn::Result<TokenStream> {
                let unreadable = |e: std::io::Error| {
                    Error::new_spanned(&path, unreadable(&path.value(), dir, &e, base.as_deref()))
                };
                let canon = dir.join(path.value()).canonicalize().map_err(unreadable)?;
                let text = std::fs::read_to_string(&canon).map_err(unreadable)?;
                let root = Root {
                    name: display(&canon, base.as_deref()),
                    source: text,
                    dir: canon.parent().map(Path::to_path_buf),
                    path: Some(canon.clone()),
                };
                let tree = Tree::resolve(root, base.as_deref())
                    .map_err(|m| Error::new_spanned(&path, m))?;
                tree.check_names(spread.is_some(), &named, &path)?;
                let abs = canon.to_string_lossy().into_owned();
                let at = call_location(path.span(), base.as_deref()).unwrap_or_default();
                Ok(tree.expand(quote! { ::core::include_str!(#abs) }, vars.clone(), &at))
            };
            match dirs.as_slice() {
                [dir] => from(dir),
                several => match same_failure(several, base.as_deref(), from) {
                    Some(e) => Err(e),
                    // The compiler resolves this `include_str!` itself, relative
                    // to the call, as it would one written outside `template!`.
                    None => Ok(unchecked(
                        &path.value(),
                        quote! { ::core::include_str!(#path) },
                        vars,
                    )),
                },
            }
        }
    }
}

/// For a call written the same way in several directories, which a host
/// that names no files cannot tell apart: the error `check` gives from every
/// one of them, alike but for the directory. Wherever the call is, that is
/// its error. `None` when one passes, when they differ, or for no directory.
fn same_failure(
    dirs: &[PathBuf],
    base: Option<&Path>,
    check: impl Fn(&Path) -> syn::Result<TokenStream>,
) -> Option<Error> {
    let shown: Vec<String> = dirs.iter().map(|d| display(d, base)).collect();
    let mut failures = Vec::new();
    for (dir, shown_dir) in dirs.iter().zip(&shown) {
        let error = check(dir).err()?;
        let alike: Vec<String> = error
            .clone()
            .into_iter()
            .map(|e| match shown_dir.is_empty() {
                true => e.to_string(),
                false => e.to_string().replace(shown_dir.as_str(), "\0"),
            })
            .collect();
        failures.push((error, alike));
    }
    let (first, alike) = failures.first()?;
    if failures.iter().any(|(_, other)| other != alike) {
        return None;
    }
    let note = format!(
        " [the same call is in `{}`, which the editor cannot tell apart; it fails in \
         each, and the paths shown are `{}`'s]",
        shown.join("`, `"),
        shown[0]
    );
    first
        .clone()
        .into_iter()
        .map(|e| Error::new(e.span(), format!("{e}{note}")))
        .reduce(|mut all, e| {
            all.combine(e);
            all
        })
}

/// The error for an `include_str!` path that cannot be read: the file it
/// names, crate-relative, where the path resolves from, and for a missing
/// file the missing directory or the likeliest file meant.
fn unreadable(written: &str, dir: &Path, e: &std::io::Error, base: Option<&Path>) -> String {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let wanted = dir.join(written);
    if e.kind() != std::io::ErrorKind::NotFound {
        return format!("`template!` cannot read `{}`: {e}", display(&wanted, base));
    }
    let shown_dir = match display(&dir, base) {
        d if d.is_empty() => ".".to_string(),
        d => d,
    };
    let mut why = format!(
        "template file not found: `{}` (an `include_str!` path is relative to the \
         directory of the file the call is in, `{shown_dir}`)",
        display(&wanted, base)
    );
    match wanted.parent() {
        Some(parent) if !parent.is_dir() => {
            why.push_str(&format!(
                "; there is no directory `{}`",
                display(parent, base)
            ));
        }
        _ => {
            if let Some(close) = closest(written, &wanted) {
                why.push_str(&format!("; did you mean `{close}`?"));
            }
        }
    }
    why
}

/// A file beside the missing `wanted` whose name is a few edits from its
/// name, written as `written` with that name in place of the last part.
fn closest(written: &str, wanted: &Path) -> Option<String> {
    let name = wanted.file_name()?.to_str()?;
    let (distance, best) = std::fs::read_dir(wanted.parent()?)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .map(|n| (edit_distance(name, &n), n))
        .min()?;
    (distance <= (name.chars().count() / 3).max(2)).then(|| match written.rfind('/') {
        Some(i) => format!("{}{best}", &written[..=i]),
        None => best,
    })
}

/// Levenshtein distance, by `char`.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let next = (diagonal + usize::from(ca != cb))
                .min(row[j] + 1)
                .min(row[j + 1] + 1);
            diagonal = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// A template rendered on the target without the compile-time checks:
/// [`Source::Dynamic`], and a compile-time template where the macro cannot
/// read what it reaches. `source` is an expression of type `&str`.
fn unchecked(name: &str, source: TokenStream, vars: TokenStream) -> TokenStream {
    quote! { ::rustible::sdk::template::__render_dynamic(#name, #source, #vars) }
}

/// Whether the macro is expanding in a host that names no source file at
/// all: `file()` of the span is empty, which rustc never answers, so a call
/// rustc does not name a local file for keeps the error. An IDE
/// expands macros only to analyse the code, never to build what runs, so
/// there a compile-time template is checked from the files [`caller_dirs`]
/// finds, or falls back to [`unchecked`], instead of failing:
/// rust-analyzer's proc-macro server names no files unless it is opted into
/// its bidirectional protocol, and every `template!` call would show an
/// error that `cargo build` never gives. The build still checks it.
fn names_no_files(span: Span) -> bool {
    span.unwrap().file().is_empty()
}

/// For a host that names no files: the directories the call may be in.
/// Every `.rs` file of the crate is searched, but for the build directory
/// (`target`) and hidden directories at the crate's root, without following
/// symlinks, as `rustible-build` finds playbooks. A file is the call's when
/// it holds a `template!(..)` with this call's tokens (`call`, as [`shape`]
/// writes them), and may be when the literal stands in it outside any
/// `template!(..)` (a call through an alias or a `macro_rules!`) or when it
/// does not lex. More than one directory is a call the editor cannot place
/// (the same call in two hosts' playbooks), left to [`same_failure`].
fn caller_dirs(lit: &LitStr, call: &str, base: Option<&Path>) -> Vec<PathBuf> {
    let Some(base) = base else {
        return Vec::new();
    };
    let written = lit.token().to_string();
    let mut found = BTreeSet::new();
    let mut pending = vec![base.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if meta.is_dir() {
                let skipped = dir == base && (name == "target" || name.starts_with('.'));
                if !skipped {
                    pending.push(path);
                }
            } else if meta.is_file()
                && name.ends_with(".rs")
                && scanned(&path, &meta).may_hold(call, &written)
            {
                found.insert(dir.clone());
            }
        }
    }
    found.into_iter().collect()
}

/// What [`caller_dirs`] needs of one `.rs` file.
#[derive(Default)]
struct Scan {
    /// The file's size and modification time when it was read.
    stamp: Option<(u64, std::time::SystemTime)>,
    /// The tokens of every `template!(..)` call in it, as [`shape`] writes them.
    calls: HashSet<String>,
    /// Every string literal, as written, outside those calls.
    loose: HashSet<String>,
    /// The text of a file that does not lex.
    unlexed: Option<String>,
}

impl Scan {
    fn may_hold(&self, call: &str, written: &str) -> bool {
        self.calls.contains(call)
            || self.loose.contains(written)
            || self.unlexed.as_ref().is_some_and(|t| t.contains(written))
    }
}

/// A file's [`Scan`], read again only when its size or modification time
/// changes: an editor expands every call on every change, and the macro's
/// process lives on between expansions.
fn scanned(path: &Path, meta: &std::fs::Metadata) -> std::sync::Arc<Scan> {
    use std::sync::{Arc, Mutex, OnceLock};
    static SCANS: OnceLock<Mutex<HashMap<PathBuf, Arc<Scan>>>> = OnceLock::new();
    let stamp = meta.modified().ok().map(|m| (meta.len(), m));
    let scans = SCANS.get_or_init(Default::default);
    if let Some(scan) = scans.lock().ok().and_then(|s| s.get(path).cloned())
        && stamp.is_some()
        && scan.stamp == stamp
    {
        return scan;
    }
    let mut scan = Scan {
        stamp,
        ..Scan::default()
    };
    if let Ok(text) = std::fs::read_to_string(path) {
        match text.parse::<TokenStream>() {
            Ok(tokens) => collect(tokens, &mut scan),
            Err(_) => scan.unlexed = Some(text),
        }
    }
    let scan = Arc::new(scan);
    if let Ok(mut scans) = scans.lock() {
        scans.insert(path.to_path_buf(), scan.clone());
    }
    scan
}

/// Every `template!(..)` call in `tokens`, at any depth, into `scan.calls`,
/// and every string literal outside them into `scan.loose`.
fn collect(tokens: TokenStream, scan: &mut Scan) {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    for (i, tree) in tokens.iter().enumerate() {
        match tree {
            TokenTree::Group(g) => {
                let invoked = i >= 2
                    && matches!(&tokens[i - 2], TokenTree::Ident(id) if id == "template")
                    && matches!(&tokens[i - 1], TokenTree::Punct(p) if p.as_char() == '!');
                if invoked {
                    scan.calls.insert(shape(g.stream()));
                } else {
                    collect(g.stream(), scan);
                }
            }
            TokenTree::Literal(l) => {
                scan.loose.insert(l.to_string());
            }
            _ => {}
        }
    }
}

/// Tokens as text, one space after each: the same for a call the compiler
/// hands the macro and for that call lexed from its file, whatever the
/// spacing, line breaks or comments in it.
fn shape(tokens: TokenStream) -> String {
    let mut out = String::new();
    for tree in tokens {
        match tree {
            TokenTree::Group(g) => {
                let (open, close) = match g.delimiter() {
                    Delimiter::Parenthesis => ("(", ")"),
                    Delimiter::Brace => ("{", "}"),
                    Delimiter::Bracket => ("[", "]"),
                    Delimiter::None => ("", ""),
                };
                out.push_str(open);
                out.push(' ');
                out.push_str(&shape(g.stream()));
                out.push_str(close);
            }
            other => out.push_str(&other.to_string()),
        }
        out.push(' ');
    }
    out
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
    /// `(from, to, line)` for every `extends` target, by index into
    /// `files`, with the line of the statement.
    extends: Vec<(usize, usize, usize)>,
    /// The first load that could not be followed because the compiler did
    /// not say which file its template is in: `(file, line, statement)`.
    /// The loads of such a template are left unresolved.
    unresolved: Option<(usize, usize, &'static str)>,
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
            unresolved: None,
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
                    tree.unresolved
                        .get_or_insert((index, load.line, load.statement));
                    continue;
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
                    if load.statement == "extends"
                        && !tree.extends.iter().any(|&(f, t, _)| (f, t) == (index, to))
                    {
                        tree.extends.push((index, to, load.line));
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
                        .map(|p| format!("`{}`", display(p, base)))
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
        tree.reject_extends_cycle()?;
        Ok(tree)
    }

    /// Refuse a template that is its own ancestor. minijinja catches an
    /// inheritance cycle by the names as written, which the target's
    /// path-join renames, so on the target it would render forever; and a
    /// chain of `extends` that comes back to where it started never has a
    /// last parent to render, whatever its conditions, so there is nothing
    /// to lose by refusing it here.
    fn reject_extends_cycle(&self) -> Result<(), String> {
        // Depth-first along `extends` only, from every template (one reached
        // by `include` can still start a cycle), each finished once.
        fn visit(
            tree: &Tree,
            at: usize,
            path: &mut Vec<usize>,
            done: &mut HashSet<usize>,
        ) -> Result<(), String> {
            if done.contains(&at) {
                return Ok(());
            }
            if let Some(start) = path.iter().position(|&p| p == at) {
                let cycle: Vec<&str> = path[start..]
                    .iter()
                    .chain(std::iter::once(&at))
                    .map(|&i| tree.files[i].name.as_str())
                    .collect();
                let from = path[path.len() - 1];
                let line = tree
                    .extends
                    .iter()
                    .find(|&&(f, t, _)| (f, t) == (from, at))
                    .map_or(0, |&(_, _, l)| l);
                return Err(tree.at(
                    from,
                    line,
                    &format!(
                        "`{{% extends %}}` makes a template its own ancestor: {}",
                        cycle.join(" -> ")
                    ),
                ));
            }
            path.push(at);
            for &(from, to, _) in &tree.extends {
                if from == at {
                    visit(tree, to, path, done)?;
                }
            }
            path.pop();
            done.insert(at);
            Ok(())
        }
        let mut done = HashSet::new();
        for start in 0..self.files.len() {
            visit(self, start, &mut Vec::new(), &mut done)?;
        }
        Ok(())
    }

    /// Why the tree is incomplete, if a load could not be followed.
    fn unresolved(&self) -> Option<String> {
        let (index, line, statement) = self.unresolved?;
        Some(self.at(
            index,
            line,
            &format!(
                "`{{% {statement} %}}` needs to know which file this template is in, to \
                 resolve its target relative to it, and the compiler does not say"
            ),
        ))
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

    /// ` (reached from `a` line 3, from `b` line 1)`, or nothing for the
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
        source: &LitStr,
    ) -> syn::Result<()> {
        let passed: HashSet<&str> = named.iter().map(|(n, _, _)| n.as_str()).collect();
        let globals: HashSet<String> = environment()
            .globals()
            .map(|(n, _)| n.to_string())
            .collect();

        // Names left to the render rather than required of the call:
        // - one the top-level template `set`s anywhere: minijinja's analysis
        //   takes a name set in both branches of an `if` for undeclared after
        //   it, and the `{% set port = port | default(80) %}` idiom reads a
        //   name it also sets. Names a loop, a `with` or a macro binds are
        //   scoped, and minijinja's analysis already tells their inside from
        //   their outside, so they are not exempt;
        // - one a parent assigns (any file reached through `extends`,
        //   transitively): minijinja runs a child's block in the scope it is
        //   called from, so the block can use the parent's loop variable.
        let root = &self.files[0];
        let mut parents = vec![0];
        let mut reached = HashSet::from([0]);
        while let Some(child) = parents.pop() {
            for &(from, to, _) in &self.extends {
                if from == child && reached.insert(to) {
                    parents.push(to);
                }
            }
        }
        let inherited: HashSet<&str> = reached
            .iter()
            .filter(|&&i| i != 0)
            .flat_map(|&i| self.files[i].walked.assigned.iter().map(String::as_str))
            .collect();

        if !has_spread {
            // minijinja's analysis, less what the template sets; plus what
            // that analysis does not see (a load target's condition, the
            // value a slice is taken of), less anything the template binds,
            // since only the walk saw it and the walk does not track scopes.
            let from_analysis = root
                .undeclared
                .iter()
                .filter(|n| !root.walked.set.contains(*n));
            let from_walk = root
                .walked
                .reads
                .unseen
                .iter()
                .filter(|n| !root.walked.assigned.contains(*n));
            let missing: BTreeSet<&str> = from_analysis
                .chain(from_walk)
                .map(String::as_str)
                .filter(|n| !passed.contains(n) && !globals.contains(*n) && !inherited.contains(n))
                .collect();
            if !missing.is_empty() {
                let list: Vec<String> = missing.iter().map(|n| format!("`{n}`")).collect();
                let add: Vec<String> = missing.iter().map(|n| format!("{n} = <value>")).collect();
                return Err(Error::new_spanned(
                    source,
                    format!(
                        "{} uses {}, not passed to `template!`: add `{}`, or a `..` spread \
                         that holds {}",
                        shown(&root.name),
                        list.join(", "),
                        add.join(", "),
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
            .flat_map(|f| f.walked.reads.all.iter().map(String::as_str))
            .collect();
        let root = &self.files[0];
        let scope = if self.files.len() > 1 {
            " or by any template it includes, extends or imports"
        } else {
            ""
        };
        // A one-line literal with no Jinja in it at all may be a file's
        // path, written without `include_str!`.
        let hint = if root.path.is_none()
            && !root.source.contains('\n')
            && !["{{", "{%", "{#"].iter().any(|d| root.source.contains(d))
        {
            format!(
                " (the template is the text {:?}, which has no `{{{{ }}}}`, `{{% %}}` or \
                 `{{# #}}` in it; if you meant a file, write `template!(include_str!({:?}), ..)`)",
                root.source, root.source
            )
        } else {
            String::new()
        };
        let mut unused = named
            .iter()
            .filter(|(name, _, _)| !used.contains(name.as_str()))
            .map(|(name, ident, _)| {
                Error::new_spanned(
                    ident,
                    format!(
                        "`{name}` is passed to `template!` but not used by {}{scope}{hint}",
                        shown(&root.name)
                    ),
                )
            });
        match unused.next() {
            Some(mut first) => {
                unused.for_each(|e| first.combine(e));
                Err(first)
            }
            None => Ok(()),
        }
    }

    /// The expansion: every file embedded with `include_str!` (so cargo
    /// rebuilds when one changes), the links, and the render call.
    fn expand(&self, root_source: TokenStream, vars: TokenStream, at: &str) -> TokenStream {
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
                        at: #at,
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

/// The variables a template reads.
#[derive(Default)]
struct Reads {
    /// Every one, wherever it is read: a name in here is used.
    all: HashSet<String>,
    /// The ones minijinja's own name analysis does not see: a load target's
    /// condition (`{% include "a.j2" if flag else "b.j2" %}`), the value a
    /// slice is taken of (`{{ xs[1:] }}`), a filter block's or set block's
    /// filter, and an `{% autoescape %}` expression.
    unseen: HashSet<String>,
}

/// What a walk over one template collects.
#[derive(Default)]
struct Walk {
    loads: Vec<Load>,
    /// Names the template assigns: `set`, `for`, `with`, macro and import
    /// names.
    assigned: HashSet<String>,
    /// The names bound for the rest of the template wherever they are
    /// bound: a `set` (or a set block), a macro, an import. A subset of
    /// `assigned`.
    set: HashSet<String>,
    /// Every variable the template reads, anywhere.
    reads: Reads,
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
            vars(&e.expr, &mut w.reads);
            Ok(())
        }
        Stmt::EmitRaw(_) | Stmt::Continue(_) | Stmt::Break(_) => Ok(()),
        Stmt::Do(d) => {
            call_vars(&d.call, &mut w.reads);
            Ok(())
        }
        Stmt::ForLoop(f) => {
            assign(&f.target, &mut w.assigned);
            w.assigned.insert("loop".to_string());
            vars(&f.iter, &mut w.reads);
            if let Some(e) = &f.filter_expr {
                vars(e, &mut w.reads);
            }
            body(&f.body, w)?;
            body(&f.else_body, w)
        }
        Stmt::IfCond(i) => {
            vars(&i.expr, &mut w.reads);
            body(&i.true_body, w)?;
            body(&i.false_body, w)
        }
        Stmt::WithBlock(b) => {
            for (target, value) in &b.assignments {
                assign(target, &mut w.assigned);
                vars(value, &mut w.reads);
            }
            body(&b.body, w)
        }
        Stmt::Set(s) => {
            assign(&s.target, &mut w.assigned);
            assign(&s.target, &mut w.set);
            vars(&s.expr, &mut w.reads);
            Ok(())
        }
        Stmt::SetBlock(s) => {
            assign(&s.target, &mut w.assigned);
            assign(&s.target, &mut w.set);
            if let Some(f) = &s.filter {
                unseen(f, w);
            }
            body(&s.body, w)
        }
        Stmt::AutoEscape(a) => {
            unseen(&a.enabled, w);
            body(&a.body, w)
        }
        Stmt::FilterBlock(f) => {
            unseen(&f.filter, w);
            body(&f.body, w)
        }
        Stmt::Block(b) => {
            w.assigned.insert("super".to_string());
            body(&b.body, w)
        }
        Stmt::Macro(m) => {
            // A macro, like an import, is bound for the rest of the
            // template wherever it is defined (an `if` branch too), as a
            // `set` is.
            w.assigned.insert(m.name.to_string());
            w.set.insert(m.name.to_string());
            w.assigned.insert("caller".to_string());
            m.args.iter().for_each(|a| assign(a, &mut w.assigned));
            m.defaults.iter().for_each(|d| vars(d, &mut w.reads));
            body(&m.body, w)
        }
        Stmt::CallBlock(c) => {
            call_vars(&c.call, &mut w.reads);
            w.assigned.insert("caller".to_string());
            c.macro_decl
                .args
                .iter()
                .for_each(|a| assign(a, &mut w.assigned));
            c.macro_decl
                .defaults
                .iter()
                .for_each(|d| vars(d, &mut w.reads));
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
            assign(&i.name, &mut w.set);
            load("import", &i.expr, false, false, line_of(i.span()), w)
        }
        Stmt::FromImport(f) => {
            for (name, alias) in &f.names {
                assign(alias.as_ref().unwrap_or(name), &mut w.assigned);
                assign(alias.as_ref().unwrap_or(name), &mut w.set);
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
    unseen(target, w);
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

/// Collect an expression's reads as ones minijinja's name analysis does not
/// see: it never visits a load target, a `{% filter %}` block's or a set
/// block's filter, or an `{% autoescape %}` expression.
fn unseen(expr: &Expr, w: &mut Walk) {
    let mut reads = Reads::default();
    vars(expr, &mut reads);
    w.reads.all.extend(reads.all.iter().cloned());
    w.reads.unseen.extend(reads.all);
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
fn vars(expr: &Expr, out: &mut Reads) {
    match expr {
        Expr::Var(v) => {
            out.all.insert(v.id.to_string());
        }
        Expr::Const(_) => {}
        Expr::Slice(s) => {
            let mut sliced = Reads::default();
            vars(&s.expr, &mut sliced);
            out.all.extend(sliced.all.iter().cloned());
            out.unseen.extend(sliced.all);
            out.unseen.extend(sliced.unseen);
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

fn call_vars(call: &minijinja::machinery::ast::Call, out: &mut Reads) {
    vars(&call.expr, out);
    call.args.iter().for_each(|a| arg_vars(a, out));
}

fn arg_vars(arg: &minijinja::machinery::ast::CallArg, out: &mut Reads) {
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
        let source: LitStr = syn::parse_str("\"t.j2\"").unwrap();
        tree.check_names(spread, &named, &source)
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
        let err = tree.unwrap().unresolved().unwrap();
        assert!(
            err.starts_with("inline template, line 1: `{% include %}` needs to know"),
            "{err}"
        );
    }

    #[test]
    fn a_literal_with_no_file_still_has_its_syntax_checked() {
        let tree = Tree::resolve(
            Root {
                name: "inline template".into(),
                source: "{% include \"x.j2\" %}{{ a ".into(),
                dir: None,
                path: None,
            },
            None,
        );
        let err = tree.err().unwrap();
        assert!(
            err.starts_with("inline template, line 1: syntax error: "),
            "{err}"
        );
    }

    #[test]
    fn a_call_is_found_in_its_file_token_for_token() {
        let call = shape(r#"include_str!("files/a.j2"), port = 80, ..&m"#.parse().unwrap());
        let file = r#"
            fn main() {
                let x = vec![rustible::template!(
                    include_str!( "files/a.j2" ), // the unit
                    port = 80,
                    ..&m
                )];
            }
        "#;
        let written = r#""files/a.j2""#;
        let holds = |text: &str| {
            let mut scan = Scan::default();
            collect(text.parse().unwrap(), &mut scan);
            scan.may_hold(&call, written)
        };
        assert!(holds(file));
        // The same literal in another call is that call's, not this one's.
        assert!(!holds(
            r#"fn main() { template!(include_str!("files/a.j2"), port = 81, ..&m); }"#
        ));
        // Outside any `template!` (an alias, a `macro_rules!`), it may be.
        assert!(holds(
            r#"fn main() { tpl!(include_str!("files/a.j2"), port = 80, ..&m); }"#
        ));
        assert!(!holds(r#"fn main() { tpl!(include_str!("files/b.j2")); }"#));
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
            "template `t.j2` uses `b`, `xs`, not passed to `template!`: add `b = <value>, xs = \
             <value>`, or a `..` spread that holds them"
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
    fn a_name_bound_in_a_scope_does_not_hide_the_same_name_outside_it() {
        for (text, name) in [
            (
                "{% macro row(user) %}<{{ user }}>{% endmacro %}{{ row(user) }}",
                "user",
            ),
            (
                "{% for host in hosts %}.{% endfor %} primary={{ host }}",
                "host",
            ),
            (
                "{% macro m() %}{{ caller(1) }}{% endmacro %}{% call(item) m() %}{{ item }}{% endcall %}{{ item }}",
                "item",
            ),
            ("{% with x = 1 %}{{ x }}{% endwith %}{{ x }}", "x"),
        ] {
            let (_d, tree) = resolve(&[("t.j2", text)], "t.j2");
            let tree = tree.unwrap();
            let passed: Vec<&str> = if name == "host" {
                vec!["hosts"]
            } else {
                vec![]
            };
            let err = check(&tree, false, &passed).unwrap_err();
            assert!(err.contains(&format!("uses `{name}`")), "{text}: {err}");
        }
    }

    #[test]
    fn a_name_read_in_any_position_is_used() {
        // `x` is passed and read only in the one position each line shows,
        // so a walk that missed the position would call it unused.
        for text in [
            "{{ x }}",
            "{{ 1 if x is defined }}",
            "{{ 'a' | default(x) }}",
            "{{ 'a' | replace(old=x, new='b') }}",
            "{{ {'k': x} }}",
            "{{ {x: 1} }}",
            "{{ [1][x] }}",
            "{{ [1][x:] }}",
            "{{ 1 < x }}",
            "{{ -x }}",
            "{{ x ~ 'a' }}",
            "{{ x.attr }}",
            "{{ range(x) }}",
            "{{ dict(k=x) }}",
            "{{ range(*x) }}",
            "{{ dict(**x) }}",
            "{{ (x, 1) }}",
            "{% do range(x) %}",
            "{% for i in [1] if x %}{% endfor %}",
            "{% for i in x %}{% endfor %}",
            "{% if x %}{% endif %}",
            "{% with y = x %}{% endwith %}",
            "{% set y = x %}",
            "{% set y | replace('a', x) %}a{% endset %}",
            "{% filter replace('a', x) %}a{% endfilter %}",
            "{% autoescape x %}{% endautoescape %}",
            "{% macro m(y=x) %}{% endmacro %}",
            "{% macro m() %}{{ caller() }}{% endmacro %}{% call m(x) %}{% endcall %}",
            "{% macro m() %}{% endmacro %}{% call(y=x) m() %}{% endcall %}",
            "{% include 'a.j2' if x else 'a.j2' %}",
        ] {
            let (_d, tree) = resolve(&[("t.j2", text), ("a.j2", "")], "t.j2");
            let tree = tree.unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(check(&tree, true, &["x"]), Ok(()), "{text}");
        }
    }

    #[test]
    fn names_a_load_condition_binds_in_scope_are_not_required() {
        // `loop` and a loop variable, read only by an include's condition.
        let (_d, tree) = resolve(
            &[
                (
                    "t.j2",
                    "{% for i in xs %}{% include 'a.j2' if loop.first and i else 'a.j2' %}{% endfor %}",
                ),
                ("a.j2", ""),
            ],
            "t.j2",
        );
        assert_eq!(check(&tree.unwrap(), false, &["xs"]), Ok(()));
    }

    #[test]
    fn a_name_bound_in_any_way_is_not_required_where_only_the_walk_sees_it() {
        // Each bound name is read only in a slice or a load condition, which
        // minijinja's analysis does not see, so only the walk's bindings
        // keep it from being required.
        for (text, passed) in [
            (
                "{% macro m(xs) %}{{ xs[1:] }}{% endmacro %}{{ m([1, 2]) }}",
                &[][..],
            ),
            (
                "{% macro m() %}{{ caller()[1:] }}{% endmacro %}{% call m() %}ab{% endcall %}",
                &[][..],
            ),
            (
                "{% macro m() %}{% endmacro %}{% call(c) m() %}{{ c[1:] }}{% endcall %}",
                &[][..],
            ),
            ("{% with b = a %}{{ b[1:] }}{% endwith %}", &["a"][..]),
            (
                "{% for i in xs %}{{ i[1:] }}{{ loop.cycle[1:] }}{% endfor %}",
                &["xs"][..],
            ),
            ("{% set s %}abc{% endset %}{{ s[1:] }}", &[][..]),
            ("{% import 'a.j2' as m %}{{ m[1:] }}", &[][..]),
            ("{% from 'a.j2' import f as g %}{{ g[1:] }}", &[][..]),
            (
                "{% from 'a.j2' import f %}{% include 'a.j2' if f else 'a.j2' %}",
                &[][..],
            ),
            // Bound in both branches of an `if`, used after it.
            (
                "{% if fancy %}{% macro row(x) %}<b>{{ x }}</b>{% endmacro %}{% else %}\
                 {% macro row(x) %}{{ x }}{% endmacro %}{% endif %}{{ row(1) }}",
                &["fancy"][..],
            ),
            (
                "{% if f %}{% import 'a.j2' as m %}{% else %}{% import 'a.j2' as m %}{% endif %}\
                 {{ m.x is defined }}",
                &["f"][..],
            ),
        ] {
            let (_d, tree) = resolve(
                &[("t.j2", text), ("a.j2", "{% macro f() %}{% endmacro %}")],
                "t.j2",
            );
            let tree = tree.unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(check(&tree, false, passed), Ok(()), "{text}");
        }
        // `super` inside a block, read in a slice.
        let (_d, tree) = resolve(
            &[
                (
                    "child.j2",
                    "{% extends 'base.j2' %}{% block b %}{{ super()[1:] }}{% endblock %}",
                ),
                ("base.j2", "{% block b %}[base]{% endblock %}"),
            ],
            "child.j2",
        );
        assert_eq!(check(&tree.unwrap(), false, &[]), Ok(()));
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
    fn a_grandparent_s_names_reach_the_top_level_block_too() {
        let (_d, tree) = resolve(
            &[
                (
                    "t.j2",
                    "{% extends 'p.j2' %}{% block b %}{{ item }}{% endblock %}",
                ),
                ("p.j2", "{% extends 'gp.j2' %}"),
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
    fn a_set_block_in_both_branches_binds_the_name_after_them() {
        let (_d, tree) = resolve(
            &[(
                "t.j2",
                "{% if c %}{% set x %}a{% endset %}{% else %}{% set x %}b{% endset %}{% endif %}{{ x }}",
            )],
            "t.j2",
        );
        assert_eq!(check(&tree.unwrap(), false, &["c"]), Ok(()));
    }

    #[test]
    fn a_name_read_by_a_filter_block_or_autoescape_must_be_passed() {
        for (text, name) in [
            ("{% filter indent(width) %}a\nb{% endfilter %}", "width"),
            (
                "{% set x | indent(width) %}a\nb{% endset %}{{ x }}",
                "width",
            ),
            ("{% autoescape flag %}a{% endautoescape %}", "flag"),
        ] {
            let (_d, tree) = resolve(&[("t.j2", text)], "t.j2");
            let tree = tree.unwrap();
            let err = check(&tree, false, &[]).unwrap_err();
            assert!(err.contains(&format!("uses `{name}`")), "{text}: {err}");
            assert_eq!(check(&tree, false, &[name]), Ok(()), "{text}");
        }
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
    fn a_template_that_is_its_own_ancestor_is_refused() {
        let (_d, tree) = resolve(&[("self.j2", "\n{% extends \"self.j2\" %}")], "self.j2");
        assert_eq!(
            tree.err().unwrap(),
            "template `self.j2` line 2: `{% extends %}` makes a template its own ancestor: \
             self.j2 -> self.j2"
        );
        let (_d, tree) = resolve(
            &[
                ("top.j2", "{% extends \"a.j2\" %}"),
                ("a.j2", "{% if x %}{% extends \"b.j2\" %}{% endif %}"),
                ("b.j2", "{% extends \"a.j2\" %}"),
            ],
            "top.j2",
        );
        let err = tree.err().unwrap();
        assert!(
            err.contains("its own ancestor: a.j2 -> b.j2 -> a.j2"),
            "{err}"
        );
        // A cycle among templates the root only includes is refused too: it
        // would hang the render just the same.
        let (_d, tree) = resolve(
            &[
                ("root.j2", "[{% include 'ca.j2' %}]"),
                ("ca.j2", "{% extends 'cb.j2' %}"),
                ("cb.j2", "{% extends 'ca.j2' %}"),
            ],
            "root.j2",
        );
        let err = tree.err().unwrap();
        assert!(
            err.contains("its own ancestor: ca.j2 -> cb.j2 -> ca.j2"),
            "{err}"
        );
        // A long chain of conditional parents is checked in linear time.
        let mut chain: Vec<(String, String)> = (0..40)
            .map(|n| {
                (
                    format!("a{n}.j2"),
                    format!("{{% extends 'a{}.j2' if x else 'b{}.j2' %}}", n + 1, n + 1),
                )
            })
            .collect();
        chain.extend((0..40).map(|n| {
            (
                format!("b{n}.j2"),
                format!("{{% extends 'a{}.j2' if x else 'b{}.j2' %}}", n + 1, n + 1),
            )
        }));
        chain.push(("a40.j2".into(), String::new()));
        chain.push(("b40.j2".into(), String::new()));
        let files: Vec<(&str, &str)> = chain
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let started = std::time::Instant::now();
        let (_d, tree) = resolve(&files, "a0.j2");
        assert!(tree.is_ok());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        // An include cycle is a recursion, which the render bounds: allowed.
        let (_d, tree) = resolve(
            &[
                ("a.j2", "{% include \"b.j2\" %}"),
                (
                    "b.j2",
                    "{% extends \"c.j2\" %}{% block x %}{% include \"a.j2\" %}{% endblock %}",
                ),
                ("c.j2", ""),
            ],
            "a.j2",
        );
        assert!(tree.is_ok());
    }

    #[test]
    fn a_slice_s_value_is_a_name_the_template_uses() {
        let (_d, tree) = resolve(&[("t.j2", "{{ xs[1:] }}{{ ys[n:] }}")], "t.j2");
        let tree = tree.unwrap();
        assert_eq!(check(&tree, false, &["xs", "ys", "n"]), Ok(()));
        let err = check(&tree, false, &[]).unwrap_err();
        assert!(err.contains("uses `n`, `xs`, `ys`"), "{err}");
    }

    #[test]
    fn a_load_inside_every_kind_of_body_is_embedded() {
        let bodies = [
            "{% for x in [1] %}{% include \"for.j2\" %}{% else %}{% include \"else.j2\" %}{% endfor %}",
            "{% if x %}{% include \"if.j2\" %}{% else %}{% include \"ifelse.j2\" %}{% endif %}",
            "{% with y = 1 %}{% include \"with.j2\" %}{% endwith %}",
            "{% set s %}{% include \"setblock.j2\" %}{% endset %}",
            "{% autoescape false %}{% include \"autoescape.j2\" %}{% endautoescape %}",
            "{% filter upper %}{% include \"filter.j2\" %}{% endfilter %}",
            "{% block b %}{% include \"block.j2\" %}{% endblock %}",
            "{% macro m() %}{% include \"macro.j2\" %}{% endmacro %}",
            "{% call m() %}{% include \"call.j2\" %}{% endcall %}",
        ];
        let targets = [
            "for",
            "else",
            "if",
            "ifelse",
            "with",
            "setblock",
            "autoescape",
            "filter",
            "block",
            "macro",
            "call",
        ];
        let mut files: Vec<(String, String)> = vec![("t.j2".into(), bodies.concat())];
        files.extend(targets.iter().map(|t| (format!("{t}.j2"), String::new())));
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, tree) = resolve(&files, "t.j2");
        let tree = tree.unwrap();
        let mut embedded = names(&tree);
        embedded.sort_unstable();
        let mut want: Vec<String> = targets.iter().map(|t| format!("{t}.j2")).collect();
        want.push("t.j2".into());
        want.sort_unstable();
        assert_eq!(embedded, want);
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
