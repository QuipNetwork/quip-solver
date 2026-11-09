# Versioning and release-tag standard

This is the versioning standard for every Quip repository that publishes
release tags, container images, or native binaries that `quip-node-manager`
consumes. At the time of writing those are **quip-solver**, **quip-validator**,
and **dashboard.quip.network**. Apply it identically in each. The CI section
below describes this repository's `.gitlab-ci.yml`.

## The rule

| Artifact | Format | Example | Why |
|----------|--------|---------|-----|
| **Git release tag** | SemVer, **hyphenated** pre-release: `vMAJOR.MINOR.PATCH-rcN` | `v0.3.7-rc1` | quip-node-manager parses this |
| Stable git tag | `vMAJOR.MINOR.PATCH` | `v0.3.7` | — |

This repository ships no Python package. The `quip-solver-core` wheel versions
from its own repository.

## Why the hyphen matters (do not drop it)

`quip-node-manager` decides whether to offer a native-binary update by parsing
the **git release tag** with a SemVer comparator
(`src-tauri/src/update.rs::parse_semver`). That parser splits the pre-release on
`-`:

- `v0.3.7-rc1` parses as `(0, 3, 7, 1)`. This is correct, and `rc17 > rc5`.
- `v0.3.7rc1` parses as `(0, 3, 0, MAX)`. This is wrong. The `.7` patch and
  the `rc1` are lost, so *every* no-hyphen rc collapses to the same value.
  rc5 == rc16 == a final 0.3.0, and the updater never offers a newer rc. This
  is the bug that froze deployed Macs on an old rc.

The standard fixes the **tag** to the form node-manager already understands
rather than changing the comparator, so any other SemVer consumer also works.

## CI tag rules

This repository's `.gitlab-ci.yml` publishes images under the `v0.3/`
registry namespace (`v0.3/quip-solver` and `v0.3/quip-solver-cuda`). Two
anchored rules decide which floating tags a git tag moves:

```yaml
.release-tag-rule: &release-tag-rule
  if: $CI_COMMIT_TAG =~ /^v\d+\.\d+\.\d+$/
  variables:
    MOVE_LATEST: "1"
    MOVE_STABLE: "1"
    MOVE_BETA: "1"

.rc-tag-rule: &rc-tag-rule
  if: $CI_COMMIT_TAG =~ /^v\d+\.\d+\.\d+-rc\d+$/
  variables:
    MOVE_BETA: "1"
```

- A stable tag `vX.Y.Z` pins `:<tag>` and moves `:beta`, `:stable`, and
  `:latest`.
- A hyphenated candidate `vX.Y.Z-rcN` pins `:<tag>` and moves `:beta` only.
- Any other tag, including the legacy no-hyphen form, builds and pins `:<tag>`
  and moves no floating tag.

`quip-node-manager` follows `:beta` or `:stable` for images and the
hyphenated git tag for the native macOS binary.

The `release-binaries` job creates a GitLab Release only for a tag that
matches `^v\d+\.\d+\.\d+(-rc\d+)?$`. A suffixed tag such as
`v0.3.1-rc3-aglais-prerelease` still uploads its bundle to the generic package
registry, but it gets no Release object and the updater never offers it.

## Cutting a release

1. Bump `version` in `crates/quip-coordinator/Cargo.toml` to the tag without
   the `v` prefix, and commit the bump. The macOS build job compares
   `quip-coordinator --version` with the tag and fails the release on a
   mismatch.
2. Tag with the **hyphenated** form and push the tag:
   ```bash
   git tag -a v0.3.8-rc1 -m "v0.3.8-rc1"
   git push origin v0.3.8-rc1
   ```
3. CI builds the images (`:v0.3.8-rc1`, and `:beta` for a candidate), builds
   the native `quip-coordinator` for darwin-arm64, bundles it with the
   darwin-arm64 miner binaries as `quip-solver-darwin-arm64.tar.gz`, and
   attaches that bundle to the GitLab Release. The release marker
   node-manager records is the git tag, so the hyphenated form flows through
   to the update check.

> Legacy `vX.Y.ZrcN` (no hyphen) tags already published remain valid images
> but are invisible to node-manager's binary updater. The first hyphenated tag
> a node-manager instance can see (numerically greater than its installed rc)
> is what unsticks it.
