# Contributing

cephlens is a small, lab-first tool. Issues and pull requests are welcome.

## Development

```sh
cargo build
cargo test
```

Before opening a pull request, run the same checks CI runs:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Conventions

- Keep changes surgical and match the surrounding style.
- Short imperative commit subjects. The history signs off commits
  (`git commit -s`); please keep doing so.
- New parsing or behavior comes with a test where practical (see the parser
  tests in `src/kfstrace.rs` and `src/radostrace.rs`).
