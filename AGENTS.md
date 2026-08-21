release.flow: rust
verify.cli-artifact: If tests compile a new CLI surface but cargo run still exposes an older binary after a universal release build, run `cargo clean -p skiller` and rebuild before probing.
