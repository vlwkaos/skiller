## [0.4.0] - 2026-08-21

### Features

- Add explicit `catalog add-skill --global|--project` authoring with bounded copy, metadata validation, dependency checks, and rollback.
- Add project-only noninteractive Git-ignore updates for configuration frontends.

### Fixed

- Preserve existing project Git-ignore state when changing a skill mode through `config --set`.

## [0.3.0] - 2026-08-20

### Changed

- Apply semantic scope postfixes to both global and project installed names.
- Derive agent-only dependency availability independently from explicit Enabled and Manual selection.
- Write Claude Code `user-invocable: false` for dependency-only skills and expose reconciled installed modes to configuration frontends.
- Report complete direct and transitive dependency-cycle paths as hard catalog errors.

## [0.2.0] - 2026-08-20

### Features

- Replace numbered interactive configuration with a scoped, keyboard-driven terminal UI.
- Add bounded noninteractive `config --set catalog/name=enable|manual|off` updates for trusted configuration frontends.

## [0.1.1] - 2026-08-20

### Fixed

- Install through explicit Vercel Skills targets for the universal canonical store, Claude Code, and Pi.

## [0.1.0] - 2026-08-20

### Features

- Add declarative project and global skill selection from registered flat catalogs.
- Add interactive configuration and machine-readable `config --print` output.
- Add project/global installation, dependency closure, manual mode, scoped project names, and per-skill Git ignore.
- Use pinned Vercel Skills for staging, canonical placement, and agent-specific links.
- Add explicit zero-loss migration for same-name legacy installations and root links.

### Security

- Preserve unrelated skills through exact ownership state and normal-install conflict checks.
- Reject symlinked catalog content, invalid names, dependency cycles, unsafe state paths, terminal control characters, and symlinked project ignore files.
- Use exclusive atomic JSON writes and disable npm lifecycle scripts.
