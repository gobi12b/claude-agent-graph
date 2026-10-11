# Notes for AI agents

## Layout

The app is Rust only. The old Python app is kept only as the `python-final` tag.

## Releases

Prebuilt programs are published as GitHub Releases, not committed. The installers download from the latest release.

1. Push `main` before tagging, so the tag matches the code the binaries were built from.
2. Build each target (see "Building a release" in the README) into a git-ignored `binaries/` folder.
3. `cd binaries && sha256sum agent-graph-* > SHA256SUMS && sha256sum -c SHA256SUMS`
4. `gh release create vX.Y.Z agent-graph-* SHA256SUMS --target main --generate-notes`

v0.1.0 was the first release.

## Pull requests and commits

- Don't put Claude session links (`https://claude.ai/code/session_…`) in pull request descriptions.
- Don't add a `Claude-Session:` trailer to commit messages.
