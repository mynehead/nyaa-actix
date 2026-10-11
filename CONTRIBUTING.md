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

Tests use in-memory SQLite. CI runs them a second time on PostgreSQL; to do that locally,
point `TEST_DATABASE_URL` at a server where you may create databases (every test gets its
own `nyaa_test_*` database, and the next run drops them):

```sh
TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test
```

Fixtures are written in SQLite's SQL. `X'..'` blobs work on both (the test template adds a
cast), but `datetime('now', ..)` and a hand-picked `id` that a later insert would also get
do not, since PostgreSQL numbers new rows from 1 whatever ids are already taken.
