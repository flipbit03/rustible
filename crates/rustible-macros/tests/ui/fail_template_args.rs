fn main() {
    let m = rustible::json!({});
    let _ = rustible::template!("{{ a }}", a = 1, a = 2);
    let _ = rustible::template!("{{ a }}", ..&m, ..&m);
    let _ = rustible::template!("{{ a }}", a: 1);
    let _ = rustible::template!();
}
