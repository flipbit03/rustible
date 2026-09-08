//! Inventory tests: the vision 10.2.2 example end to end, every load-time
//! error in 10.2.3 and the brief, precedence, sibling conflicts, the vars
//! validation report of 10.3, slash-dash.

use serde_json::json;

use super::*;

const VISION: &str = include_str!("../../testdata/vision/hosts.kdl");

fn load(src: &str) -> Inventory {
    match Inventory::parse(src, "hosts.kdl") {
        Ok(inv) => inv,
        Err(errs) => panic!("expected a clean load, got:\n{errs}"),
    }
}

fn errors(src: &str) -> Vec<LoadError> {
    match Inventory::parse(src, "hosts.kdl") {
        Ok(_) => panic!("expected errors, loaded fine"),
        Err(errs) => errs.0,
    }
}

/// The one error a source must produce, checked by line, column and text.
fn one_error(src: &str, line: usize, column: usize, contains: &str) {
    let errs = errors(src);
    assert_eq!(
        errs.len(),
        1,
        "expected one error, got:\n{}",
        LoadErrors(errs.clone())
    );
    let e = &errs[0];
    assert_eq!((e.line, e.column), (line, column), "position of: {e}");
    assert!(
        e.message.contains(contains),
        "message `{}` lacks `{contains}`",
        e.message
    );
    assert_eq!(e.file, "hosts.kdl");
}

fn s(v: &str) -> Scalar {
    Scalar::Str(v.into())
}

fn group(g: &str) -> Source {
    Source::Group(g.into())
}

// --- vision 10.2.2 ---------------------------------------------------------

#[test]
fn vision_example_loads() {
    let inv = load(VISION);
    assert_eq!(inv.host_names(), ["laptop", "web1", "web2", "db1", "db2"]);
    assert_eq!(inv.group_names(), ["web", "db", "production", "monitored"]);
    assert_eq!(inv.defaults.ssh_user.as_deref(), Some("cadu"));
    assert_eq!(inv.defaults.port, Some(22));
    assert_eq!(inv.defaults.escalate, Some(Escalate::Sudo));
    assert_eq!(inv.vars["fruit"], s("banana"));
    assert_eq!(inv.groups["web"].hosts, ["web1", "web2"]);
    assert_eq!(inv.groups["production"].members, ["web", "db"]);
    assert_eq!(inv.hosts["web2"].group.as_deref(), Some("web"));
    assert_eq!(inv.hosts["laptop"].group, None);
}

#[test]
fn vision_web2_resolves_with_sources() {
    let inv = load(VISION);
    let r = inv.resolve("web2").unwrap();
    assert_eq!(r.groups, ["web", "production"]);

    let p = &r.params;
    assert_eq!(p.addr.as_deref(), Some("10.0.1.12"));
    assert_eq!(p.connection, Connection::Ssh);
    assert_eq!(p.ssh_user, "deploy");
    assert_eq!(p.port, 22);
    assert_eq!(p.escalate, Escalate::Sudo);
    assert_eq!(p.escalate_user, "root");
    assert!(p.ssh_args.is_empty());
    let ps = &r.sources.params;
    assert_eq!(ps["addr"], Source::Host);
    assert_eq!(ps["connection"], Source::BuiltIn);
    assert_eq!(ps["ssh_user"], group("web"));
    assert_eq!(ps["port"], Source::Defaults);
    assert_eq!(ps["escalate"], Source::Defaults);
    assert_eq!(ps["escalate_user"], Source::BuiltIn);
    assert_eq!(ps["ssh_args"], Source::BuiltIn);
    assert_eq!(
        r.sources.overridden_params,
        [Overridden {
            key: "ssh_user".into(),
            value: "\"cadu\"".into(),
            source: Source::Defaults
        }]
    );

    let v = &r.vars;
    assert_eq!(v["fruit"], s("banana"));
    assert_eq!(v["timezone"], s("America/Sao_Paulo"));
    assert_eq!(v["nginx_workers"], Scalar::Int(8));
    assert_eq!(
        v["allowed_ports"],
        Scalar::List(vec![Scalar::Int(22), Scalar::Int(80), Scalar::Int(443)])
    );
    assert_eq!(v["tls"], Scalar::Bool(true));
    assert_eq!(v["env"], s("production"));
    assert_eq!(v.len(), 6, "{v:?}");
    let vs = &r.sources.vars;
    assert_eq!(vs["fruit"], Source::All);
    assert_eq!(vs["timezone"], Source::All);
    assert_eq!(vs["nginx_workers"], Source::Host);
    assert_eq!(vs["allowed_ports"], group("web"));
    assert_eq!(vs["tls"], group("web"));
    assert_eq!(vs["env"], group("production"));
    assert_eq!(
        r.sources.overridden_vars,
        [Overridden {
            key: "nginx_workers".into(),
            value: "4".into(),
            source: group("web")
        }]
    );
}

