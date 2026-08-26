---
name: skiller-migrate
description: Guide reviewed legacy Agent Skills into a configured Skiller catalog without editing installed projections or ownership state.
---

# Skiller migration

Use Skiller's normal authoring, configuration, and reconciliation commands. Migration has no special binary mode.

## Workflow

1. Run `skiller doctor [-g]` and retain the non-TTY JSON report.
2. Identify one canonical legacy skill directory. Prefer a real source directory, not an agent projection symlink.
3. Resolve the target catalog alias and its validated authoring checkout with `skiller config -g`.
4. Review the source name, `SKILL.md`, dependencies, scope, and global eligibility.
5. Copy through Skiller:

   ```bash
   skiller catalog add-skill <alias> <source> <scope> [--global]
   ```

6. Inspect the catalog diff. Skiller does not commit or publish it.
7. After the catalog change is canonical, select the skill:

   ```bash
   skiller config [-g] --set <alias>/<name>=enable|manual
   skiller install [-g]
   ```

8. Run `skiller doctor [-g]` again and verify every configured agent projection.
9. Ask separately before deleting any legacy source. Never delete a locally modified project override through migration.
10. Report catalog, configuration, state, installed paths, mode, dependency closure, and remaining commit, push, cleanup, or reload steps.

## Boundaries

- Never edit installed projections, `.skiller/installed.json`, Vercel lock files, or generated links.
- Do not absorb unrelated same-name skills.
- Project is the default eligibility; `--global` is explicit.
- Source names remain unpostfixed. Scope appears in projected descriptions and Pygmalion aliases.
- A failed install preserves per-skill progress and reports unresolved blockers.
