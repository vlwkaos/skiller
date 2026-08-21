## [0.8.1] - 2026-08-21

### Changed

- Make `config --print` read synchronized local catalog state without network refresh.

## [0.8.0] - 2026-08-21

### Features

- Continue noninteractive reconciliation when a registered catalog source is unreachable, while preserving its declarations, owned installations, and projections.
- Add stable camelCase catalog availability status to config, update, and Doctor JSON reports, including read-only stale cache rows.

### Fixed

- Validate refreshed catalog clones before replacing the last known-good cache and keep authoring-checkout failures diagnostic-only.

## [0.7.3] - 2026-08-21

### Security

- Replace a repository-specific test fixture with a neutral catalog alias.

## [0.7.2] - 2026-08-21

### Fixed

- Use a stable transaction-owned prepared source so project `skills-lock.json` does not churn on every install.

## [0.7.1] - 2026-08-21

### Fixed

- Ignore empty runtime-created directories when comparing authoring and canonical skill content.

## [0.7.0] - 2026-08-21

### Features

- Add canonical catalog refs, explicit authoring checkouts, and unified `catalog configure` registration.
- Add deterministic projected-skill digests and backward-compatible compact installed-state schema 3.
- Add read-only JSON update checks that distinguish published catalog changes from unpublished authoring drafts.
- Add confirmation-gated `skiller update`; installation always uses canonical catalog content.

### Security

- Make background update checks noninteractive and bound SSH connection attempts.
- Keep authoring checkouts detection-only so unpublished skill behavior cannot install accidentally.

## [0.6.1] - 2026-08-21

### Fixed

- Replace complete folded YAML description blocks when adding projected scope labels.
- Retry only skills omitted by a successful Vercel batch placement, then require complete selected-agent verification.

## [0.6.0] - 2026-08-21

### Features

- Add guided and plan-driven legacy migration into explicit catalogs and configuration.
- Persist configurable nonempty Vercel agent targets and verify them through Vercel JSON listing.
- Add a bundled `skiller-migrate` Agent Skill for human and agent workflows.

### Changed

- Keep installed skill names unpostfixed and add semantic scope to projected descriptions.
- Remove `install --migrate`; full migration now uses `skiller migrate`.
- Replace the README with a concise command, configuration, catalog, and safety reference.

## [0.5.0] - 2026-08-21

### Features

- Add read-only project and global Doctor diagnostics with compact JSON output and explicit confirmation-gated repair.
- Add validated catalog rename declarations and deterministic configuration-key migration.
- Add compact installed-state schema 2 with transparent reads of schema 1.
- Add owned transaction journals for interrupted install recovery.

### Security

- Block repair on invalid configuration, invalid journals, or unowned projection conflicts.
- Limit recovery cleanup and replacement to prior ownership or names recorded after conflict validation.

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
