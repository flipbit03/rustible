//! `template!` end to end (T1): what a template renders, what a render
//! fails with, includes and inheritance through the embedded loader, the
//! refusal of every load in a dynamic template, and a secret rendered
//! through a template masked in a step's events.
//!
//! Every call goes through the real macro, so each also checks what the
//! macro accepts at compile time. What it refuses at compile time is in
//! `rustible-macros`' trybuild cases and its `template` unit tests; every
//! built-in filter, test and function is in `template_builtins.rs`.

// The dynamic-template refusals plant real files on this machine to prove
// none is read. Vision 7.2's `sys` rule is about operations on a target.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use rustible::prelude::*;
use rustible::sdk::backend::Fake;
use rustible::sdk::event::Collect;
use rustible::sdk::{System, testing};
use rustible::std_ops::file;

#[derive(rustible::serde::Serialize)]
#[serde(crate = "rustible::serde")]
struct Vhost {
    host: String,
    upstream: String,
    websocket: bool,
}

fn vhosts() -> Vec<Vhost> {
    vec![
        Vhost {
            host: "a.example.com".into(),
            upstream: "127.0.0.1:8080".into(),
            websocket: true,
        },
        Vhost {
            host: "b.example.com".into(),
            upstream: "127.0.0.1:9000".into(),
            websocket: false,
        },
    ]
}

// ---- rendering ----

#[test]
fn loops_and_conditionals_render() {
    let out = template!(
        "{% for f in flags %}{% if not loop.first %} {% endif %}{{ f }}{% endfor %}",
        flags = &["-a", "-b", "-c"],
    )
    .unwrap();
    assert_eq!(out, "-a -b -c");

    let out = template!(
        "JAVA_OPTS=-Xmx{{ heap }}{% for f in flags %} {{ f }}{% endfor %}",
        heap = "2G",
        flags = &["-XX:+UseG1GC"],
    )
    .unwrap();
    assert_eq!(out, "JAVA_OPTS=-Xmx2G -XX:+UseG1GC");
}

#[test]
fn trim_blocks_removes_the_newline_after_a_tag_and_the_trailing_newline_stays() {
    let out = template!(
        "start\n{% for i in items %}\nitem {{ i }}\n{% endfor %}\nend\n",
        items = &[1, 2],
    )
    .unwrap();
    assert_eq!(out, "start\nitem 1\nitem 2\nend\n");
    // `{%-` strips the whitespace before the tag; the newline after a block
    // tag goes, the one after a variable tag stays.
    assert_eq!(
        template!("a  {%- if x %}b{% endif %}\n{{ y }}\n", x = true, y = 1).unwrap(),
        "ab1\n"
    );
}

#[test]
fn booleans_and_none_render_as_jinja_renders_them() {
    // As in Jinja2 (and Ansible): Python's spelling. A config file that
    // wants `true` says so with a filter.
    assert_eq!(
        template!(
            "{{ t }} {{ f }} {{ n }} {{ t | lower }} {{ t | tojson }} {{ n | tojson }}",
            t = true,
            f = false,
            n = &None::<u8>,
        )
        .unwrap(),
        "True False None true true null"
    );
}

#[test]
fn an_included_file_keeps_its_final_newline_so_one_include_per_line_works() {
    // `vhost.j2` ends with a newline, and the newline after the include tag
    // goes (`trim_blocks`), so each include is exactly its own lines. A
    // fragment meant to sit mid-line is written without a final newline.
    assert_eq!(
        template!(
            "[{% include \"templates/shared/leaf.j2\" %}]\n{% for v in vhosts %}\n{% include \"templates/caddy/vhost.j2\" %}\n{% endfor %}",
            who = "w",
            vhosts = &vhosts()[1..],
        )
        .unwrap(),
        "[leaf w]\nb.example.com {\n    reverse_proxy 127.0.0.1:9000\n}\n"
    );
    // Every trailing newline of the template stays, as Ansible keeps them.
    assert_eq!(template!("x\n\n").unwrap(), "x\n\n");
}

