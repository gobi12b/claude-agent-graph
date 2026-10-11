# Notes for AI agents

## Layout

The app is Rust only. The old Python app is kept only as the `python-final` tag.

## Releases

Prebuilt programs are published as GitHub Releases, not committed. The installers download from the latest release.

Every merge to `main` runs `.github/workflows/release.yml`, which builds all targets and publishes the release with `SHA256SUMS`. The tag is the `Cargo.toml` version, or the next patch after the latest tag if that version is already released. Bump `Cargo.toml` for a minor or major release. Docs-only merges don't release.

Pushes to other branches run `cargo test` (`.github/workflows/test.yml`).

To release by hand instead, see "Building a release" in the README.

v0.1.0 was the first release.

## Pull requests and commits

- Don't put Claude session links (`https://claude.ai/code/session_…`) in pull request descriptions.
- Don't add a `Claude-Session:` trailer to commit messages.
