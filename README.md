# skiller

`skiller` declaratively selects skills from registered catalogs and delegates final project or global placement to pinned [Vercel Skills](https://github.com/vercel-labs/skills).

## Commands

```bash
skiller add-catalog pyg vlwkaos/skills
skiller catalog add-skill --root <catalog> --source <skill> --scope <scope> --global|--project
skiller config [--print] [--set catalog/name=enable|manual|off] [--set-gitignore catalog/name=true|false]
skiller install [--migrate]
skiller doctor [--print|--repair [--yes]]
skiller config -g [--print] [--set catalog/name=enable|manual|off]
skiller install -g [--migrate]
skiller doctor -g [--print|--repair [--yes]]
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
  },
  "renames": {
    "old-develop": "develop"
  }
}
```

Global configuration shows only `global: true` skills. Project configuration shows only project skills. Global and project installations receive their display-scope postfix, such as `develop-engineering`. The postfix keeps semantic scope portable across native agent command surfaces.

### Catalog authoring

`catalog add-skill` copies one external skill directory into one explicit writable catalog checkout and registers its existing scope. Exactly one of `--global` or `--project` is required. The command never discovers an authoring checkout, commits, pushes, deletes a source, or infers company ownership. It rejects symlinked content, duplicate or invalid names, unknown scopes, missing dependencies, and a global skill whose dependency closure includes project-only skills.

Dependencies use a comma-separated string in `metadata.skiller.requires`. Missing targets and direct or transitive cycles fail catalog loading with the complete cycle path. Global dependency closure must also be global.

Source-name changes must be declared in `renames`. Every rename chain must be acyclic and end at a current skill. Skiller never infers identity from descriptions or files. Doctor can then migrate configuration keys while preserving modes and project Git-ignore state.

## Installation and ownership

Skiller stages untrusted catalog content through `skills@1.5.23`, rejects symlinks and invalid names, applies naming/manual transforms, then invokes Vercel Skills again as the final writer with explicit `universal`, `claude-code`, and `pi` targets. Vercel creates the canonical Agent Skills store and Claude Code/Pi projections.

Project ownership lives in `.skiller/installed.json`; global ownership lives in the XDG state path. Version 2 stores only each stable catalog key's installed name, effective mode (`e`, `m`, or `d`), and Git-ignore bit. Skiller reads verbose version 1 state and writes compact version 2 state after successful reconciliation. Removal passes only previously owned names to Vercel Skills. Skiller never removes unrelated skills.

Each install writes a compact transaction journal before final placement, advances it through prepared, installed, verified, and cleaned phases, and removes it only after ownership and Git-ignore state commit. A later normal install refuses an unfinished journal.

## Doctor and recovery

`doctor` is read-only by default, although refreshing a remote catalog may update Skiller's cache. It diagnoses declared renames, malformed or stale configuration, dependency and eligibility errors, obsolete ownership, missing universal/Claude Code/Pi projections, owned staging residue, unowned conflicts, and interrupted transactions. `--print` emits compact JSON.

`doctor --repair` previews repairable findings and prompts before changing state. Use `--yes` only for an already-reviewed noninteractive repair. Repair applies declared renames, preserves modes and Git-ignore state, removes only proven owned residue, and runs normal verified reconciliation. Any unowned conflict or invalid configuration blocks all repair.

## Apply on another machine

### Existing machine upgrading from Skiller 0.4

```bash
brew update
brew upgrade vlwkaos/tap/skiller
skiller --version
skiller doctor -g --print
skiller doctor -g --repair
skiller doctor -g
```

The expected version is `skiller 0.5.0`. Review the JSON report before repair. The repair rewrites legacy ownership state compactly and restores missing projections. For each configured project:

```bash
cd <project>
skiller doctor --print
skiller doctor --repair
skiller doctor
```

### Clean machine

1. Apply dotfiles first so `~/.config/skiller/config.json` resolves to the tracked global configuration.
2. Install Skiller:
   ```bash
   brew install vlwkaos/tap/skiller
   skiller --version
   ```
3. Install the declared global selection:
   ```bash
   skiller install -g
   skiller doctor -g
   ```
4. In each project containing `skiller.config.json`:
   ```bash
   cd <project>
   skiller install
   skiller doctor
   ```
5. Use `install --migrate` instead of normal install only when replacing same-name legacy copies or legacy vendor-root links.

For unattended provisioning, first inspect `doctor -g --print`, then use `doctor -g --repair --yes`. Do not use `--yes` without retaining the diagnostic output. `--migrate` unlinks legacy vendor root symlinks without deleting their source tree.
