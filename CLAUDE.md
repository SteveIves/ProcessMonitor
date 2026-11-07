# CLAUDE.md

Process Monitor is a Rust command-line program (binary `process-monitor`) that checks websites and
servers and sends alerts through email, SMS and push providers. README.md describes the features,
configuration and deployment; read it before changing behaviour.

## Making changes

- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` before committing.
  CI runs the same on Linux, Windows and macOS.
- Every source file has a header comment and every type and function a doc comment saying what it
  does, its parameters, return value, side effects and errors. Update comments with the code.
- Record user-visible changes in CHANGELOG.md under `## Unreleased` (create the heading at the top
  if it isn't there). Don't edit the version in Cargo.toml by hand.
- When adding or changing a notifier or config key, update `notifiers.example.toml` /
  `monitor.example.toml` and README.md. The tests parse both example files and count the providers.

## Releasing

When asked to "do a release", trigger the **Release** workflow (`.github/workflows/release.yml`) on
`main` with the GitHub Actions API (workflow_dispatch), input `bump` = `patch` unless a minor or
major release was asked for. Don't create tags yourself: the workflow runs the tests, picks the next
version, commits "Release v<version>" to main, tags it, builds all platforms and publishes the
GitHub release. Make sure everything to be released is pushed to main first, and fetch main
afterwards, because the workflow adds a commit. Then check that the run succeeded and report the
release URL.
