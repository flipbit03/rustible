fn main() {
    let _ = rustible::template!("line one\n{% if x %}never closed", x = true);
}