#[test]
fn vision_other_hosts() {
    let inv = load(VISION);
    let web1 = inv.resolve("web1").unwrap();
    assert_eq!(web1.groups, ["web", "monitored", "production"]);
    assert_eq!(web1.vars["alerts"], Scalar::Bool(true));
    assert_eq!(web1.vars["nginx_workers"], Scalar::Int(4));

    let db1 = inv.resolve("db1").unwrap();
    assert_eq!(db1.groups, ["db", "monitored", "production"]);
    assert_eq!(db1.params.ssh_user, "pgadmin");
    assert_eq!(db1.params.port, 2222);
    assert_eq!(db1.sources.params["port"], Source::Host);
    assert_eq!(db1.vars["pg_version"], Scalar::Int(16));
    assert_eq!(db1.vars["role"], s("primary"));

    let laptop = inv.resolve("laptop").unwrap();
    assert!(laptop.groups.is_empty());
    assert_eq!(laptop.params.connection, Connection::Local);
    assert_eq!(laptop.params.addr, None);
    assert_eq!(laptop.params.ssh_user, "cadu");
    assert_eq!(laptop.vars.len(), 2);
}

#[test]
fn vision_show_layout() {
    let inv = load(VISION);
    let out = render_show(&inv.resolve("web2").unwrap());
    let expected = "\
host web2  (groups: web, production)

parameters
  addr           \"10.0.1.12\"          host
  connection     ssh                  built-in
  ssh_user       \"deploy\"             group web
  port           22                   defaults
  escalate       sudo                 defaults
  escalate_user  \"root\"               built-in
  ssh_args       []                   built-in

vars
  allowed_ports  [22, 80, 443]        group web
  env            \"production\"         group production
  fruit          \"banana\"             all
  nginx_workers  8                    host
  timezone       \"America/Sao_Paulo\"  all
  tls            #true                group web

overridden
  ssh_user       \"cadu\"               defaults
  nginx_workers  4                    group web
";
    assert_eq!(out, expected, "got:\n{out}");
}

#[test]
fn slash_dash_disables_a_host_with_its_children() {
    let inv = load(VISION);
    assert!(!inv.hosts.contains_key("web3"));
    assert!(inv.resolve("web3").is_err());
    // Inside a vars block too, and on a whole group.
    let inv = load(
        r#"
host "a" connection="local" {
    vars {
        keep 1
        /-drop 2
    }
}
/-group "gone" {
    host "b" addr="1"
}
"#,
    );
    assert_eq!(inv.hosts["a"].vars.len(), 1);
    assert!(inv.groups.is_empty());
    assert!(!inv.hosts.contains_key("b"));
}

#[test]
fn select_group_host_or_error() {
    let inv = load(VISION);
    fn names(v: Vec<&Host>) -> Vec<&str> {
        v.iter().map(|h| h.name.as_str()).collect()
    }
    assert_eq!(names(inv.select("web").unwrap()), ["web1", "web2"]);
    assert_eq!(
        names(inv.select("production").unwrap()),
        ["web1", "web2", "db1", "db2"]
    );
    assert_eq!(names(inv.select("monitored").unwrap()), ["web1", "db1"]);
    assert_eq!(names(inv.select("db2").unwrap()), ["db2"]);
    let e = inv.select("wb").unwrap_err();
    assert_eq!(
        e.to_string(),
        "no host or group named `wb`; did you mean `web`?"
    );
    let e = inv.select("zzzzz").unwrap_err();
    assert_eq!(e.to_string(), "no host or group named `zzzzz`");
}

