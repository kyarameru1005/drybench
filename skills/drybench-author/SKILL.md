---
name: drybench-author
description: Use when writing or editing a Claude Code skill, subagent, or hook that will be installed with drybench — it defines the directory layout, required files, frontmatter, and the hook.json registration shape.
---

# drybench-author

## When to use

Whenever you are authoring something that drybench will install into `~/.claude`. drybench
will not recognise an entry whose required file is missing, so getting the layout right
matters more than getting the prose right.

## Layout

```
<source>/
  skills/<name>/SKILL.md      required — directory-based
  agents/<name>.md            file-based
  hooks/<name>/hook.json      required — directory-based
```

`<name>` may not contain `..`, a path separator, or a leading dot. drybench refuses those
outright.

## Frontmatter

Skills and subagents both need `name` and `description`. The `description` is what Claude
reads when deciding whether to load the thing — write it as *when to reach for this*, not
as *what this is*.

## hooks

A hook file does nothing on its own. `hook.json` is what gets registered into
`settings.json`:

```json
{
  "description": "...",
  "events": [
    {
      "event": "PostToolUse",
      "matcher": "Bash",
      "command": "\"$HOME/.claude/hooks/<name>/run.sh\"",
      "timeout": 5
    }
  ]
}
```

- `matcher` is omitted for events that do not take one (`Stop`, for example).
- `command` runs from an unspecified working directory — always use absolute paths.
- Keep the script fast. It runs inline with the tool call.

## What not to do

- Do not edit `~/.claude` directly to test something. Write it in the source directory
  and let drybench install it — that is what keeps it removable.
- Do not write secrets into a skill, subagent, or hook. These files get synced and shared.
