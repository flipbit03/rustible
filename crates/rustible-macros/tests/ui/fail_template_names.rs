fn main() {
    // `heap` is not passed.
    let _ = rustible::template!("-Xmx{{ heap }} {{ flags }}", flags = "");
    // `unused` is used nowhere.
    let _ = rustible::template!("{{ a }}", a = 1, unused = 2);
    // A spread relaxes the first check, never the second.
    let m = rustible::json!({});
    let _ = rustible::template!("{{ a }}", ..&m, unused = 2);
}
