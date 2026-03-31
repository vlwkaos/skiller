## [0.2.0] - 2026-03-31

### Features

- Add granular link mode — per-skill symlinks into a real target directory, allowing target-specific skills to coexist with shared ones ([`5e586fc`](https://github.com/vlwkaos/skiller/commit/5e586fc))
- Add `hermes` built-in target path (`~/.hermes/skills`) ([`e49d5af`](https://github.com/vlwkaos/skiller/commit/e49d5af))
- `skiller link [type]` and `skiller unlink [type]` now accept an optional target type to operate on a single target

### Bug Fixes

- Canonicalize source path on `skiller source` to prevent relative symlinks ([`f88a1ba`](https://github.com/vlwkaos/skiller/commit/f88a1ba))

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
