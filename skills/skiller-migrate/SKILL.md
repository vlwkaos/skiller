---
name: skiller-migrate
description: Guide legacy Agent Skills into an explicit Skiller catalog, configuration, selected Vercel agents, verified installation, and optional approved cleanup.
---

# Skiller migration

Use Skiller's migration-plan engine. The app owns validation, copying, configuration, installation, ownership, and cleanup. Never hand-edit installed projections, Skiller state, Vercel lock files, or generated links.

## Choose the interface

| Situation | Command |
|---|---|
| Human-guided migration | `skiller migrate` |
| Create an agent-editable plan | `skiller migrate --init migration.json` |
| Read-only deterministic validation | `skiller migrate --plan migration.json --check` |
| Interactive plan application | `skiller migrate --plan migration.json --apply` |
| Reviewed noninteractive application | `skiller migrate --plan migration.json --apply --yes` |

## Workflow

1. Run `skiller doctor [-g] --print` and retain the report.
2. Resolve one explicit writable catalog checkout, alias, and portable source. Never infer among clones.
3. Select canonical legacy skill directories. Prefer real directories under `.agents/skills`, not agent projection symlinks.
4. For each skill, choose an existing catalog scope, global or project target, Enabled or Manual mode, project Git-ignore state, and exact legacy installed name.
5. Select at least one Vercel agent. Defaults are `universal`, `claude-code`, and `pi`, but any can be removed or additional Vercel-supported names added.
6. Keep `cleanupLegacy` false on the first review. Cleanup is a separate destructive approval and is valid only with installation enabled.
7. Run `--check`. Resolve every invalid name, dependency, scope, eligibility, collision, symlink, or unowned path before applying.
8. Show the complete plan and ask approval. Use `--yes` only when the user already approved that exact checked plan.
9. Apply. Skiller copies catalog sources, creates configuration, installs through pinned Vercel Skills, verifies every selected agent through Vercel's JSON listing, and records ownership.
10. Run Doctor again. Inspect the catalog diff before any commit or publication.
11. If cleanup was deferred, create and check a second exact plan with `cleanupLegacy: true`; apply only after replacement installation is healthy.
12. Report catalog/config/state paths, selected agents, modes, cleanup names, verification, and remaining commit/push/reload steps.

## Boundaries

- The migration command does not commit, push, publish, or choose company ownership.
- A failed pre-install mutation rolls catalog metadata and newly copied skills back.
- An install failure leaves valid catalog/config state and an owned recovery journal when placement started.
- Never use migration to absorb unrelated same-name skills.
- Source names remain unpostfixed. Semantic scope appears in projected descriptions as `[scope]` and in Pygmalion aliases as `$scope:name`.
