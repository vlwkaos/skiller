# skiller

`skiller` declaratively selects skills from registered catalogs and delegates final project or global placement to pinned [Vercel Skills](https://github.com/vercel-labs/skills).

## Commands

```bash
skiller add-catalog pyg vlwkaos/skills
skiller catalog add-skill --root <catalog> --source <skill> --scope <scope> --global|--project
skiller config [--print] [--set catalog/name=enable|manual|off] [--set-gitignore catalog/name=true|false]
skiller install [--migrate]
skiller config -g [--print] [--set catalog/name=enable|manual|off]
skiller install -g [--migrate]
```

Interactive `config` opens a scoped terminal UI: arrows navigate, Space cycles Agent + Human, Human, and Off, `i` toggles project Git-ignore state, and `s` or Enter saves. Escape or `q` cancels without writing.

`--print` emits machine-readable catalog, selection, dependency, and installed state without prompting or changing configuration/installation state; remote catalog refresh may update Skiller's cache. `--set` applies one or more validated selections without installing and preserves existing project Git-ignore state. Project-only `--set-gitignore` updates that state for selected skills. A frontend can save once and then run `skiller install`. `--migrate` adopts same-name legacy installations only after every selected source stages successfully. Unrelated skills remain untouched.

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

- `enable`: agent and human invocation.
- `manual`: human invocation without initial model discovery.
- `gitignore`: omits that project skill's Vercel projections from Git.
- Omitted entries are not selected. Required dependency closure is installed automatically.

Dependency reachability is independent from configured mode. An unselected required skill is installed agent-only with Claude Code's `user-invocable: false`; a required manual skill becomes effectively Agent + Human, while an enabled skill is already fully available. Parent modes are never inherited. `config --print` reports the reconciled `installedMode` separately from `selected`. Dependency-only user hiding is portable only where the host supports it; Pygmalion hides those entries from human aliases, while other agents may still accept an exact invocation.

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

Global configuration shows only `global: true` skills. Project configuration shows only project skills. Global and project installations receive their display-scope postfix, such as `develop-engineering`. The postfix keeps semantic scope portable across native agent command surfaces.

### Catalog authoring

`catalog add-skill` copies one external skill directory into one explicit writable catalog checkout and registers its existing scope. Exactly one of `--global` or `--project` is required. The command never discovers an authoring checkout, commits, pushes, deletes a source, or infers company ownership. It rejects symlinked content, duplicate or invalid names, unknown scopes, missing dependencies, and a global skill whose dependency closure includes project-only skills.

Dependencies use a comma-separated string in `metadata.skiller.requires`. Missing targets and direct or transitive cycles fail catalog loading with the complete cycle path. Global dependency closure must also be global.

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