// --- syntax forms ----------------------------------------------------------

#[test]
fn both_vars_forms_and_scalars() {
    let inv = load(
        r#"
vars a=1 b="x" c=#false
host "h" connection="local" {
    vars d=2.5 {
        e "y" "z"
        f #true
    }
}
"#,
    );
    assert_eq!(inv.vars["a"], Scalar::Int(1));
    assert_eq!(inv.vars["b"], s("x"));
    assert_eq!(inv.vars["c"], Scalar::Bool(false));
    let h = &inv.hosts["h"].vars;
    assert_eq!(h["d"], Scalar::Float(2.5));
    assert_eq!(h["e"], Scalar::List(vec![s("y"), s("z")]));
    assert_eq!(h["f"], Scalar::Bool(true));
    assert_eq!(
        bag_to_json(h),
        json!({"d": 2.5, "e": ["y", "z"], "f": true})
    );
}

#[test]
fn ssh_args_child_node_and_property() {
    let inv = load(
        r#"
defaults {
    ssh_args "-o" "StrictHostKeyChecking=no"
}
host "a" addr="1"
host "b" addr="2" ssh_args="-4"
"#,
    );
    let a = inv.resolve("a").unwrap();
    assert_eq!(a.params.ssh_args, ["-o", "StrictHostKeyChecking=no"]);
    assert_eq!(a.sources.params["ssh_args"], Source::Defaults);
    let b = inv.resolve("b").unwrap();
    assert_eq!(b.params.ssh_args, ["-4"]);
    assert_eq!(b.sources.params["ssh_args"], Source::Host);
}

// --- load-time errors (10.2.3 and brief item 3) -----------------------------

#[test]
fn error_unknown_parameter_with_did_you_mean() {
    one_error(
        "host \"a\" connection=\"local\" {\n    vars { x 1; x 2 }\n}\n",
        2,
        17,
        "var `x` is set twice on host `a`",
    );
    one_error(
        "host \"a\" connection=\"local\" {\n    vars { x #null }\n}\n",
        2,
        14,
        "var `x` on host `a` is #null",
    );
    one_error(
        "host \"a\" connection=\"local\" {\n    vars { x y=1 }\n}\n",
        2,
        14,
        "var `x` on host `a` takes values, not properties",
    );
    one_error(
        "host \"a\" connection=\"local\" {\n    vars { x { y 1 } }\n}\n",
        2,
        12,
        "var `x` on host `a` has a block; vars are scalars or lists, not nested",
    );
}

#[test]
fn error_structure() {
    one_error(
        "hosts \"a\"\n",
        1,
        1,
        "unknown node `hosts` at top level; did you mean `host`?",
    );
    one_error(
        "host \"a\" connection=\"local\" {\n    var { x 1 }\n}\n",
        2,
        5,
        "unknown node `var` in host `a`; did you mean `vars`?",
    );
    one_error(
        "group \"a\" {\n    group \"b\" {\n    }\n}\n",
        2,
        5,
        "group `b` inside group `a`: groups nest through `members`, not by placing a group inside a group",
    );
    one_error(
        "host addr=\"1\"\n",
        1,
        1,
        "`host` needs a name: host \"name\" ...",
    );
    one_error(
        "host \"a\" \"b\" addr=\"1\"\n",
        1,
        10,
        "host takes one name; unexpected extra argument \"b\"",
    );
    one_error(
        "host 1 addr=\"1\"\n",
        1,
        6,
        "host name must be a string, got 1",
    );
    one_error(
        "host \"a\" addr=\"1\" port=1 port=2\n",
        1,
        26,
        "parameter `port` is set twice on host `a`",
    );
    one_error(
        "defaults port=1\ndefaults port=2\n",
        2,
        1,
        "`defaults` is defined twice (first at line 1)",
    );
    one_error(
        "vars a=1\nvars b=2\n",
        2,
        1,
        "top-level `vars` is defined twice (first at line 1)",
    );
}

