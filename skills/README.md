# skills

Skills that drybench ships with — not scaffolds, but working skills you can install into
your own `~/.claude`.

Prompts live here rather than inside the binary (plan §10): they can be improved without
a rebuild, and the skill itself is distributable through drybench like anything else.

| Skill | What it does |
|---|---|
| `drybench-author/` | Teaches Claude the layout and frontmatter drybench expects, so `claude` invoked from the TUI writes something installable on the first try. |
