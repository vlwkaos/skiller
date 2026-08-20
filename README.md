# skiller

`skiller` declaratively selects skills from registered catalogs and delegates final project or global placement to pinned [Vercel Skills](https://github.com/vercel-labs/skills).

## Commands

```bash
skiller add-catalog pyg vlwkaos/skills
skiller config [--print]
skiller install [--migrate]
skiller config -g [--print]
skiller install -g [--migrate]
```

`--print` emits machine-readable catalog, selection, dependency, and installed state without prompting or changing configuration/installation state; remote catalog refresh may update Skiller's cache. `--migrate` adopts same-name legacy installations only after every selected source stages successfully. Unrelated skills remain untouched.

## Configuration

Project selections live in `<project>/skiller.config.json`:

```json
{
  "version": 1,
  "skills": {
    "pyg/develop": "enable",
    "pyg/private-workflow": {
      "mode": "manual",
      "gitignore": true
    }
  }
}
```

Global catalog registration and selection share `~/.config/skiller/config.json`:

```json
{
  "version": 1,
  "catalogs": {
    "pyg": { "source": "vlwkaos/skills" }
  },
  "skills": {
    "pyg/develop": "enable",
    "pyg/note": "manual"
  }
}
```

The global file may be symlinked from dotfiles. Runtime ownership stays outside dotfiles under `${XDG_STATE_HOME:-~/.local/state}/skiller/installed.json`.

- `enable`: normal Agent Skills discovery.
- `manual`: adds supported explicit-invocation controls.
- `gitignore`: omits that project skill's Vercel projections from Git.
- Omitted entries are not selected. Required dependency closure is installed automatically.

## Catalog format

Catalogs use flat source names and optional organizational metadata:

```text
skills/develop/SKILL.md
skills/commit/SKILL.md
skiller.json
```

```json
{
  "version": 1,
  "scopes": {
    "engineering": { "label": "Engineering", "order": 10 }
  },
  "skills": {
    "develop": { "scope": "engineering", "global": true },
    "commit": { "scope": "engineering", "global": false }
  }
}
```

Global configuration shows only `global: true` skills. Project configuration shows only project skills. Global names remain unchanged; project names receive their display-scope postfix, such as `develop-engineering`.

Dependencies use a comma-separated string in `metadata.skiller.requires`. Missing targets and cycles fail catalog loading. Global dependency closure must also be global.

## Installation and ownership

Skiller stages untrusted catalog content through `skills@1.5.23`, rejects symlinks and invalid names, applies naming/manual transforms, then invokes Vercel Skills again as the final writer with explicit `universal`, `claude-code`, and `pi` targets. Vercel creates the canonical Agent Skills store and Claude Code/Pi projections.

Project ownership lives in `.skiller/installed.json`; global ownership lives in the XDG state path. Removal passes only previously owned names to Vercel Skills. Skiller never removes unrelated skills.

## New machine

1. Install Skiller through Homebrew or Cargo.
2. Apply dotfiles so `~/.config/skiller/config.json` points to the tracked global configuration.
3. Run `skiller install -g` on a clean machine.
4. Run `skiller install -g --migrate` when replacing legacy global skill roots.
5. In each project, run `skiller install` or `skiller install --migrate` once for legacy project layouts.

`--migrate` unlinks legacy vendor root symlinks without deleting their source tree, then lets Vercel recreate per-skill links.
