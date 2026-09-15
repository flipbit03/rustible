//! The `inventory show <host>` layout: resolved parameters and vars, each
//! with its source, then what was overridden (vision 10.2.2).

use std::fmt::Write;

use super::resolve::{Overridden, Resolved};

/// Render one resolved host.
pub fn render(r: &Resolved) -> String {
    let mut out = String::new();
    if r.groups.is_empty() {
        let _ = writeln!(out, "host {}", r.host);
    } else {
        let _ = writeln!(out, "host {}  (groups: {})", r.host, r.groups.join(", "));
    }

    let params = r.params.rendered();
    let vars: Vec<(&str, String)> = r
        .vars
        .iter()
        .map(|(k, v)| (k.as_str(), v.to_string()))
        .collect();
    let key_w = params
        .iter()
        .map(|(k, _)| k.len())
        .chain(vars.iter().map(|(k, _)| k.len()))
        .max()
        .unwrap_or(0);
    let val_w = params
        .iter()
        .map(|(_, v)| v.chars().count())
        .chain(vars.iter().map(|(_, v)| v.chars().count()))
        .max()
        .unwrap_or(0);

    let _ = writeln!(out, "\nparameters");
    for (k, v) in &params {
        let src = r
            .sources
            .params
            .get(k)
            .map(ToString::to_string)
            .unwrap_or_default();
        let _ = writeln!(out, "  {k:<key_w$}  {v:<val_w$}  {src}");
    }

    let _ = writeln!(out, "\nvars");
    if vars.is_empty() {
        let _ = writeln!(out, "  (none)");
    }
    for (k, v) in &vars {
        let src = r
            .sources
            .vars
            .get(*k)
            .map(ToString::to_string)
            .unwrap_or_default();
        let _ = writeln!(out, "  {k:<key_w$}  {v:<val_w$}  {src}");
    }

    let overridden: Vec<&Overridden> = r
        .sources
        .overridden_params
        .iter()
        .chain(r.sources.overridden_vars.iter())
        .collect();
    if !overridden.is_empty() {
        let _ = writeln!(out, "\noverridden");
        for o in overridden {
            let _ = writeln!(
                out,
                "  {:<key_w$}  {:<val_w$}  {}",
                o.key, o.value, o.source
            );
        }
    }
    out
}
