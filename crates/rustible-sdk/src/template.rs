//! Rendering for `rustible::template!`: the one minijinja environment every
//! template renders in, the loader that serves a compile-time template's
//! embedded files, and the errors a render fails with.
//!
//! A playbook never calls anything here by name. It writes
//! `rustible::template!(..)`, whose rustdoc is the reference for the
//! language, the checks and the edge cases; the macro expands to calls into
//! this module, which is why its items are hidden.
//!
//! The environment, the same for every template:
//!
//! - **Strict undefined.** A missing name or attribute fails the render; it
//!   never renders as empty text.
//! - **`trim_blocks`**, as Ansible's `template` module renders: the newline
//!   after a `{% … %}` tag is removed. **`keep_trailing_newline`**: a
//!   template, and an included one, keeps its final newline, so an include
//!   on a line of its own renders exactly the file's lines.
//! - **No autoescape**, whatever the file is called: these are config files,
//!   not HTML.
//! - Every built-in filter, test and global function minijinja ships, and
//!   nothing else.
//! - minijinja's recursion limit, so a template that includes itself forever
//!   fails with an error rather than running out of stack.
//!
//! A compile-time template (a literal, or `include_str!` of a file) renders
//! with a loader that serves only the files the macro embedded, looked up by
//! where each `include`, `extends` or `import` was written, so it never
//! touches the target's filesystem. A dynamic template (text known only at
//! run time) renders with no loader at all, and any of those statements in it
//! fails the render before anything is read.

use std::borrow::Cow;

use minijinja::machinery::{Token, tokenize};
use minijinja::syntax::SyntaxConfig;
use minijinja::value::{Serde, ValueKind};
use minijinja::{AutoEscape, Environment, ErrorKind, UndefinedBehavior, Value};
use serde::Serialize;

use crate::error::{Error, Result};

/// The refusal a dynamic template's `include`, `extends`, `import` or
/// `from … import` fails with.
const NO_LOADER: &str = "includes are only available for templates known at compile time";

/// How many include sites a render error lists before it counts the rest. A
/// template that includes itself until the recursion limit stops it would
/// otherwise name the same line hundreds of times.
const SHOWN_INCLUDE_SITES: usize = 4;

/// A compile-time template as the macro embeds it. Built only by
/// `rustible::template!`'s expansion.
#[doc(hidden)]
pub struct __Embedded {
    /// The top-level template's name: its file, relative to the playbook
    /// crate, or `inline template at <file>:<line>` for a literal.
    pub name: &'static str,
    /// Every template the top-level one can reach, itself included, as
    /// `(name, source)`. A file reached twice is here once.
    pub files: &'static [(&'static str, &'static str)],
    /// `(including template, target as written, target's name)` for every
    /// `include`, `extends` and `import` target the macro resolved.
    pub links: &'static [(&'static str, &'static str, &'static str)],
}

/// The variables a template renders with, as the macro collects them: the
/// `..spread`, if any, then the named arguments, which win.
#[doc(hidden)]
pub struct __Vars {
    spread: Option<Value>,
    named: Vec<(&'static str, Value)>,
}

impl __Vars {
    #[doc(hidden)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        __Vars {
            spread: None,
            named: Vec::new(),
        }
    }

    #[doc(hidden)]
    pub fn spread<T: Serialize + ?Sized>(mut self, value: &T) -> Self {
        self.spread = Some(Value::from(Serde(value)));
        self
    }

    #[doc(hidden)]
    pub fn named<T: Serialize + ?Sized>(mut self, name: &'static str, value: &T) -> Self {
        self.named.push((name, Value::from(Serde(value))));
        self
    }

    /// The context the template sees. Fails when a value did not serialize,
    /// or when the spread is not a map.
    fn into_context(self, template: &str) -> Result<Value> {
        let template = &shown(template);
        let mut entries: Vec<(Value, Value)> = Vec::new();
        if let Some(spread) = self.spread {
            invalid(&spread, template, "the `..` spread")?;
            if spread.kind() != ValueKind::Map {
                return Err(Error::msg(format!(
                    "{template}: the `..` spread must serialize to a map (a struct, a `HashMap`, \
                     a `json!({{..}})` object), but it is {}",
                    describe(&spread)
                )));
            }
            for key in spread.try_iter().map_err(|e| render_error(&e))? {
                if key.as_str().is_none() {
                    return Err(Error::msg(format!(
                        "{template}: the `..` spread's keys are variable names, so they must be \
                         strings, but one is {}",
                        describe(&key)
                    )));
                }
                let value = spread.get_item(&key).map_err(|e| render_error(&e))?;
                entries.push((key, value));
            }
        }
        for (name, value) in self.named {
            invalid(&value, template, &format!("argument `{name}`"))?;
            let key = Value::from(name);
            match entries.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = value,
                None => entries.push((key, value)),
            }
        }
        Ok(Value::from_pairs(entries))
    }
}

