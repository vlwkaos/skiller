## [0.1.1] - 2026-03-30

### Features

- Add built-in `hermes` target path for `~/.hermes/skills`
- Show top-level help when `skiller` runs without a subcommand
- Expand `target add --help` with built-in target types and path guidance

### Docs

- Add a `README.md` with a simple setup and usage example

### Bug Fixes

- Canonicalize source paths before linking to avoid creating relative symlinks ([`f88a1ba`](https://github.com/vlwkaos/skiller/commit/f88a1ba))

## [0.1.0] - 2026-03-11

### Features

- Initial release — symlink manager for AI tool skill bundles ([`c6be5fa`](https://github.com/vlwkaos/skiller/commit/c6be5fa))
- Five commands: `source`, `target`, `link`, `unlink`, `status`
- Built-in paths for Claude, Codex, OpenCode, OpenClaw
- Conflict resolution: overwrite, migrate, or skip
- Source validation requires at least one `*/SKILL.md`
