# Design principles

These are not guidelines. A change that breaks one of them is not accepted, because the
whole value of drybench is that its blast radius is knowably small.

`~/.claude` is outside this repository. It is the user's live working environment.
**Nothing that drybench did not itself install is ever overwritten or deleted.**

## 1. An entry with no manifest record is never touched

Unmanaged entries are shown — seeing them is the point — but they are locked, always, in
every code path. There is no override.

## 2. A manifest record with a mismatched hash is not touched either

The manifest stores a sha256 of the content at the moment drybench wrote it. If the bytes
on disk have changed since, the user edited it by hand. That is a **Conflict**: skipped
by default, actionable only after an explicit per-entry override on the confirmation
screen.

## 3. State is re-derived immediately before the write

The state shown in the list is from the last scan. Between the scan and the keypress,
anything could have happened. Every action re-checks its own preconditions right before
executing (TOCTOU).

## 4. Write targets are verified by canonical path, and symlinks are refused

Before any write or delete, the resolved path must sit inside the target directory. If
the target itself is a symlink, drybench refuses rather than following it. Entry names
containing `..`, a path separator, or a leading dot are excluded at scan time.

## 5. `settings.json` is edited surgically

The one place drybench writes to a file the user owns.

- Only the group drybench inserted is removed, matched by the hash recorded at install.
- Nothing outside `hooks` is read or written. Other people's hook groups are left alone.
- Malformed JSON means **do nothing** — never repair, never rewrite.
- Backup first (`settings.json.drybench-backup`), then write a temp file and rename.
  Key order is preserved via `serde_json`'s `preserve_order`.
- An operation involving no hooks does not open `settings.json` at all.
- A failed registration does not roll back the file copy. The manifest is saved first so
  the record and the filesystem stay consistent; toggling on again re-registers.

## 6. Destructive operations go through a confirmation screen

Every path that will be written or removed is listed before anything happens.

## 7. Generated content is never installed unreviewed

Including — especially — content Claude wrote. Output from a child process is ordinary
source content and still passes every gate above.

## Corollary: no API keys

drybench shells out to the user's `claude` binary. It never calls a model API directly,
so there is no credential for it to hold, leak, or mishandle.

## Corollary: testability

Every I/O function takes its target directory as a parameter. Path resolution happens
only in `main.rs` and `cli.rs`. Tests therefore run entirely against directories under
`std::env::temp_dir()` and can never reach a real `~/.claude`.
