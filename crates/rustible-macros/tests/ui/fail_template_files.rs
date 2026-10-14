fn main() {
    // A file name passed as a string literal: the literal is the template,
    // so the argument is unused, and the error says how to render the file.
    let _ = rustible::template!("templates/app.conf.j2", listen = 1);
    // An `include_str!` path one typo away from a file that is there.
    let _ = rustible::template!(include_str!("templates/app.conf.j3"), listen = 1);
    // An `include_str!` path into a directory that is not there.
    let _ = rustible::template!(include_str!("tempaltes/app.conf.j2"), listen = 1);
    // Every name not passed is listed, and every one is suggested.
    let _ = rustible::template!("{{ a }} {{ b }}");
    // Every argument nothing uses is its own error; the code after the call
    // still type-checks against `Result<String>`.
    let s: rustible::sdk::Result<String> = rustible::template!("{{ a }}", a = 1, b = 2, c = 3);
    let _ = s;
}
