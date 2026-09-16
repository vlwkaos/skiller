# skiller

Declarative Agent Skill catalogs and convergent project/global installation over pinned Vercel Skills.

## Commands

```bash
skiller
skiller catalog configure <alias> <source> [--ref <ref>] [--authoring-root <path>]
skiller catalog add-skill <alias> <skill> <scope> [--global]
skiller config [-g] [--set catalog/name=STATE] [--agents universal,claude-code,pi]
skiller update [-g] [--yes]
skiller install [-g]
skiller doctor [-g] [--repair [--yes]]
```

Run bare `skiller` to choose Project or Global configuration. Saving interactive configuration and applying `config --set` or `--agents` immediately reconcile the matching installation. Piped read-only `config` remains inspection-only. Explicit `install` remains available for recovery and automation.

`STATE` is `enable`, `manual`, `enable-ignored`, `manual-ignored`, or `off`. Project is the safe default for new catalog skills; `--global` is explicit. Project and global eligibility are exclusive, so globally catalogued skills never appear in project configuration. An explicit `off` may remove an existing selection whose catalog eligibility later changed.

Read-only commands choose output automatically. A TTY gets the organized interactive or human view with semantic color and status icons; `NO_COLOR` and `TERM=dumb` disable styling. A pipe, agent, or subprocess gets compact one-line JSON where supported and plain output otherwise. Interactive `config` and mutating `config --set` or `--agents` refresh catalogs; piped read-only `config` and `doctor` use synchronized cache only. Failed config refreshes show cached catalog entries as read-only with recovery guidance. `update` and `install` retain their explicit reconciliation roles. Global `update` checks the stable Skiller release without blocking skill results when the registry is unavailable, and reports a newer binary without installing it.

## Configuration

Skiller keeps policy and generated state outside the tracked worktree:

| Data | Path | Scope |
|---|---|---|
| Global configuration | `~/.config/skiller/config.json` | Device |
| Project configuration | `$(git rev-parse --git-common-dir)/skiller/config.json` | Repository clone, shared by linked worktrees |
| Project installation state | `$(git rev-parse --git-dir)/skiller/installed.json` | Current worktree |
| Installed projections | `<worktree>/.agents/skills/` and agent-native equivalents | Current worktree |

Project mode requires Git. Outside a repository, bare `skiller` opens Global configuration directly.

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

Interactive configuration restores the pre-Skiller selector geometry. Wide terminals keep scope navigation, compact one-line skill/configuration rows, and selected description, package-manager-style install plan, reverse dependents, installed state, and sync details visible in three columns. Enter moves focus from scopes to skills; Escape moves back. A persistent action bar exposes Save (`S`) and direct Cancel (`Q`). Narrow terminals retain the same scope-first navigation, stack only the selected skill's labeled details, and preserve both global actions before contextual hints. Semantic scope, dependency, mode, recommendation, warning, error, focus, and action colors remain stable and respect `NO_COLOR` and `TERM=dumb`. Redraws queue one synchronized frame and replace rows in place instead of blanking the alternate screen.

Enabled skills allow agent and human invocation. Manual skills are human-only unless required. Unselected dependencies are agent-only. Dependency reachability never changes configured selection.

Every install prints an aligned plan before any projection mutation: one row per resolved skill, tree branches for dependency edges, and a colored state column showing `Agent + Human`, `Human only`, or `dependency`. Skill names are shown without their catalog prefix. A dependency that is also configured directly is annotated in the state column rather than repeated as a separate identity.

## Project reconciliation

Catalog-managed installations are read-only, disposable projections. The catalog is authoritative.

| Status | Meaning | Install behavior |
|---|---|---|
| `synced` | Projection matches the installed catalog tree | No content change |
| `missing` | An owned projection is absent | Reinstall it |
| `drift` | Projection differs from authoritative content or lacks an old baseline | Overwrite it |
| `incoming` | Catalog identity, mode, or metadata changed | Install the new projection |

To change a managed skill, edit its catalog authoring source, publish it, then run `skiller config` or `skiller install`. Skiller does not preserve project overrides or merge projection edits. A divergent unowned same-name skill remains protected; byte-identical unowned projections are adopted safely.

A divergent unowned name blocks noninteractive installation and is reported as `[unowned-conflict]`. When installation runs in a terminal, Skiller lists each conflict and asks per skill: `y` replaces that name with the catalog version, `Y` replaces every remaining conflict, and any other answer keeps the existing copy. An unanswered prompt, EOF, and every automated or piped run keep the existing copy, so replacement only happens through explicit interactive approval.

On the first mutating Project command, Skiller imports legacy `<project>/skiller.config.json` and `<project>/.skiller/` data. It removes untracked legacy files after successful migration. A tracked or divergent legacy config remains for explicit review but is ignored once the Git-private config exists.

## Project Skills lock

Skiller catalog skills are owned only by the current worktree's Git-private `skiller/installed.json`; native project skills added directly through Vercel Skills are owned only by `skills-lock.json`. Before and after Vercel placement, reconciliation removes only state-proven Skiller entries whose source resolves to the current Git-private `skiller/prepared-current` and preserves every native entry. Skills 1.5.23 add, install, sync, and named removal do not prune unrelated projections.

## Catalog

`skiller.json` declares semantic scopes, eligibility, and renames. Skill frontmatter declares comma-separated dependencies through `metadata.skiller.requires` and optional deterministic project recommendations. Missing dependencies, cycles, invalid rename chains, eligibility mismatches, symlinks, and installed-name collisions are hard errors.

### Catalog recommendations

A project-only skill may declare `metadata.skiller.recommend.files: "Cargo.toml"` and `metadata.skiller.recommend.keywords: "release,Homebrew"` in `SKILL.md`. Values are literal: files match exact root names, keywords match case-insensitively in root names plus the first 20 KB of `package.json`, `pyproject.toml`, `Cargo.toml`, `README.md`, and `AGENTS.md`. Alternatives within one field are OR; populated fields combine with AND. Config JSON returns exact `recommendedBy` reasons, and the TUI marks matching scopes and skills without selecting or installing them.

`catalog add-skill` resolves the alias's validated authoring checkout. It no longer accepts an arbitrary catalog root. Legacy migration uses the bundled `skiller-migrate` guidance with normal catalog, config, and install commands.

## Doctor and recovery

`skiller doctor [-g]` is read-only. Its human report groups ownership conflicts by skill and maps catalog freshness, projection drift, project Skills lock entries, and owned-state problems to explicit actions. `skiller install [-g]` adopts byte-identical unowned skills automatically; divergent content stays unchanged and Doctor shows the exact command to keep the existing owner. Non-TTY Doctor JSON remains deterministic and does not include presentation-only recommendations. Repair still requires `--repair` and confirmation unless `--yes` is supplied.

## Safety

- Skiller removes only verified ownership or exact approved recovery names.
- Unowned projections are adopted only when every discovered copy is byte-identical.
- Installed state is compact schema 4 and records catalog identity plus the authoritative content baseline.
- Install resumes only validated interrupted transactions and retains independent per-skill progress.
- Catalog-owned project and global projections are overwritten from authoritative content.
- Tracked legacy configuration is never deleted automatically; migrated legacy paths stop competing as readers.
- Vercel listing is bounded to 15 seconds and placement to 60 seconds.
- Git SSH acquisition is bounded and repeated unreachable sources are suppressed briefly.
- Permission, process, network, timeout, placement, and state failures are classified separately.
- Mutation remains explicit: `update --yes`, `doctor --repair`, and `doctor --repair --yes` for reviewed automation.

Skiller pins `skills@1.5.23` for final validation and placement.
