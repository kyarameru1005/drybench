# drybench

**A local workbench for your Claude Code setup** — see what is installed in `~/.claude`,
safely toggle skills, subagents and hooks on and off, and spin up new ones to try.

<!-- TODO: demo GIF here (record with VHS so it can be re-recorded). See docs/demo.md -->

> [!NOTE]
> **v0.1 — not released yet.** This repository currently contains the skeleton only.

---

## The problem

Write enough skills, subagents and hooks and `~/.claude` fills up. Two things get painful:

- **You lose track of what is in there** — and whether you still need it.
- **Hooks do not fire just because the file exists.** They have to be registered in
  `settings.json`, by hand, in the right shape.

Trying out someone else's skill has the same shape: copy files in, edit JSON, and hope
you can put it all back.

## What drybench does

drybench is not a skill generator — Claude Code already writes skills better than a
template ever will. drybench is the **bench you put them on**: it takes them in, tries
them out, and takes them back off without leaving anything behind.

- **Inspect** — one screen showing everything in `~/.claude`, managed or not.
- **Import** — take what is already there under management, non-destructively.
- **Toggle** — install and uninstall, including the `settings.json` hook registration.
- **Create** — scaffold a new skill and hand it straight to `$EDITOR` or `claude`.

## Safety, by design

This is the part that is not bolted on afterwards:

1. **Anything not in the manifest is never touched.** Unmanaged entries are locked, always.
2. **A manifest entry with a mismatched hash is not touched either.** Conflicts require
   explicit permission.
3. **State is re-checked immediately before writing** (no TOCTOU window).
4. **Write targets are verified by canonical path** to be inside the destination
   directory, and symlinked targets are refused.
5. **Your `settings.json` is edited by removing only the exact group drybench added,**
   matched by hash. Malformed JSON means drybench does nothing. Writes are backed up
   and atomic.
6. **Destructive operations always go through a confirmation screen.**
7. **Nothing generated — including by Claude — is installed without your review.**

The blast radius is limited to what drybench itself put there.

## Install

<!-- TODO: fill in once release binaries exist (macOS arm64, Linux x86_64/aarch64) -->

```sh
cargo install --path .
```

## Usage

```sh
drybench                      # inspect ~/.claude
drybench --source ./drafts    # use a different source directory
drybench --help
```

<!-- TODO: keybindings table -->

## Compatibility

Tested against Claude Code `<version>`.
<!-- TODO: pin the verified version. The settings.json hook format is the moving part. -->

## Non-goals

- Calling any model API directly. drybench shells out to your `claude` binary, so it
  never holds an API key.
- Being a registry or sharing hub — that is what the official plugin marketplace is for.
- Supporting agents other than Claude Code, at least through v1.
- A GUI.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The module map is in
[docs/architecture.md](docs/architecture.md), the roadmap in
[docs/roadmap.md](docs/roadmap.md), and the rules that are not up for negotiation in
[docs/design-principles.md](docs/design-principles.md).

## License

MIT — see [LICENSE](LICENSE).
