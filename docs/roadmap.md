# Roadmap

The core loop:

```
    author ──→ install ──→ use ──→ review ──┐
      ↑                                     │
      └──────── fix / drop / promote ───────┘
```

"install" is the part that already works. Each release extends one step outward.

## v0.1 — fixed at four items

Nothing else ships in v0.1. This list is deliberately closed.

1. **Inspect and import `~/.claude`** — unmanaged entries appear in the list; `import`
   brings them under management.
2. **Toggle and apply safely** — already implemented: manifest + hash double gate,
   confirmation screen, hook registration.
3. **An authoring path** — `new` scaffolds from a template, then hands off to `$EDITOR`
   or `claude`; re-scan on return.
4. **`--source <path>`** — without it, anyone whose layout differs gives up on first run.

Before announcing: demo GIF, English README, release binaries.

## v0.2

- Conflict diffs — showing *what* differs. Without this nobody else can judge a conflict.
- `claude -p` draft generation (non-interactive, with progress).
- Bulk toggle, sort by state.
- Target switching (`~/.claude` ↔ a project's `.claude/`).

## v0.3 and later

- Promotion to plugin-marketplace format — so drafts have an exit, not just an inbox.
- Apply log and rollback.
- Usage observation (session log aggregation), trial expiry.

## Explicitly out of scope

- Calling a model API directly. That would mean holding an API key.
- A registry or sharing hub — the official plugin marketplace owns that.
- Agents other than Claude Code, through v1.
- A GUI.

## Open questions

- What to call the source directory (`drafts` is inherited, not decided).
- UI strings: English only, or i18n.
- Compatibility policy for the `settings.json` hook format, which is the part most likely
  to move underneath us.
