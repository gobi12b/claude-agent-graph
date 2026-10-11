# Notes for AI agents

## Layout

The app is Rust only. The old Python app is kept only as the `python-final` tag.

## Releases

Prebuilt programs are published as GitHub Releases, not committed. The installers download from the latest release.

A merge to `main` runs `.github/workflows/release.yml`, which builds all targets and publishes the release with `SHA256SUMS`. The version comes from the commits since the latest tag (`.github/scripts/next-version.sh`): `feat` bumps minor, `fix`/`perf` bump patch, and a breaking change (`feat!:` or a `BREAKING CHANGE:` footer) bumps major (minor while on 0.x). Merges with only other types don't release. A higher version set by hand in `Cargo.toml` wins.

Pushes to other branches check that every commit follows Conventional Commits and run `cargo test` (`.github/workflows/test.yml`).

To release by hand instead, see "Building a release" in the README.

v0.1.0 was the first release.

## Pull requests and commits

- Write commit messages as [Conventional Commits](https://www.conventionalcommits.org): `type(scope)?: summary`, with type one of `feat fix perf refactor docs test build ci chore style revert`. When squash-merging, give the PR a conventional title too.
- Don't put Claude session links (`https://claude.ai/code/session_…`) in pull request descriptions.
- Don't add a `Claude-Session:` trailer to commit messages.
