# Notes for AI agents

## Layout

The app is Rust only. The old Python app is kept only as the `python-final` tag.

## Releases

Prebuilt programs are published as GitHub Releases, not committed. The installers download from the latest release.

A merge to `main` runs `.github/workflows/release.yml`, which builds all targets and publishes the release with `SHA256SUMS`. The version comes from the commits since the latest tag (`.github/scripts/next-version.sh`): `feat` bumps minor, `fix`/`perf` bump patch, and a breaking change (`feat!:` or a `BREAKING CHANGE:` footer) bumps major (minor while on 0.x). Merges with only other types don't release. A higher version set by hand in `Cargo.toml` wins.

Pushes to other branches check that every commit follows Conventional Commits and run `cargo test` (`.github/workflows/test.yml`).

To release by hand instead, see "Building a release" in the README.

v0.1.0 was the first release.

## Code graph

`graphify-out/` holds a code graph built by [Graphify](https://github.com/Graphify-Labs/graphify): symbols, calls, imports and how modules cluster. Use it to learn the code's structure before grepping or reading whole files:

- Read `graphify-out/GRAPH_REPORT.md` first for the main modules and hubs.
- With `graphify` installed, ask scoped questions: `graphify query "<question>"`, `graphify explain "<symbol>"`, `graphify path "<A>" "<B>"`.
- The report names the commit it was built from. If code changed since, run `graphify update .` (offline, no API key).

The pre-commit hook in `.githooks/` rebuilds and commits the graph. Don't edit `graphify-out/` by hand.

## Pull requests and commits

- Write commit messages as [Conventional Commits](https://www.conventionalcommits.org): `type(scope)?: summary`, with type one of `feat fix perf refactor docs test build ci chore style revert`. When squash-merging, give the PR a conventional title too.
- Git hooks in `.githooks/` enforce this locally, update the code graph on commit and run `cargo test` before push. Enable with `git config core.hooksPath .githooks`.
- Don't put Claude session links (`https://claude.ai/code/session_…`) in pull request descriptions.
- Don't add a `Claude-Session:` trailer to commit messages.
