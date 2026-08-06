# Contributing

Thanks for taking a look. drybench is small on purpose — please read
[docs/design-principles.md](docs/design-principles.md) before proposing anything that
touches how files are written.

## Development

```sh
cargo check
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Ground rules

- **I/O functions take their target directory as an argument.** Nothing resolves
  `~/.claude` on its own deep in the call stack. Path resolution happens in `main.rs` and
  `cli.rs`, nowhere else. This is what makes the tests able to run entirely inside a
  temporary directory, and it is not negotiable.
- **Tests never touch a real `~/.claude`.** They build their own directories under
  `std::env::temp_dir()`. There are no dev-dependencies and adding one needs a good
  reason.
- **No new dependency without a reason in the PR description.** Single binary, minimal
  tree.
- **No API keys, no network calls to model providers.** AI assistance goes through the
  user's `claude` binary as a child process.

## Scope

v0.1 is deliberately fixed at four things: inspect/import, toggle, scaffold, and
`--source`. Usage telemetry, trial expiry, and plugin promotion are later — please open
an issue to discuss before building them.
