# skiller

`skiller` manages one source skills directory and links it into multiple AI tool skill locations.

## Usage

```bash
skiller source ~/skills
skiller target add claude
skiller target add codex
skiller target add openclaw
skiller link
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
skiller target add mytool ~/.config/mytool/skills
```