#[test]
fn nothing_is_autoescaped_whatever_the_file_is_called() {
    assert_eq!(
        template!("{{ v }}", v = "<a href=\"x\">&amp;</a>").unwrap(),
        "<a href=\"x\">&amp;</a>"
    );
    // minijinja's default would JSON-escape a `.yaml` name and HTML-escape
    // an `.html` one.
    assert_eq!(
        template!(include_str!("templates/escape/app.yaml.j2"), v = "a\"b").unwrap(),
        "k: a\"b\n"
    );
    assert_eq!(
        template!(include_str!("templates/escape/page.html.j2"), v = "<b>&").unwrap(),
        "<p><b>&</p>\n"
    );
}

#[test]
fn a_missing_name_or_attribute_is_an_error_never_empty_text() {
    let dynamic = "{{ host }} {{ v.upstrem }}".to_string();
    let err = template!(&dynamic, host = "h", v = &vhosts()[0])
        .unwrap_err()
        .chain();
    assert!(err.contains("`v.upstrem` is undefined"), "{err}");
    assert!(
        err.starts_with("dynamic template at tests/template.rs:"),
        "{err}"
    );
    assert!(err.contains(", line 1: undefined value"), "{err}");

    let err = template!(&dynamic, v = &vhosts()[0]).unwrap_err().chain();
    assert!(err.contains("`host` is undefined"), "{err}");

    // An attribute of `none` is undefined too.
    let err = template!("{{ x.y }}", x = &None::<String>)
        .unwrap_err()
        .chain();
    assert!(err.contains("undefined"), "{err}");
    assert!(
        err.starts_with("inline template at tests/template.rs:"),
        "{err}"
    );
    // Named by its call already, so no `(rendered at ..)` as a file's has.
    assert!(!err.contains("rendered at"), "{err}");

    // Optional values are said so.
    assert_eq!(
        template!(
            "{{ x | default('none') }}|{% if x %}set{% else %}unset{% endif %}",
            x = &None::<u8>
        )
        .unwrap(),
        "None|unset"
    );
    assert_eq!(
        template!("{{ x | default('off', true) }}", x = &None::<u8>).unwrap(),
        "off"
    );
    let missing = "{{ y | default('d') }}|{% if y is defined %}{{ y }}{% endif %}".to_string();
    assert_eq!(template!(&missing).unwrap(), "d|");
}

