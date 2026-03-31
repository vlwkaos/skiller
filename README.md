# skiller

`skiller` manages one source skills directory and links it into multiple AI tool skill locations.

It supports two target modes:

- `folder`: symlink the entire target skills directory to the source directory
- `granular`: keep the target directory as a real directory and symlink each source skill into it

`granular` is useful when the target already has tool-specific skills you do not want to migrate into a shared source. For example, you can keep `~/.hermes/skills` as the source of Hermes-only skills and link those into `~/.claude/skills` without replacing Claude's whole skills directory.

## Usage

```bash
skiller source ~/.hermes/skills
skiller target add claude granular
skiller link claude
skiller status
```

Built-in target types:

- `claude` -> `~/.claude/skills`
- `codex` -> `~/.codex/skills`
- `opencode` -> `~/.config/opencode/skills`
- `openclaw` -> `~/.openclaw/skills`
- `hermes` -> `~/.hermes/skills`

You can also provide an explicit path for a built-in or custom type:

```bash
skiller target add codex ~/.codex/skills
skiller target add mytool granular ~/.config/mytool/skills
```

## Commands

```bash
skiller source <path>
skiller target add <type> [mode|path] [path]
skiller target remove <type>
skiller target list
skiller link [type]
skiller unlink [type]
skiller status
```

`target add` parsing rules:

- `skiller target add claude` -> built-in path, `folder` mode
- `skiller target add claude granular` -> built-in path, `granular` mode
- `skiller target add codex ~/.codex/skills` -> explicit path, `folder` mode
- `skiller target add mytool granular ~/.config/mytool/skills` -> explicit path, `granular` mode

## Mode behavior

### folder

This is the original behavior. The entire target directory becomes a symlink to the source directory.

```bash
skiller source ~/skills
skiller target add codex
skiller link codex
```

### granular

The target root stays a normal directory. Each skill directory from the source is linked individually into the target.

```bash
skiller source ~/.hermes/skills
skiller target add claude granular
skiller link claude
```

This allows target-specific real directories and linked shared skills to coexist.

## Notes

- `link` and `unlink` can target one configured type or all configured targets.
- `unlink` in `granular` mode only removes per-skill symlinks that point to the configured source. It does not remove real directories.
- `status` shows the target mode and a granular summary like `granular 3/5 linked, 1 conflict, 1 missing`.