#[test]
fn errors_are_collected_not_first_only() {
    let src = include_str!("../../testdata/three-errors/hosts.kdl");
    let errs = errors(src);
    let lines: Vec<usize> = errs.iter().map(|e| e.line).collect();
    assert_eq!(lines, [5, 7, 10], "{}", LoadErrors(errs.clone()));
    assert!(
        errs[0]
            .message
            .contains("`addr` is not allowed on group `web`")
    );
    assert!(errs[1].message.contains("unknown parameter `sshuser`"));
    assert!(
        errs[2]
            .message
            .contains("parameter `port` on host `db1` must be an integer")
    );
    assert_eq!(
        errs[0].to_string(),
        format!("hosts.kdl:5:13: error: {}", errs[0].message)
    );
}

#[test]
fn positions_count_characters_not_bytes() {
    // `ã` is two bytes; the column after it must still be in characters.
    one_error(
        "// olá\nhost \"pão\" addr=\"1\" prot=22\n",
        2,
        21,
        "unknown parameter `prot` on host `pão`; did you mean `port`?",
    );
}

#[test]
fn load_reports_unreadable_file() {
    let e = Inventory::load("/nonexistent/hosts.kdl").unwrap_err();
    assert_eq!(e.len(), 1);
    assert!(e.0[0].message.starts_with("cannot read: "));
    assert_eq!((e.0[0].line, e.0[0].column), (1, 1));
}

// --- precedence --------------------------------------------------------------

#[test]
fn parameter_precedence_host_group_defaults_builtin() {
    let inv = load(
        r#"
defaults port=1 escalate="doas" escalate_user="admin"
group "outer" port=2 escalate="none" {
    members "inner"
}
group "inner" port=3 {
    host "h" addr="x" port=4
}
"#,
    );
    let r = inv.resolve("h").unwrap();
    assert_eq!(r.groups, ["inner", "outer"]);
    assert_eq!(r.params.port, 4);
    assert_eq!(r.sources.params["port"], Source::Host);
    assert_eq!(r.params.escalate, Escalate::None);
    assert_eq!(r.sources.params["escalate"], group("outer"));
    assert_eq!(r.params.escalate_user, "admin");
    assert_eq!(r.sources.params["escalate_user"], Source::Defaults);
    assert_eq!(r.params.connection, Connection::Ssh);
    assert_eq!(r.sources.params["connection"], Source::BuiltIn);
    assert_eq!(
        r.params.ssh_user,
        local_username().unwrap_or_else(|| "(ssh default)".to_string())
    );
    let over: Vec<(&str, &str, &Source)> = r
        .sources
        .overridden_params
        .iter()
        .map(|o| (o.key.as_str(), o.value.as_str(), &o.source))
        .collect();
    assert_eq!(
        over,
        [
            ("port", "3", &group("inner")),
            ("port", "2", &group("outer")),
            ("port", "1", &Source::Defaults),
            ("escalate", "doas", &Source::Defaults),
        ]
    );
}

#[test]
fn var_precedence_all_outer_inner_host() {
    let inv = load(
        r#"
vars { a "all"; b "all"; c "all"; d "all" }
group "outer" {
    members "inner"
    vars { b "outer"; c "outer"; d "outer" }
}
group "inner" {
    vars { c "inner"; d "inner" }
    host "h" connection="local" {
        vars { d "host" }
    }
}
"#,
    );
    let r = inv.resolve("h").unwrap();
    assert_eq!(r.vars["a"], s("all"));
    assert_eq!(r.vars["b"], s("outer"));
    assert_eq!(r.vars["c"], s("inner"));
    assert_eq!(r.vars["d"], s("host"));
    assert_eq!(r.sources.vars["a"], Source::All);
    assert_eq!(r.sources.vars["b"], group("outer"));
    assert_eq!(r.sources.vars["c"], group("inner"));
    assert_eq!(r.sources.vars["d"], Source::Host);
    assert_eq!(r.sources.overridden_vars.len(), 6);
}

#[test]
fn group_distance_is_shortest_path() {
    // `far` reaches h through mid (distance 2) and also lists h directly:
    // the direct listing counts, so far sits at distance 1 next to mid.
    let inv = load(
        r#"
group "far" {
    members "mid" "h"
    vars { y "far" }
}
group "mid" {
    members "h"
    vars { x "mid" }
}
host "h" connection="local"
"#,
    );
    assert_eq!(
        inv.group_levels("h"),
        [vec!["far".to_string(), "mid".to_string()]]
    );
    let r = inv.resolve("h").unwrap();
    assert_eq!(r.groups, ["far", "mid"]);
    assert_eq!(r.sources.vars["y"], group("far"));
    assert_eq!(r.sources.vars["x"], group("mid"));
}