/// Fail on a value that did not serialize: minijinja keeps the error inside
/// the value and would raise it only where the template touched it.
fn invalid(value: &Value, template: &str, what: &str) -> Result<()> {
    if value.kind() == ValueKind::Invalid {
        return Err(Error::msg(format!(
            "{template}: {what} cannot be given to a template: {value}"
        )));
    }
    Ok(())
}

/// A value's kind, and the value when it is short, for an error message.
fn describe(value: &Value) -> String {
    let shown = value.to_string();
    if shown.chars().count() <= 40 {
        format!("{} `{shown}`", value.kind())
    } else {
        format!("a {}", value.kind())
    }
}

/// The environment every template renders in; see the [module docs](self).
fn environment<'source>() -> Environment<'source> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_syntax(syntax());
    // Debug mode is what names the undefined expression in an error
    // (`` `v.upstrem` is undefined ``). It also copies the variables in
    // scope into the error's debug info, which `render_error` never shows.
    env.set_debug(true);
    env
}

/// `trim_blocks` and `keep_trailing_newline`, minijinja's delimiters.
fn syntax() -> SyntaxConfig {
    SyntaxConfig::builder()
        .trim_blocks(true)
        .keep_trailing_newline(true)
        .build()
        .expect("the default delimiters are valid")
}

/// Render a compile-time template: `template!` with a literal or an
/// `include_str!` source.
#[doc(hidden)]
pub fn __render_embedded(template: &'static __Embedded, vars: __Vars) -> Result<String> {
    let context = vars.into_context(template.name)?;
    let env = embedded_environment(template);
    env.get_template(template.name)
        .and_then(|t| t.render(context))
        .map_err(|e| render_error(&e))
}

