# Releasing

Releases are built and published by `.github/workflows/release.yml` when a tag
`vX.Y.Z` is pushed. Each release carries, per target, a binary
(`rommd-<target>[.exe]`) and its minisign signature (`.minisig`), plus
`SHA256SUMS` and the install scripts.

## One-time setup

1. Generate the signing key. CI signs without a password, so create the key
   without one (`-W`) and keep the secret key only in GitHub:

   ```sh
   minisign -G -W -p minisign.pub -s rommd.key
   ```

2. Commit `minisign.pub` at the repository root. `build.rs` embeds it, and the
   updater refuses any update not signed by it. Builds made before it exists,
   or without it, can never install updates.
3. Add the contents of `rommd.key` as the repository secret
   `MINISIGN_SECRET_KEY` (Settings → Secrets and variables → Actions). Store a
   backup of it somewhere safe offline, then delete the local file.

Losing the secret key means existing installs can no longer update themselves:
a new key needs a new `minisign.pub`, which only reaches users through a
manual reinstall. Rotating it therefore also means telling users to reinstall.

The repository must be public: the updater and install scripts download from
GitHub without logging in.

## Cutting a release

1. Bump `version` in `Cargo.toml`, run `cargo test`, commit.
2. Tag and push: `git tag v0.2.0 && git push origin v0.2.0`.
3. The workflow checks that the tag matches `Cargo.toml` and that
   `minisign.pub` exists, builds every target, signs each binary with the
   trusted comment `rommd <version> <asset>` (the updater checks it), verifies
   the signatures, and publishes the release.

Installs in `auto` mode pick it up within a day.