// --- sibling conflicts (10.3) ------------------------------------------------

#[test]
fn sibling_conflict_is_a_load_error_naming_both_groups_and_host() {
    let src = r#"
group "a" {
    members "h"
    vars { x 1 }
}
group "b" {
    members "h"
    vars { x 2 }
}
host "h" connection="local"
"#;
    one_error(
        src,
        10,
        1,
        "host `h`: var `x` is defined by both group `a` and group `b` at the same distance; set it on host `h` or on a common parent group",
    );
}

#[test]
fn sibling_conflict_resolved_by_host_override() {
    let inv = load(
        r#"
group "a" {
    members "h"
    vars { x 1 }
}
group "b" {
    members "h"
    vars { x 2 }
}
host "h" connection="local" {
    vars { x 3 }
}
"#,
    );
    let r = inv.resolve("h").unwrap();
    assert_eq!(r.vars["x"], Scalar::Int(3));
    assert_eq!(r.sources.vars["x"], Source::Host);
}

#[test]
fn sibling_conflict_resolved_by_nearer_group() {
    let inv = load(
        r#"
group "a" {
    members "near"
    vars { x 1 }
}
group "b" {
    members "near"
    vars { x 2 }
}
group "near" {
    vars { x 3 }
    host "h" connection="local"
}
"#,
    );
    let r = inv.resolve("h").unwrap();
    assert_eq!(r.vars["x"], Scalar::Int(3));
    assert_eq!(r.sources.vars["x"], group("near"));
}

#[test]
fn sibling_conflict_on_a_parameter() {
    let src = r#"
group "a" port=1 {
    members "h"
}
group "b" port=2 {
    members "h"
}
host "h" addr="x"
"#;
    one_error(
        src,
        8,
        1,
        "host `h`: parameter `port` is defined by both group `a` and group `b` at the same distance; set it on host `h` or on a common parent group",
    );
}

#[test]
fn no_conflict_when_only_one_sibling_sets_the_key() {
    let inv = load(
        r#"
group "a" {
    members "h"
    vars { x 1 }
}
group "b" {
    members "h"
    vars { y 2 }
}
host "h" connection="local"
"#,
    );
    let r = inv.resolve("h").unwrap();
    assert_eq!(r.vars["x"], Scalar::Int(1));
    assert_eq!(r.vars["y"], Scalar::Int(2));
}

// --- vars validation (10.3) --------------------------------------------------

/// What `--describe` prints as `vars_schema` for the 10.3 `Vars` struct
/// (schemars 1.x; the shape M1 documented).
fn thing_schema() -> serde_json::Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Vars",
        "type": "object",
        "properties": {
            "user": { "type": "string" },
            "fruit": { "type": "string" },
            "port": { "type": ["integer", "null"], "format": "uint16", "minimum": 0, "maximum": 65535 },
            "retries": { "type": "integer", "format": "uint32", "minimum": 0, "default": 3 }
        },
        "required": ["user", "fruit"]
    })
}

#[test]
fn vars_validation_report_matches_vision() {
    let inv = load(
        r#"
vars { fruit "banana" }
group "myservers" {
    host "a" addr="1" { vars { user "cadu" } }
    host "b" addr="2"
    host "c" addr="3"
}
"#,
    );
    let schema = thing_schema();
    let results: HostResults = inv
        .select("myservers")
        .unwrap()
        .iter()
        .map(|h| {
            (
                h.name.clone(),
                validate(&inv.resolve(&h.name).unwrap().vars, &schema),
            )
        })
        .collect();
    let report = format_vars_report(
        "myservers",
        true,
        "playbooks/thing.rs",
        "hosts.kdl",
        &results,
    )
    .unwrap();
    let expected = "\
error: 2 of 3 hosts in group `myservers` do not satisfy the vars of playbooks/thing.rs

  host `a`   ok
  host `b`   missing required var `user`
  host `c`   missing required var `user`

  Vars are resolved from hosts.kdl as: vars -> group vars -> host vars.
  Add `user` to hosts b and c, or to group \"myservers\" vars if it is shared.
";
    assert_eq!(report, expected, "got:\n{report}");
}