#[test]
fn an_undefined_value_given_to_a_built_in_is_an_error_never_empty_text() {
    let v = rustible::json!({ "names": ["a", "b"], "items": [{ "a": 1 }], "sep": "," });
    for (text, said) in [
        // An argument, the input, a keyword argument: each named.
        (
            "{{ v.names | join(v.seperator) }}",
            "`v.seperator` is undefined",
        ),
        ("{{ v.itmes | join(',') }}", "`v.itmes` is undefined"),
        ("{{ v.itmes | tojson }}", "`v.itmes` is undefined"),
        ("{{ 'ab' | indent(v.indnet) }}", "`v.indnet` is undefined"),
        (
            "{{ v.names | sort(reverse=v.rev) }}",
            "`v.rev` is undefined",
        ),
        ("{{ v.nope | default(v.nope2) }}", "`v.nope2` is undefined"),
        ("{{ dict(a=v.nope) }}", "`v.nope` is undefined"),
        ("{{ range(v.nope) }}", "`v.nope` is undefined"),
        ("{{ v.nope is string }}", "`v.nope` is undefined"),
        // An item of a list: what `map` makes of a missing attribute.
        (
            "{{ v.items | map(attribute='b') | join(',') }}",
            "undefined value",
        ),
        (
            "{{ [v.nope] | join(',') }}",
            "filter `join` was given a list holding an undefined item",
        ),
    ] {
        let dynamic = text.to_string();
        let err = template!(&dynamic, v = &v).unwrap_err().chain();
        assert!(err.contains(said), "{text}: {err}");
        assert!(err.contains(", line 1: undefined value"), "{text}: {err}");
    }
    // What exists to take an undefined value still does, and a defined
    // value that holds one (a `loop` at its edges) or holds itself (a
    // `namespace`) is given to a built-in as before.
    let fine = "{{ v.nope | default('d') }} {{ v.nope is defined }} {{ v.nope is undefined }} \
                {{ v.names | join(v.sep) }}\
                {% for n in v.names %} {{ loop | length }}{% endfor %}\
                {% set ns = namespace(a=1) %}{% set ns.me = ns %} {{ ns | length }}"
        .to_string();
    assert_eq!(
        template!(&fine, v = &v).unwrap(),
        "d False True a,b 11 11 2"
    );
    // A template that handles the missing items itself still renders.
    let users = rustible::json!([{ "email": "b@x" }, {}, {}]);
    let handled = "{{ users | map(attribute='email') | reject('undefined') | list }} \
                   {{ users | map(attribute='email') | select('defined') | list }} \
                   {{ users | map(attribute='email') | map('default', 'z') | join(',') }} \
                   {{ users | map(attribute='email') | map('d', 'y') | join(',') }} \
                   {{ users | map(attribute='email') | length }}"
        .to_string();
    assert_eq!(
        template!(&handled, users = &users).unwrap(),
        "['b@x'] ['b@x'] b@x,z,z b@x,y,y 3"
    );
    // But a truth test or a comparison does not handle them: it gets them
    // wrong, so it refuses them.
    for text in [
        "{{ users | map(attribute='emial') | select | join(',') }}",
        "{% if 'b@x' is in(users | map(attribute='emial')) %}{% endif %}",
        "{% if [users[0].emial] is eq(['b@x']) %}{% endif %}",
    ] {
        let dynamic = text.to_string();
        let err = template!(&dynamic, users = &users).unwrap_err().chain();
        assert!(
            err.contains("a list holding an undefined item"),
            "{text}: {err}"
        );
    }
}

