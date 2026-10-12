use rustible::prelude::*;

#[derive(rustible::serde::Serialize)]
#[serde(crate = "rustible::serde")]
struct Item {
    name: String,
}

fn render(motd: &str) -> Result<String> {
    // A literal, with an include resolved relative to this file.
    let a = template!("{{ x }}{% include \"templates/deep/b/c/leaf.j2\" %}", x = 1, listen = "l")?;
    // A file whose includes resolve relative to it, one of them absent.
    let b = template!(include_str!("templates/app.conf.j2"), listen = "0.0.0.0:80")?;
    // A dynamic template: no checks until it renders.
    let c = template!(motd, anything = 1)?;
    // A spread relaxes "used but not passed".
    let vars = rustible::json!({ "listen": "x" });
    let d = template!(include_str!("templates/app.conf.j2"), ..&vars)?;
    // Keywords are names like any other; a trailing comma is fine.
    let e = template!("{{ type }}{{ fn }}", type = 1, r#fn = 2,)?;
    let items = vec![Item { name: "n".into() }];
    let f = template!("{% for i in items %}{{ i.name }}{% endfor %}", items = &items)?;
    Ok([a, b, c, d, e, f].concat())
}

fn main() {
    let _ = render("{{ x }}");
}
