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