/// The environment for a compile-time template: [`environment`], with the
/// embedded files as its only templates.
fn embedded_environment(template: &'static __Embedded) -> Environment<'static> {
    let mut env = environment();
    env.set_path_join_callback(move |name, parent| {
        match template
            .links
            .iter()
            .find(|(from, written, _)| *from == parent && *written == name)
        {
            Some((_, _, to)) => Cow::Borrowed(to),
            // Only a target the macro found missing under `ignore missing`
            // (or the missing entries of an include list) gets here, and the
            // loader serves nothing under this name, which those statements
            // treat as absent. Never the bare name: it could be another
            // embedded file's.
            None => Cow::Owned(format!(
                "{name} (from {parent}, not found when the playbook was compiled)"
            )),
        }
    });
    env.set_loader(move |name| {
        Ok(template
            .files
            .iter()
            .find(|(embedded, _)| *embedded == name)
            .map(|(_, source)| source.to_string()))
    });
    env
}

/// Render a dynamic template: `template!` with a `&str` known only at run
/// time. `name` says where the call is.
#[doc(hidden)]
pub fn __render_dynamic(name: &str, source: &str, vars: __Vars) -> Result<String> {
    let context = vars.into_context(name)?;
    let env = environment();
    let template = env
        .template_from_named_str(name, source)
        .map_err(|e| render_error(&e))?;
    if let Some((statement, line)) = first_load(source) {
        return Err(Error::msg(format!(
            "{name}, line {line}: `{{% {statement} %}}`: {NO_LOADER} (a string literal or \
             `include_str!` given to `template!`). A template whose text arrives at run time \
             reads no file: read what it needs in Rust, with `ctx.sys().read_to_string(..)` \
             for a file on the target, and pass it in as a variable"
        )));
    }
    template.render(context).map_err(|e| render_error(&e))
}

/// The first `include`, `extends`, `import` or `from` statement in a
/// template, and its line. A statement's keyword is the token right after
/// `{%`, so this finds every one, wherever it sits (an untaken branch, a
/// macro, a block) and whatever its target, and nothing in `{% raw %}` or
/// in a string. Only called on a template that already compiled.
fn first_load(source: &str) -> Option<(&'static str, usize)> {
    let mut after_block_start = false;
    for token in tokenize(source, false, syntax()) {
        let Ok((token, span)) = token else {
            return None;
        };
        if after_block_start && let Token::Ident(word) = token {
            for statement in ["include", "extends", "import", "from"] {
                if word == statement {
                    return Some((statement, usize::from(span.start_line)));
                }
            }
        }
        after_block_start = matches!(token, Token::BlockStart);
    }
    None
}

/// A render error as one line: what failed, in which template and line,
/// and the include sites it was reached through, innermost first.
fn render_error(err: &minijinja::Error) -> Error {
    // minijinja wraps an error raised inside an included template in one
    // `BadInclude` per include level; the innermost error is the cause.
    let mut sites = Vec::new();
    let mut cause = err;
    while cause.kind() == ErrorKind::BadInclude {
        let Some(inner) =
            std::error::Error::source(cause).and_then(|s| s.downcast_ref::<minijinja::Error>())
        else {
            break;
        };
        sites.push(location(cause));
        cause = inner;
    }
    let mut message = match cause.detail() {
        Some(detail) => format!("{}: {detail}", cause.kind()),
        None => cause.kind().to_string(),
    };
    if cause.kind() == ErrorKind::TemplateNotFound && cause.detail().is_none() {
        message.push_str(" (an `include`, `extends` or `import` target)");
    }
    let mut out = format!("{}: {message}", location(cause));
    if !sites.is_empty() {
        let shown: Vec<String> = sites
            .iter()
            .rev()
            .take(SHOWN_INCLUDE_SITES)
            .cloned()
            .collect();
        out.push_str(&format!(" (included from {}", shown.join(", from ")));
        if sites.len() > SHOWN_INCLUDE_SITES {
            out.push_str(&format!(", and {} more", sites.len() - SHOWN_INCLUDE_SITES));
        }
        out.push(')');
    }
    Error::msg(out)
}

/// Where an error is: ``template `files/x.j2` line 3``, or
/// `inline template at playbooks/site.rs:12, line 1` (and `dynamic …`) for
/// a template the macro named by its call.
fn location(err: &minijinja::Error) -> String {
    let Some(name) = err.name() else {
        return "a template".to_string();
    };
    match err.line() {
        Some(line) if is_call_site(name) => format!("{name}, line {line}"),
        Some(line) => format!("{} line {line}", shown(name)),
        None => shown(name),
    }
}

/// A template's name as a message shows it: ``template `files/x.j2` `` for
/// a file, the name itself for one the macro named by its call.
fn shown(name: &str) -> String {
    if is_call_site(name) {
        name.to_string()
    } else {
        format!("template `{name}`")
    }
}

/// The macro names a literal or dynamic template by where the call is.
fn is_call_site(name: &str) -> bool {
    name.starts_with("inline template") || name.starts_with("dynamic template")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> __Vars {
        __Vars::new()
    }

    // ---- pure ----

    #[test]
    fn first_load_finds_every_statement_and_its_line() {
        assert_eq!(first_load("{% include 'x' %}"), Some(("include", 1)));
        assert_eq!(first_load("a\nb\n{%- extends x -%}"), Some(("extends", 3)));
        assert_eq!(first_load("{% import 'm' as m %}"), Some(("import", 1)));
        assert_eq!(first_load("\n{% from 'm' import f %}"), Some(("from", 2)));
        assert_eq!(
            first_load(
                "{% if false %}{% macro m() %}{% include 'x' ignore missing %}{% endmacro %}{% endif %}"
            ),
            Some(("include", 1))
        );
    }

    #[test]
    fn first_load_ignores_raw_text_strings_and_names() {
        for text in [
            "{% raw %}{% include 'x' %}{% endraw %}",
            "{{ '{% include \"x\" %}' }}",
            "{% set include = 1 %}{{ include }}{% for import in [1] %}{% endfor %}",
            "include extends import from",
        ] {
            assert_eq!(first_load(text), None, "{text}");
        }
    }

    // ---- rendering ----

    #[test]
    fn the_dynamic_environment_has_no_loader() {
        // A real file that a loader of any kind would find by this name.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("planted.j2");
        std::fs::write(&path, "PLANTED-MARKER").unwrap();
        let name = path.to_string_lossy().into_owned();

        let env = environment();
        let err = env.get_template(&name).expect_err("no loader, no template");
        assert_eq!(err.kind(), ErrorKind::TemplateNotFound);
        assert!(!err.to_string().contains("PLANTED-MARKER"));
        let err = env.get_template("planted.j2").err().unwrap();
        assert_eq!(err.kind(), ErrorKind::TemplateNotFound);
    }

    #[test]
    fn a_dynamic_template_with_a_load_fails_before_it_renders() {
        let err = __render_dynamic(
            "dynamic template at x.rs:1",
            "{% include 'y' ignore missing %}",
            vars(),
        )
        .unwrap_err()
        .chain();
        assert!(
            err.starts_with(
                "dynamic template at x.rs:1, line 1: `{% include %}`: includes are only \
                 available for templates known at compile time"
            ),
            "{err}"
        );
    }

    static UNRESOLVED: __Embedded = __Embedded {
        name: "top.j2",
        files: &[
            ("top.j2", "[{% include 'x.j2' ignore missing %}]"),
            ("x.j2", "another file of the same bare name"),
        ],
        links: &[],
    };

    #[test]
    fn a_target_the_macro_did_not_resolve_never_loads_another_file_by_its_name() {
        assert_eq!(__render_embedded(&UNRESOLVED, vars()).unwrap(), "[]");
    }

    static RESOLVED: __Embedded = __Embedded {
        name: "top.j2",
        files: &[
            ("top.j2", "[{% include 'x.j2' %}]\n"),
            ("dir/x.j2", "{{ who }}"),
        ],
        links: &[("top.j2", "x.j2", "dir/x.j2")],
    };

    #[test]
    fn a_target_loads_the_file_the_macro_resolved_it_to() {
        let out = __render_embedded(&RESOLVED, vars().named("who", "w")).unwrap();
        assert_eq!(out, "[w]\n");
        let err = __render_embedded(&RESOLVED, vars()).unwrap_err().chain();
        assert_eq!(
            err,
            "template `dir/x.j2` line 1: undefined value: `who` is undefined (included from \
             template `top.j2` line 1)"
        );
    }

    static FOREVER: __Embedded = __Embedded {
        name: "f.j2",
        files: &[("f.j2", "x{% include 'f.j2' %}")],
        links: &[("f.j2", "f.j2", "f.j2")],
    };

    #[test]
    fn a_deep_include_chain_is_counted_not_listed() {
        let (tx, rx) = std::sync::mpsc::channel();
        // Time-bounded, so a regression to a hang fails rather than wedges.
        std::thread::Builder::new()
            .spawn(move || tx.send(__render_embedded(&FOREVER, vars()).map_err(|e| e.chain())))
            .unwrap();
        let err = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .unwrap()
            .unwrap_err();
        assert!(err.starts_with("template `f.j2` line 1: invalid operation: recursion limit exceeded (included from template `f.j2` line 1, from "), "{err}");
        assert_eq!(
            err.matches("from template").count(),
            SHOWN_INCLUDE_SITES,
            "{err}"
        );
        assert!(err.ends_with(" more)"), "{err}");
    }

    #[test]
    fn the_spread_must_be_a_map_with_string_keys() {
        let err = __render_dynamic("dynamic template", "x", vars().spread(&5))
            .unwrap_err()
            .chain();
        assert_eq!(
            err,
            "dynamic template: the `..` spread must serialize to a map (a struct, a `HashMap`, a \
             `json!({..})` object), but it is number `5`"
        );
        let mut by_number = std::collections::BTreeMap::new();
        by_number.insert(1, "a");
        let err = __render_dynamic("dynamic template", "x", vars().spread(&by_number))
            .unwrap_err()
            .chain();
        assert!(
            err.contains("keys are variable names, so they must be strings, but one is number `1`"),
            "{err}"
        );
    }

    #[test]
    fn a_value_that_does_not_serialize_fails_naming_its_argument() {
        let bad = crate::Secret::new(vec![0xff, 0xfe]);
        let err = __render_dynamic("dynamic template", "{{ s }}", vars().named("s", &bad))
            .unwrap_err()
            .chain();
        assert!(
            err.starts_with("dynamic template: argument `s` cannot be given to a template: "),
            "{err}"
        );
        assert!(err.contains("secret is not utf-8"), "{err}");
    }

    #[test]
    fn named_arguments_override_the_spread() {
        let mut map = std::collections::BTreeMap::new();
        map.insert("a", 1);
        map.insert("b", 2);
        let out =
            __render_dynamic("d", "{{ a }}{{ b }}", vars().named("a", &9).spread(&map)).unwrap();
        assert_eq!(out, "92");
    }
}
