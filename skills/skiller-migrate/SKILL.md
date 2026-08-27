---
name: skiller-migrate
description: Guide reviewed legacy Agent Skills either into a configured Skiller catalog or into native project-only Vercel Skills without editing installed projections or ownership state by hand.
---

# Skiller migration

Use the catalog route for reusable Skiller ownership. Use the native route for a skill that belongs only to one project and should remain represented in that project's `skills-lock.json`. Migration has no special Skiller binary mode.

## 1. Inspect

1. Resolve the project root and run `skiller doctor`; retain the non-TTY JSON report when automating.
2. Identify one real canonical source directory, not a projection symlink.
3. Require exactly one canonical `SKILL.md` with valid `name` and `description` frontmatter. Merge or remove conflicting case variants such as `skill.md` before installation.
4. Review the complete skill, its relative resources, agent-specific features, dependencies, and requested tools. Never migrate malformed or ambiguous content as-is.
5. Check the intended name against `.agents/skills/`, `.claude/skills/`, `.pi/skills/`, `skiller config`, and `.skiller/installed.json`. Stop on any distinct same-name owner.
6. Ask the user to choose catalog-managed or native project-only ownership.

## 2A. Catalog-managed

1. Resolve the catalog alias and validated authoring checkout with `skiller config -g`.
2. Review scope and global eligibility. Project is the default; `--global` is explicit and globally eligible skills do not appear in project configuration.
3. Copy through Skiller:

   ```bash
   skiller catalog add-skill <alias> <source> <scope> [--global]
   ```

4. Inspect, evaluate, commit, and publish the catalog through its normal workflow.
5. After the change is canonical, select and reconcile it:

   ```bash
   skiller config [-g] --set <alias>/<name>=enable|manual
   skiller install [-g]
   ```

## 2B. Native project-only

1. Run from the target project root. Review the pinned Vercel Skills package before changing its version.
2. Install the selected local or remote source without `--global` and without `--copy`; default symlink placement keeps one canonical project body:

   ```bash
   npx --yes skills@1.5.23 add <source> \
     --skill <name> \
     --agent universal --agent claude-code --agent pi \
     --yes
   ```

3. Verify `.agents/skills/<name>/SKILL.md`, the Claude Code and Pi projections, and the native `skills-lock.json` entry. Its source must describe the native source, never `.skiller/prepared-*`.
4. Run `skiller doctor`. Skiller may share the projection directories, but it must neither claim the native entry in `.skiller/installed.json` nor report a same-name conflict.
5. Run `skiller install` when the project also has catalog skills, then confirm the native lock entry and projections remain unchanged.

## 3. Finish

Ask separately before deleting the legacy source. Never delete a locally modified project override through migration. Report the chosen owner, source, lock/state record, installed paths, mode, dependency closure, and remaining commit, cleanup, or reload steps.

## Boundaries

- Never hand-edit installed projections, `.skiller/installed.json`, Vercel lock files, or generated links.
- Do not absorb unrelated same-name skills.
- Skiller catalog skills are recorded only in `.skiller/installed.json`; native project skills are recorded only in `skills-lock.json`.
- A failed installation preserves verified independent skills and reports unresolved blockers.