#[test]
fn vars_validation_kinds() {
    let schema = thing_schema();
    let mut bag = VarBag::new();
    bag.insert("user".into(), s("cadu"));
    bag.insert("port".into(), s("22"));
    bag.insert("retries".into(), Scalar::Int(-1));
    bag.insert("frut".into(), s("apple"));
    bag.insert("zzz".into(), Scalar::Bool(true));
    let errs = validate(&bag, &schema);
    let lines: Vec<(&str, Severity, &str)> = errs
        .iter()
        .map(|e| (e.var.as_str(), e.severity, e.message.as_str()))
        .collect();
    assert_eq!(
        lines,
        [
            ("fruit", Severity::Error, "missing required var `fruit`"),
            (
                "port",
                Severity::Error,
                "var `port` must be integer (uint16) or null, got \"22\""
            ),
            (
                "retries",
                Severity::Error,
                "var `retries` must be integer (uint32), got -1"
            ),
            (
                "frut",
                Severity::Warning,
                "var `frut` is not declared by this playbook; did you mean `fruit`?"
            ),
            (
                "zzz",
                Severity::Warning,
                "var `zzz` is not declared by this playbook (ignored)"
            ),
        ]
    );
    // Good values, port omitted (Option), retries defaulted: clean.
    let mut bag = VarBag::new();
    bag.insert("user".into(), s("cadu"));
    bag.insert("fruit".into(), s("banana"));
    assert!(validate(&bag, &schema).is_empty());
    // A playbook without vars validates nothing.
    assert!(validate(&bag, &serde_json::Value::Null).is_empty());
}

#[test]
fn vars_validation_lists_and_enums() {
    let schema = json!({
        "type": "object",
        "properties": {
            "ports": { "type": "array", "items": { "type": "integer", "format": "uint16", "minimum": 0 } },
            "env": { "$ref": "#/$defs/Env" },
            "tls": { "type": "boolean" }
        },
        "required": ["ports", "env"],
        "$defs": { "Env": { "type": "string", "enum": ["staging", "production"] } }
    });
    let mut bag = VarBag::new();
    bag.insert("ports".into(), Scalar::List(vec![Scalar::Int(22), s("80")]));
    bag.insert("env".into(), s("prod"));
    bag.insert("tls".into(), Scalar::Int(1));
    let msgs: Vec<String> = validate(&bag, &schema)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        msgs,
        [
            "var `env` must be one of \"staging\" | \"production\", got \"prod\"",
            "var `ports` must be list of integer (uint16), got [22,\"80\"]",
            "var `tls` must be boolean, got 1",
        ]
    );
    bag.insert("ports".into(), Scalar::List(vec![Scalar::Int(22)]));
    bag.insert("env".into(), s("production"));
    bag.insert("tls".into(), Scalar::Bool(true));
    assert!(validate(&bag, &schema).is_empty());
}

#[test]
fn vars_report_single_host_and_multiple_errors() {
    let schema = thing_schema();
    let mut bag = VarBag::new();
    bag.insert("port".into(), s("x"));
    let results: HostResults = vec![("solo".into(), validate(&bag, &schema))];
    let report =
        format_vars_report("solo", false, "playbooks/thing.rs", "hosts.kdl", &results).unwrap();
    let expected = "\
error: host `solo` does not satisfy the vars of playbooks/thing.rs

  host `solo`   missing required var `user`
                missing required var `fruit`
                var `port` must be integer (uint16) or null, got \"x\"

  Vars are resolved from hosts.kdl as: vars -> group vars -> host vars.
  Add `user` to host solo.
  Add `fruit` to host solo.
";
    assert_eq!(report, expected, "got:\n{report}");
    // Warnings alone are not a failure.
    let mut bag = VarBag::new();
    bag.insert("user".into(), s("u"));
    bag.insert("fruit".into(), s("f"));
    bag.insert("extra".into(), s("e"));
    let results: HostResults = vec![("solo".into(), validate(&bag, &schema))];
    assert!(format_vars_report("solo", false, "p.rs", "hosts.kdl", &results).is_none());
}

