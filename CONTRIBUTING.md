# Contributing

## Settings in `.env`

Every setting the app reads from the environment must be listed in `.env.example`.
When a change adds a new `KEY=value` setting, add it to `.env.example` in the same
pull request, commented out, with a one-line explanation and its default:

```sh
# How often changed tracker stats are pushed to the index (default: 60)
# MEILI_STATS_SYNC_SECS=60
```

Settings a fresh checkout needs to start (such as `SECRET_KEY`) stay as active lines.

## Formatting and lints

CI runs these and fails on any difference or warning, so run them before pushing:

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```
