fn main() {
    // A target the macro cannot know at compile time.
    let _ = rustible::template!("{% include name %}", name = "x.j2");
    // A target that is not there.
    let _ = rustible::template!(include_str!("templates/missing_include.j2"));
    // A template that is its own ancestor.
    let _ = rustible::template!(include_str!("templates/self_extends.j2"));
    // An `include_str!` path that is not there.
    let _ = rustible::template!(include_str!("templates/not_there.j2"));
    // An `include_str!` the macro cannot read itself.
    let _ = rustible::template!(include_str!(concat!("templates/", "app.conf.j2")), listen = 1);
}
