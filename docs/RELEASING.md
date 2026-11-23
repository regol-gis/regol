# Releasing regol

The `regol`, `regol-overpass` and `regol-cli` crates ship together.
A release is identified by an annotated Git tag such as `v0.2.0`. Each tag
identifies one commit and one set of published files.

## Choose a version

Use Semantic Versioning for the public Rust API, command-line interface and
documented behaviour. During 0.x, increment the minor version for incompatible
changes or new features and the patch version for backward-compatible fixes. From 1.0 onward,
use a major version for incompatible changes, a minor version for compatible
features and a patch version for compatible fixes. Use `X.Y.Z-rc.1`, `-rc.2`
and so on for release candidates. [Cargo compatibility conventions](https://doc.rust-lang.org/cargo/reference/semver.html)

## Prepare and verify

1. Set `workspace.package.version` in the root manifest and the matching workspace dependency versions in
   `overpass/Cargo.toml` and `cli/Cargo.toml`. All three crates inherit the workspace
   version. For a version change, refresh the local package entries in both
   lockfiles and include their diffs in the release PR.

   ```sh
   cargo metadata --format-version 1 > /dev/null
   cargo metadata --manifest-path fuzz/Cargo.toml --format-version 1 > /dev/null
   ```

2. Add `docs/releases/X.Y.Z.md` (include the suffix for a candidate). Describe
   user-visible changes, compatibility changes and any migration steps. Run
   `python3 tools/release.py --check` to verify the versions and notes.
3. Merge the reviewed change after **Check workspace** passes. The **Release**
   workflow can be run manually via `workflow_dispatch` on a candidate branch
   to rehearse the build, packaging and smoke tests. Download the rehearsal
   artifacts from the workflow run to inspect the same files a tag would
   publish. Locally, run `python3 tools/release.py` from a clean Linux x86-64
   checkout with Python 3.11.4+ and a rustup toolchain with its standard
   library copyright notices.

## GitHub release

From the tested commit on `main`, derive the tag from Cargo and push that
single tag. An annotated tag can also be signed using `git tag -s`.

```sh
version=$(python3 tools/release.py --check)
git tag -a "v$version" -m "regol $version"
git push origin "refs/tags/v$version"
```

GitHub Actions checks that the tag matches the manifest and the commit belongs
to `main`. It runs the workspace tests, packages all three crates, builds the Linux
x86-64 GNU `regol` binary from the archived sources, checks its version, and
builds and queries a small GOL. It publishes the release notes, binary archive
with dependency licenses, all three source crates and `SHA256SUMS` to GitHub
Releases. Release candidates are marked as prereleases. Download the assets and
checksum file, then run `sha256sum -c SHA256SUMS` to verify them.

Tag publication uses `GITHUB_TOKEN`. Repository maintainers can restrict the
`v*` tag pattern using a GitHub ruleset.

If a build fails, fix the cause before publishing. For a change to published
code or assets, issue a new version.

## Publish the crates

The GitHub tag builds downloadable binaries. Publishing to crates.io is a
separate maintainer action. Review each package's README and archive contents
from a clean checkout of the release tag.

```sh
cargo package -p regol --list
cargo package -p regol-overpass --list
cargo package -p regol-cli --list
cargo publish --locked --dry-run -p regol --all-features
```

Publish `regol` first, then `regol-overpass`, then the CLI. Wait for each version
to become visible in the registry before checking the next package.

```sh
cargo publish --locked -p regol
cargo publish --locked --dry-run -p regol-overpass
cargo publish --locked -p regol-overpass
cargo publish --locked --dry-run -p regol-cli
cargo publish --locked -p regol-cli
```

Check the rendered READMEs, the library API on docs.rs (built automatically
after publishing `regol`), and installation of `regol-cli` from the registry.
Released versions are immutable.
[Cargo publishing guide](https://doc.rust-lang.org/cargo/reference/publishing.html)
