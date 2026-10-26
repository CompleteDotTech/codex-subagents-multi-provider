# Workflow Strategy

The workflows in this directory are split so that pull requests get fast, review-friendly signal while `main` still gets the full cross-platform verification pass.

## Pull Requests

- Required checks run against GitHub's synthetic merge commit, not the pull
  request head alone. This includes changes already on `main` and catches
  conflicts before they reach the branch.
- `bazel.yml` is the main pre-merge verification path for Rust code.
  It runs Bazel `test` and Bazel `clippy` on the supported Bazel targets,
  including the generated Rust test binaries needed to lint inline `#[cfg(test)]`
  code.
- `rust-ci.yml` keeps the Cargo-native PR checks intentionally small:
  - `cargo fmt --check`
  - `cargo shear`
  - `argument-comment-lint` on Linux, macOS, and Windows
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes

## Post-Merge On `main`

- `bazel.yml` also runs on pushes to `main`.
  This re-verifies the merged Bazel path and helps keep the BuildBuddy caches warm.
- `rust-ci-full.yml` is the full Cargo-native verification workflow.
  It keeps the heavier checks off the PR path while still validating them after merge:
  - the full Cargo `clippy` matrix
  - the full Cargo `nextest` matrix via per-platform archive-backed shards
  - Windows ARM64 nextest archives cross-compiled on Windows x64, then replayed on native Windows ARM64 shards
  - release-profile Cargo builds
  - cross-platform `argument-comment-lint`
  - Linux remote-env tests

## Rule Of Thumb

- If a build/test/clippy check can be expressed in Bazel, prefer putting the PR-time version in `bazel.yml`.
- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Reserve `rust-ci-full.yml` for heavyweight Cargo-native coverage that Bazel does not replace yet.

## Fork runner selection

The blocking CI family and full Cargo workflows retain the upstream runner
selection for `openai/codex`. In other repositories, their `runs-on` expressions
use standard GitHub-hosted runners instead of the upstream private runner groups
and larger macOS runners:

| Upstream runner | Fork runner | Architecture |
| --- | --- | --- |
| Linux x64 group | `ubuntu-24.04` | x64 |
| Linux ARM64 group | `ubuntu-24.04-arm` | ARM64 |
| Windows x64 group | `windows-2025` | x64 |
| Windows ARM64 group | `windows-11-arm` | ARM64 |
| `macos-15-xlarge` | `macos-15` | ARM64 |

The matrix names and cache identities remain upstream logical identifiers; the
workflow's runner setup log identifies the actual hosted image. The macOS x64
target retains its existing cross-build on an ARM64 host. Windows ARM64 nextest
archives still build on x64 and execute on native ARM64 shards. Test commands,
matrix targets, toolchains, actions, and required-result jobs are unchanged.

These mappings require neither a BuildBuddy key nor private runner provisioning;
the existing Bazel wrapper falls back to local execution without that key.
Standard hosted machines have fewer resources than upstream's private/larger
runners, so startup alone does not establish build success or native test
coverage. Record actual job results and artifact source identities.

GitHub documents the labels and architecture under
[standard hosted runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
This mapping does not change release workflows or billing settings.
