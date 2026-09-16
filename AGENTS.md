release.flow: rust
verify.cli-artifact: If tests compile a new CLI surface but cargo run still exposes an older binary after a universal release build, run `cargo clean -p skiller` and rebuild before probing.
verify.tests: Do not run concurrent `cargo test` invocations; tests share fixed `target/test-work` fixture paths.
