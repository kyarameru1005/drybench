# Architecture

Flat `src/`, single binary, minimal dependency tree.

## Modules

| Module | Responsibility | Status |
|---|---|---|
| `main.rs` | Terminal setup, event loop, panic hook. **The only place real paths are resolved.** | port |
| `cli.rs` | `--source` / `--target` / `--dry-run` | new |
| `model.rs` | `ItemKind` (Skill/Agent/Hook), `DraftItem`, `ItemState` (On/Off/Conflict/Unmanaged) | port |
| `scan.rs` | Walk the source and target directories, name validation | port + extend |
| `manifest.rs` | Read/write `~/.claude/.drybench-manifest.json` | port |
| `settings.rs` | `hook.json` parsing, `settings.json` hook registration/removal | port |
| `sync.rs` | Hashing, state resolution, plan construction and application — the safety gates | port |
| `import.rs` | Bring an unmanaged entry under management | new |
| `scaffold.rs` | Create an entry from `templates/` | new |
| `editor.rs` | Hand an entry to `$EDITOR` or `claude` | new |
| `ui.rs` | ratatui rendering and key handling | port + extend |

"port" means the implementation moves over from `apps/proteus`; each such file carries a
`TODO(migrate)` note naming its source and what must change on the way in.

## Data flow

```
scan(source) ─┐
              ├─→ resolve_state ──→ ItemState ──→ ui (list)
scan(target) ─┤                                      │
manifest ─────┘                                      │ user toggles, presses sync
                                                     ▼
                                              build_plan ──→ ui (confirm)
                                                                 │ Enter
                                                                 ▼
                              safety gates 1-4 (re-checked) ─→ apply_plan
                                                                 │
                                              ┌──────────────────┼──────────────┐
                                              ▼                  ▼              ▼
                                        copy / delete     settings.json    manifest
```

## Two directories, always passed in

- **source** — where entries are authored. Default: `drafts/` found by walking up from
  cwd, then `~/.drybench/source`. Overridable with `--source`.
- **target** — where entries are installed. Default `~/.claude`. Overridable with
  `--target`. Switching between `~/.claude` and a project's `.claude/` is v0.2.

Neither is ever resolved implicitly below `main.rs`. See
[design-principles.md](design-principles.md), "Corollary: testability".

## Entry shapes

| Kind | Unit | Required file |
|---|---|---|
| skill | directory | `SKILL.md` |
| agent | single file | `<name>.md` |
| hook | directory | `hook.json` |

Hooks are the only kind that touch `settings.json`.