// --- workspace example ---------------------------------------------------------

#[test]
fn example_workspace_inventory_loads() {
    let inv = Inventory::load(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/workspace/hosts.kdl"
    ))
    .unwrap();
    assert_eq!(inv.select("lab").unwrap().len(), 2);
    let arm = inv.resolve("arm").unwrap();
    assert_eq!(arm.params.addr.as_deref(), Some("cadu-cogram-vm-arm"));
    assert_eq!(arm.params.ssh_user, "cadu");
    let local = inv.resolve("local").unwrap();
    assert_eq!(local.params.connection, Connection::Local);
    assert_eq!(local.groups, ["lab"]);
    assert!(inv.resolve("web2").is_ok());
}

#[test]
fn review_fixes_lists_floats_bigints_groups_and_conflicts() {
    // `key` with no values is an empty list; `key 1` stays a scalar (coerced
    // to a one-element list where the playbook's schema wants a list).
    let inv = load(r#"host "a" connection="local" { vars { empty; one 1; many 1 2 } }"#);
    let r = inv.resolve("a").unwrap();
    assert_eq!(r.vars["empty"], Scalar::List(vec![]));
    assert_eq!(r.vars["one"], Scalar::Int(1));
    let schema = serde_json::json!({"type":"object","properties":{"one":{"type":"array","items":{"type":"integer"}}}});
    assert!(
        validate(&r.vars, &schema).iter().all(|e| !e.is_error()),
        "{:?}",
        validate(&r.vars, &schema)
    );

    // Non-finite floats are refused; u64 above i64::MAX is accepted.
    one_error(
        r#"host "a" connection="local" { vars { t #inf } }"#,
        1,
        40,
        "is not a finite number",
    );
    let inv = load(r#"host "a" connection="local" { vars { big 18446744073709551615 } }"#);
    assert_eq!(
        inv.resolve("a").unwrap().vars["big"],
        Scalar::UInt(u64::MAX)
    );

    // `resolve` on a group says so instead of denying it exists.
    let inv = load(r#"group "web" { host "w1" connection="local"; host "w2" connection="local" }"#);
    let e = inv.resolve("web").unwrap_err().to_string();
    assert!(
        e.contains("`web` is a group") && e.contains("w1, w2"),
        "{e}"
    );

    // Conflicts are reported even when an unrelated error exists.
    let errs = errors(
        r#"group "a" { vars { k 1 }; host "x" addr="1.1.1.1" port="22" }
group "b" { vars { k 2 }; members "x" }"#,
    );
    let msgs: Vec<String> = errs.iter().map(|e| e.to_string()).collect();
    assert!(msgs.iter().any(|m| m.contains("port")), "{msgs:?}");
    assert!(
        msgs.iter()
            .any(|m| m.contains("var `k` is defined by both")),
        "{msgs:?}"
    );

    // A conflicted `connection` is reported as the conflict, not as a bogus
    // missing-addr error.
    let errs = errors(
        r#"group "b" connection="ssh" { members "x" }
group "a" connection="local" { host "x" }"#,
    );
    let msgs: Vec<String> = errs.iter().map(|e| e.to_string()).collect();
    assert!(
        msgs.iter()
            .any(|m| m.contains("parameter `connection` is defined by both")),
        "{msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("has no `addr`")),
        "{msgs:?}"
    );

    // A duplicate group's children still get checked.
    let errs = errors(
        r#"group "g" { host "a" connection="local" }
group "g" { host "b" connection="local" bogus=1 }"#,
    );
    let msgs: Vec<String> = errs.iter().map(|e| e.to_string()).collect();
    assert!(msgs.iter().any(|m| m.contains("defined twice")), "{msgs:?}");
    assert!(msgs.iter().any(|m| m.contains("bogus")), "{msgs:?}");

    // Both overridden siblings are recorded.
    let inv = load(
        r#"group "a" { vars { x 1 }; members "h" }
group "b" { vars { x 2 }; members "h" }
host "h" connection="local" { vars { x 3 } }"#,
    );
    let r = inv.resolve("h").unwrap();
    assert_eq!(
        r.sources.overridden_vars.len(),
        2,
        "{:?}",
        r.sources.overridden_vars
    );
}
