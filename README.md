# skiller

Declarative Agent Skill catalog, configuration, migration, and installation over pinned Vercel Skills.

## Commands

```bash
skiller add-catalog <alias> <source>
skiller catalog add-skill --root <catalog> --source <skill> --scope <scope> --global|--project
skiller config [-g] [--print] [--set catalog/name=enable|manual|off]
skiller config [-g] --agent <agent> [--agent <agent>...]
skiller install [-g]
skiller doctor [-g] [--print|--repair [--yes]]
skiller migrate
skiller migrate --init migration.json
skiller migrate --plan migration.json --check
skiller migrate --plan migration.json --apply [--yes]
```

`config` edits desired selection. `install` reconciles it. `doctor` diagnoses and explicitly repairs owned state. `migrate` guides legacy skills into a writable catalog, creates configuration, and optionally installs and cleans exact approved legacy names.

## Configuration

Global configuration is `~/.config/skiller/config.json`. Project configuration is `<project>/skiller.config.json`.

```json
{
  "version": 1,
  "catalogs": {
    "pyg": { "source": "vlwkaos/skills" }
  },
  "agents": ["universal", "claude-code", "pi"],
  "skills": {
    "pyg/develop": "enable",
    "pyg/note": "manual"
  }
}
```

At least one Vercel agent is required. Agent names pass to `skills@1.5.23`, which performs final validation and placement. Project configuration omits `catalogs` but has the same `agents` and `skills` fields.

Enabled skills allow agent and human invocation. Manual skills are human-only unless required. Unselected dependencies are agent-only. Dependency reachability never changes configured selection.

## Catalog

```json
{
  "version": 1,
  "scopes": {
    "engineering": { "label": "Engineering", "order": 10 }
  },
  "skills": {
    "develop": { "scope": "engineering", "global": true }
  },
  "renames": {
    "old-develop": "develop"
  }
}
```

Source and installed names stay clean, such as `develop`. Skiller adds scope to projected descriptions, such as `[engineering] Develop features safely.` Pygmalion may still expose `$engineering:develop` aliases.

Dependencies use comma-separated `metadata.skiller.requires`. Missing dependencies, cycles, invalid rename chains, eligibility mismatches, symlinks, and selected-name collisions are hard errors.

## Safety

- Catalog authoring and migration require an explicit writable checkout and portable source.
- Migration never commits or pushes.
- Legacy cleanup is disabled by default and runs only after verified installation.
- Doctor is read-only unless `--repair` is supplied.
- Noninteractive mutation requires `--yes`.
- Skiller removes only prior ownership or exact validated migration/recovery names.
- Installed state is compact schema 2 under the XDG state directory.
- Interrupted installation retains an owned transaction journal for Doctor recovery.

The guided migration procedure is also available as `skills/skiller-migrate/SKILL.md` in this repository.
