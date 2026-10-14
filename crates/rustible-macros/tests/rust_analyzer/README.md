Playbooks for `tests/rust_analyzer.rs`, copied into a copy of
`examples/workspace` as `playbooks/ra/`. Every error both the build and
rust-analyzer must report sits on a line marked `//~ <part of the message>`;
`//~ build: <part>` marks one only the build reports, where the editor cannot
tell which file the call is in and must stay silent. Any other error, in
these playbooks or the example's own, fails the test.
