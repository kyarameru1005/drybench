# templates

Scaffolds used by `drybench new`. They are compiled into the binary with `include_str!`
(see `src/scaffold.rs`), but they live here as ordinary files so they can be read and
improved without digging through string literals.

| Path | Used for |
|---|---|
| `skill/SKILL.md` | `<source>/skills/<name>/SKILL.md` |
| `agent/agent.md` | `<source>/agents/<name>.md` |
| `hook/hook.json` | `<source>/hooks/<name>/hook.json` — the `settings.json` registration |
| `hook/run.sh` | `<source>/hooks/<name>/run.sh` — the script the registration points at |

`{{name}}` is the only placeholder. It is replaced with the entry name the user typed.

A skill is not recognised without `SKILL.md`; a hook is not recognised without
`hook.json`. Both are directory-based. Subagents are a single file.