#[test]
fn the_check_stays_linear_in_a_loop_over_large_data() {
    let users: Vec<_> = (0..20_000)
        .map(|i| rustible::json!({ "name": format!("u{i}"), "a": 1, "b": 2, "c": 3, "d": 4 }))
        .collect();
    let started = std::time::Instant::now();
    let out = template!(
        "{% for u in users %}{{ users | length }}{{ users | count }}{{ users | bool }}\
         {{ (users | first).a }}{{ (users | reverse | first).a }}{{ users is sequence }}\
         {% if u == users | last %}!{% endif %}{% endfor %}",
        users = &users,
    )
    .unwrap();
    assert!(out.starts_with("2000020000True11True"), "{}", &out[..40]);
    assert!(out.ends_with("2000020000True11True!"));
    // These take no look at the items, so this is linear; a check that
    // walked them on every call is quadratic, minutes here.
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn named_arguments_win_over_the_spread_wherever_they_are_written() {
    let settings = rustible::json!({ "listen": "0.0.0.0:80", "env": "dev", "workers": 4 });
    let out = template!(
        "{{ listen }} {{ env }} {{ workers }}",
        env = "prod",
        ..&settings,
    )
    .unwrap();
    assert_eq!(out, "0.0.0.0:80 prod 4");

    // A struct spreads its fields.
    let v = &vhosts()[0];
    assert_eq!(
        template!(
            "{{ host }} {{ upstream }} {{ websocket }}",
            ..v,
            host = "over"
        )
        .unwrap(),
        "over 127.0.0.1:8080 True"
    );

    // A map with any keys, for names known only at run time.
    let mut map = std::collections::BTreeMap::new();
    map.insert("dynamic_name".to_string(), 7);
    let text = "{{ dynamic_name }}".to_string();
    assert_eq!(template!(&text, ..&map).unwrap(), "7");
}

#[test]
fn a_spread_that_is_not_a_map_fails_naming_it() {
    let err = template!("{{ x }}", ..&[1, 2]).unwrap_err().chain();
    assert!(
        err.contains("the `..` spread must serialize to a map"),
        "{err}"
    );
    assert!(err.contains("seq"), "{err}");
}

#[test]
fn any_name_can_be_passed_a_rust_keyword_too() {
    assert_eq!(
        template!("{{ type }} {{ match }} {{ loop_ }}", type = "t", r#match = "m", loop_ = 1)
            .unwrap(),
        "t m 1"
    );
}

#[test]
fn typed_and_ad_hoc_nested_data() {
    let ops = [
        rustible::json!({ "name": "alice", "level": 4 }),
        rustible::json!({ "name": "bob",   "level": 2 }),
    ];
    let out = template!(
        "{% for op in ops %}{{ op.name }} (level {{ op.level }})\n{% endfor %}",
        ops = &ops,
    )
    .unwrap();
    assert_eq!(out, "alice (level 4)\nbob (level 2)\n");

    // A struct's fields iterate in declaration order.
    let v = &vhosts()[1];
    assert_eq!(
        template!(
            "{% for k, x in v|items %}{{ k }}={{ x }};{% endfor %}",
            v = v
        )
        .unwrap(),
        "host=b.example.com;upstream=127.0.0.1:9000;websocket=False;"
    );
}

#[test]
fn a_dynamic_template_takes_any_str() {
    let owned: String = "hello {{ who }}".into();
    assert_eq!(template!(&owned, who = "a").unwrap(), "hello a");
    assert_eq!(template!(owned.as_str(), who = "b").unwrap(), "hello b");
    assert_eq!(template!(owned, who = "c").unwrap(), "hello c");
    let err = template!("{% if %}".to_string()).unwrap_err().chain();
    assert!(err.contains("syntax error"), "{err}");
}

// ---- files, includes, inheritance ----

#[test]
fn a_file_includes_relative_to_itself_and_the_include_sees_the_loop_variable() {
    let conf = template!(
        include_str!("templates/Caddyfile.j2"),
        email = "admin@example.com",
        vhosts = &vhosts(),
    )
    .unwrap();
    assert_eq!(
        conf,
        "{\n    email admin@example.com\n}\n\
         a.example.com {\n    reverse_proxy 127.0.0.1:8080\n    header_up Connection {>Connection}\n}\n\
         b.example.com {\n    reverse_proxy 127.0.0.1:9000\n}\n"
    );
}

#[test]
fn includes_resolve_from_each_including_file_at_every_depth() {
    // one.j2 -> two/two.j2 -> two/three/three.j2 -> ../../../shared/leaf.j2
    assert_eq!(
        template!(include_str!("templates/nest/one.j2"), who = "w").unwrap(),
        "one[two[three[leaf w]]]\n"
    );
}

#[test]
fn an_inline_literal_includes_relative_to_the_playbook_file() {
    assert_eq!(
        template!("<{% include \"templates/shared/leaf.j2\" %}>", who = "x").unwrap(),
        "<leaf x>"
    );
}

#[test]
fn an_error_in_an_included_file_names_that_file_its_line_and_the_include_sites() {
    let line = line!() + 1;
    let err = template!(include_str!("templates/nest_err/one.j2"), who = "w")
        .unwrap_err()
        .chain();
    // The call site last: one file may be rendered from several calls.
    assert_eq!(
        err,
        format!(
            "template `tests/templates/nest_err/two/three/three.j2` line 2: undefined value: \
             `whom` is undefined (reached from template `tests/templates/nest_err/two/two.j2` \
             line 2, from template `tests/templates/nest_err/one.j2` line 2) (rendered at \
             tests/template.rs:{line})"
        )
    );
}

#[test]
fn a_child_outputs_only_its_blocks_and_super_renders_the_parent_block() {
    let unit = template!(
        include_str!("templates/inherit/mine-world.service.j2"),
        description = "Minecraft",
        heap = "4G",
    )
    .unwrap();
    assert_eq!(
        unit,
        "[Unit]\nDescription=Minecraft\n[Service]\nType=simple\n\
         ExecStart=/usr/bin/java -Xmx4G -jar server.jar nogui\n\
         [Install]\nWantedBy=default.target\n"
    );
}

#[test]
fn a_conditional_extends_reaches_either_parent() {
    let a = template!(include_str!("templates/inherit/conditional.j2"), alt = true).unwrap();
    let b = template!(
        include_str!("templates/inherit/conditional.j2"),
        alt = false
    )
    .unwrap();
    assert_eq!((a.as_str(), b.as_str()), ("A<child>", "B<child>"));
}

#[test]
fn an_include_list_takes_the_first_that_exists_and_ignore_missing_renders_nothing() {
    assert_eq!(
        template!(include_str!("templates/lists/list.j2"), x = 1).unwrap(),
        "present 1||"
    );
}

#[test]
fn import_and_from_import_bring_macros() {
    assert_eq!(
        template!(include_str!("templates/macros/use.j2"), who = "ann").unwrap(),
        "hello ann ANN!"
    );
}

#[test]
fn set_in_an_included_file_inside_a_loop_does_not_leak_back_and_trim_blocks_applies_inside() {
    assert_eq!(
        template!(include_str!("templates/lists/scope.j2")).unwrap(),
        "inner1[yes]after-if\n"
    );
}

#[test]
fn set_in_an_included_file_outside_a_loop_replaces_the_includer_s_variable() {
    // minijinja, unlike Jinja2: documented on `template!`.
    assert_eq!(
        template!(include_str!("templates/lists/leak.j2")).unwrap(),
        "[inc]"
    );
    assert_eq!(
        template!(
            "{% include \"templates/lists/setx.j2\" %}[{{ x }}]",
            x = "passed"
        )
        .unwrap(),
        "[inc]"
    );
}

#[test]
fn the_same_target_written_in_two_files_resolves_from_each() {
    // `leaf.j2` is a different file beside each template that names it:
    // blocks run from the child and from the parent, `super()`, a macro
    // defined in another directory, and a `call` body.
    assert_eq!(
        template!(include_str!("templates/rel/child/child.j2")).unwrap(),
        "[CHILD|BASE|BASE+CHILD]"
    );
    assert_eq!(
        template!(include_str!("templates/rel/use/use.j2")).unwrap(),
        "LIB/USE/<USE>"
    );
}

#[test]
fn strict_undefined_holds_inside_an_included_file() {
    let err = template!(
        "{% include \"templates/shared/leaf.j2\" %}",
        ..&rustible::json!({})
    )
    .unwrap_err()
    .chain();
    assert!(
        err.contains("template `tests/templates/shared/leaf.j2` line 1"),
        "{err}"
    );
    assert!(err.contains("`who` is undefined"), "{err}");
}

#[test]
fn an_error_inside_a_super_block_names_the_parent_s_file_and_line() {
    let err = template!(include_str!("templates/superr/child.j2"))
        .unwrap_err()
        .chain();
    assert!(
        err.starts_with(
            "template `tests/templates/superr/base.j2` line 1: undefined value: `missing` is \
             undefined (reached from template `tests/templates/superr/child.j2` line 1) \
             (rendered at tests/template.rs:"
        ),
        "{err}"
    );
}

// ---- recursion ----

#[test]
fn a_self_include_renders_a_finite_tree() {
    let tree = rustible::json!({
        "name": "root",
        "children": [
            { "name": "a", "children": [ { "name": "a1", "children": [] } ] },
            { "name": "b", "children": [] },
        ],
    });
    assert_eq!(
        template!(include_str!("templates/tree/tree.j2"), node = &tree).unwrap(),
        "root(a(a1),b)"
    );
}

#[test]
fn a_cycle_between_two_files_renders_as_far_as_its_data_goes() {
    assert_eq!(
        template!(include_str!("templates/tree/cycle_a.j2"), n = 3).unwrap(),
        "a3b2a1b0"
    );
}

#[test]
fn infinite_recursion_fails_at_the_limit_and_does_not_hang() {
    let (tx, rx) = mpsc::channel();
    // On a thread of its own, so a hang fails at the timeout instead of
    // wedging the run. minijinja's limit (500 levels) fits in 256 KiB of
    // stack in a release build and about 1 MiB in a debug one.
    std::thread::Builder::new()
        .spawn(move || {
            let r = template!(include_str!("templates/tree/forever.j2")).map_err(|e| e.chain());
            tx.send(r).unwrap();
        })
        .unwrap();
    let err = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("an infinite include must fail, not hang")
        .unwrap_err();
    assert!(err.contains("recursion limit exceeded"), "{err}");
    assert!(
        err.starts_with("template `tests/templates/tree/forever.j2` line 1"),
        "{err}"
    );
    assert!(
        err.contains(", and "),
        "the include sites are counted, not listed: {err}"
    );
    assert!(err.len() < 1000, "{err}");
}

#[test]
fn an_import_cycle_fails_at_the_recursion_limit_and_does_not_hang() {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .spawn(move || {
            tx.send(template!(include_str!("templates/importcycle/a.j2")).map_err(|e| e.chain()))
                .unwrap();
        })
        .unwrap();
    let err = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("an import cycle must end, not hang")
        .unwrap_err();
    assert!(err.contains("recursion limit exceeded"), "{err}");
}

// ---- dynamic templates read nothing ----

/// A real file holding a marker no template could produce, so a test can
/// tell whether it was read.
struct Planted {
    dir: tempfile::TempDir,
    marker: String,
}

impl Planted {
    fn new() -> Planted {
        let dir = tempfile::tempdir().unwrap();
        let marker = format!("PLANTED-MARKER-{}", std::process::id());
        for name in ["target.j2", "other.j2"] {
            std::fs::write(
                dir.path().join(name),
                format!("{marker} {{% macro m() %}}{marker}{{% endmacro %}}{{% block b %}}{marker}{{% endblock %}}"),
            )
            .unwrap();
        }
        Planted { dir, marker }
    }

    fn abs(&self, name: &str) -> String {
        self.dir.path().join(name).to_string_lossy().into_owned()
    }
}

#[test]
fn every_load_in_a_dynamic_template_is_refused_and_reads_nothing() {
    let planted = Planted::new();
    let abs = planted.abs("target.j2");
    let other = planted.abs("other.j2");
    // A relative target, as minijinja would join it: from this process's
    // working directory, which holds the file for this test.
    let cwd = std::env::current_dir().unwrap();
    let relative = pathdiff(&planted.dir.path().join("target.j2"), &cwd);
    let cases = [
        (format!("{{% include \"{abs}\" %}}"), "include"),
        (format!("{{% include \"{relative}\" %}}"), "include"),
        (
            format!("a\n{{% include [\"{abs}\", \"{other}\"] %}}"),
            "include",
        ),
        (
            format!("{{% include \"{abs}\" ignore missing %}}"),
            "include",
        ),
        (
            format!("{{% if false %}}{{% include \"{abs}\" %}}{{% endif %}}"),
            "include",
        ),
        (format!("{{% extends \"{abs}\" %}}"), "extends"),
        (
            format!("{{% import \"{abs}\" as t %}}{{{{ t.m() }}}}"),
            "import",
        ),
        (
            format!("{{% from \"{relative}\" import m %}}{{{{ m() }}}}"),
            "from",
        ),
        ("{% include name %}".to_string(), "include"),
    ];
    for (text, statement) in cases {
        let err = template!(&text, name = &abs).unwrap_err().chain();
        assert!(
            err.contains("includes are only available for templates known at compile time"),
            "{text}: {err}"
        );
        assert!(
            err.contains(&format!("`{{% {statement} %}}`")),
            "{text}: {err}"
        );
        assert!(
            err.starts_with("dynamic template at tests/template.rs:"),
            "{err}"
        );
        assert!(
            !err.contains(&planted.marker),
            "{text} read the planted file: {err}"
        );
    }
    // The second case's statement is on line 2, and the message says so.
    let text = format!("a\n{{% include \"{abs}\" %}}");
    let err = template!(&text).unwrap_err().chain();
    assert!(err.contains(", line 2: "), "{err}");
}

/// `to` relative to `from`, for an existing directory `from`.
fn pathdiff(to: &std::path::Path, from: &std::path::Path) -> String {
    let to: Vec<_> = to.components().collect();
    let from: Vec<_> = from.components().collect();
    let common = to.iter().zip(&from).take_while(|(a, b)| a == b).count();
    let mut out: Vec<String> = vec!["..".into(); from.len() - common];
    out.extend(
        to[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    out.join("/")
}

// ---- plain files, and secrets ----

#[test]
fn a_plain_copy_is_never_templated() {
    let fake = Arc::new(Fake::new().with_dir("/etc"));
    let sys = System::fake(fake.clone(), Arc::new(Collect::default()));
    let mut ctx = testing::ctx(sys);
    ctx.step(
        "plain",
        file::Copy::from_str(include_str!("templates/plain.txt")).to("/etc/plain"),
    )
    .unwrap();
    assert_eq!(
        fake.content("/etc/plain").unwrap(),
        "raw {{ not_a_variable }} stays\n"
    );
}

#[test]
fn a_secret_rendered_through_a_template_or_an_include_is_masked_in_events() {
    // `ctx.local_secret` registers what it streams exactly as `Secret::new`
    // does (both end in `Secret::registered`); a test has no orchestrator to
    // stream from.
    let token = Secret::new("s3cr3t-t0ken-value");
    let sink = Arc::new(Collect::default());
    let fake = Arc::new(Fake::new().with_dir("/etc"));
    let sys = System::fake(fake.clone(), sink.clone());
    let mut ctx = testing::ctx(sys);

    let direct = template!(include_str!("templates/secret.conf.j2"), token = &token).unwrap();
    let included = template!(include_str!("templates/secret_outer.j2"), token = &token).unwrap();
    assert_eq!(direct, "token=s3cr3t-t0ken-value\n");
    assert_eq!(included, "outer\ntoken=s3cr3t-t0ken-value\n");
    ctx.step(
        "direct",
        file::Copy::from_str(&direct).to("/etc/direct.conf"),
    )
    .unwrap();
    ctx.step(
        "included",
        file::Copy::from_str(&included).to("/etc/included.conf"),
    )
    .unwrap();
    // What lands on disk is the plaintext: masking is for what is reported.
    assert_eq!(fake.content("/etc/direct.conf").unwrap(), direct);

    // Through `tojson`, whose escapes for HTML the masker would not know.
    let html = Secret::new("pa'ss<wo>rd&xyz");
    let json = template!("{\"password\": {{ p | tojson }}}\n", p = &html).unwrap();
    assert_eq!(json, "{\"password\": \"pa'ss<wo>rd&xyz\"}\n");
    ctx.step("json", file::Copy::from_str(&json).to("/etc/secret.json"))
        .unwrap();

    let events = rustible::sdk::__private::serde_json::to_string(&sink.events()).unwrap();
    assert!(!events.contains("s3cr3t-t0ken-value"), "{events}");
    assert!(
        !events.contains("rd&xyz") && !events.contains("u0026xyz"),
        "{events}"
    );
    assert_eq!(events.matches("token=<secret>").count(), 2, "{events}");
}
