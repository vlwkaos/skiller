# skiller

Declarative Agent Skill catalogs and convergent project/global installation over pinned Vercel Skills.

## Commands

```bash
skiller catalog configure <alias> <source> [--ref <ref>] [--authoring-root <path>]
skiller catalog add-skill <alias> <skill> <scope> [--global]
skiller config [-g] [--set catalog/name=STATE] [--agents universal,claude-code,pi]
skiller update [-g] [--yes]
skiller install [-g]
skiller doctor [-g] [--repair [--yes]]
```

`STATE` is `enable`, `manual`, `enable-ignored`, `manual-ignored`, or `off`. Project is the safe default for new catalog skills; `--global` is explicit.

Read-only commands choose output automatically. A TTY gets the organized interactive or human view with semantic color and status icons; `NO_COLOR` and `TERM=dumb` disable styling. A pipe, agent, or subprocess gets compact one-line JSON where supported and plain output otherwise. `config` and `doctor` use synchronized cache only; `update` and `install` own remote refresh. Global `update` checks the stable Skiller release without blocking skill results when the registry is unavailable, and reports a newer binary without installing it.

## Configuration

Global configuration is `~/.config/skiller/config.json`. Project configuration is `<project>/skiller.config.json`.

```json
{
  "version": 1,
  "catalogs": {
    "pyg": {
      "source": "git@github.com:owner/skills.git",
      "ref": "main",
      "authoring_root": "/explicit/local/checkout"
    }
  },
  "agents": ["universal", "claude-code", "pi"],
  "skills": {
    "pyg/develop": "enable",
    "pyg/note": "manual"
  }
}
```

Canonical `source` and optional `ref` own consumer content. `authoring_root` is an optional writable checkout used for guidance and unpublished-draft checks. Installation always uses canonical content.

Interactive configuration restores the pre-Skiller selector geometry. Wide terminals keep scope navigation, compact one-line skill/configuration rows, and selected description, required-by, installed, and sync details visible in three columns. Enter moves focus from scopes to skills; Escape moves back. Narrow terminals retain the same scope-first navigation and stack only the selected skill's labeled details. Semantic scope, mode, warning, error, focus, and hint colors remain stable and respect `NO_COLOR` and `TERM=dumb`. Redraws queue one synchronized frame and replace rows in place instead of blanking the alternate screen.

Enabled skills allow agent and human invocation. Manual skills are human-only unless required. Unselected dependencies are agent-only. Dependency reachability never changes configured selection.

## Project reconciliation

Global projections are canonical. Project projections are writable working content tracked against the exact tree Skiller last installed.

| Status | Meaning | Install behavior |
|---|---|---|
| `synced` | Project matches its baseline | Apply incoming catalog updates |
| `keep-local` | Only project content changed | Preserve the complete project skill |
| `conflict` | Project and catalog changed | Preserve, block the skill and dependents, return nonzero |
| `orphaned-local` | Upstream removal/rename would discard project work | Preserve and require manual review |
| `unknown` | Older state has no exact baseline | Preserve conservatively until reconciled |

Config JSON and the TUI show the sync state and validated authoring skill path. Skiller never promotes, merges, commits, or publishes project edits. Matching current canonical content resolves divergence automatically.

## Catalog

`skiller.json` declares semantic scopes, eligibility, and renames. Dependencies use comma-separated `metadata.skiller.requires`. Missing dependencies, cycles, invalid rename chains, eligibility mismatches, symlinks, and installed-name collisions are hard errors.

`catalog add-skill` resolves the alias's validated authoring checkout. It no longer accepts an arbitrary catalog root. Legacy migration uses the bundled `skiller-migrate` guidance with normal catalog, config, and install commands.

## Doctor and recovery

`skiller doctor [-g]` is read-only. Its human report maps diagnosed catalog freshness, projection drift, and owned-state problems to explicit `update`, `install`, or `doctor --repair` suggestions without prompting or mutating. Non-TTY Doctor JSON remains stable and does not include presentation-only recommendations. Repair still requires `--repair` and confirmation unless `--yes` is supplied.

## Safety

- Skiller removes only verified ownership or exact approved recovery names.
- Unowned projections are adopted only when every discovered copy is byte-identical.
- Installed state is compact schema 4 and records catalog identity plus exact content baseline.
- Install resumes only validated interrupted transactions and retains independent per-skill progress.
- Modified project skills are never removed, renamed, or overwritten automatically.
- Global installed directories remain read-only projections; project projections may carry tracked overrides.
- Vercel listing is bounded to 15 seconds and placement to 60 seconds.
- Git SSH acquisition is bounded and repeated unreachable sources are suppressed briefly.
- Permission, process, network, timeout, placement, and state failures are classified separately.
- Mutation remains explicit: `update --yes`, `doctor --repair`, and `doctor --repair --yes` for reviewed automation.

Skiller pins `skills@1.5.23` for final validation and placement.
