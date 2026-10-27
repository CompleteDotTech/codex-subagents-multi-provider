# Licensed age source and native scrypt calibration

This focused source copy retains `age` 0.12.1 from public rage revision
`b5b68c4f4f7ea5c368bc060f6fe0237549c3a13f` under its original MIT or Apache-2.0
license. Both license texts, original crate manifest, original workspace manifest,
fixtures, assets, translations and benchmarks are retained. `age-core` remains
the unchanged Git dependency at the same immutable upstream revision.

`age.upstream.json` records the 174 original source/license file hashes.
`age.patched.json` allows exactly thirteen modified upstream files, two provenance
copies, one style file and two integration files (Bazel rules and standalone
lock). `age.patch` reconstructs
the manifest normalization, native calibration, canonical decimal parsing, localization
resource tests and strict source corrections against the pinned original bytes. The offline verifier checks the exact file set, aliases, hashes,
normalization and reverse patch before a style or manifest exception applies.

The twelve patched source files are explicitly enumerated in the verifier: encrypted
identity, header format, localization, native tags and recipients, primitives and
stream, protocol, simple helpers, and utility parsing. Typed fixed-size copies and
explicit contextual invariant panics preserve encoding and internal failure policy.
`simple::encrypt` now propagates custom recipient and invalid-label errors through
its existing Result contract; two regression tests assert the original errors.
Native stanza-derivation hooks remain after format/work prechecks. The localization
resource assertions are retained. Stream chunk boundaries use the fixed 2^16 mask
without raising the retained source MSRV. The encrypted identity test module is
armor-qualified to match its sole existing consumer; its test body is unchanged.

The additional armor unit binary and wrapper reuse the existing default unit
metadata hub and activate only default plus armor. This permits separate configured
feature proof, strict linting, and execution of armor inline tests, including the
encrypted identity round trip. These provenance records do not claim those gates
have passed.

## Manifest and feature policy

`Cargo.toml.orig` and `UPSTREAM-WORKSPACE.toml` preserve the inheritance inputs.
The normalized manifest expands metadata and dependency settings without changing
upstream versions, requirements or feature definitions. Only the sibling
`age-core` path becomes the project's existing exact Git pin. All original dev
and benchmark requirements remain; their test-only graph is distinct from the
empty default feature set used by production consumers.

The crate is a standalone workspace, explicitly excluded from the application
workspace. This prevents its original dev features from being unified into the
normal production metadata. The production Bazel label aliases the generated
path-dependency library. Three separate test metadata repositories use this same
source and standalone lock: default unit tests, SSH vectors, and armor/async
testkit. Each test and its library use one repository's complete dependency graph,
including its own coherent `age-core` target; types do not cross graph boundaries.
Optional features are selected only on the corresponding test repositories.

The native change calibrates each new recipient and identity independently,
using monotonic elapsed time and the median of three measurements. Native
zero-resolution progression is limited to factors 10 through 16, at most 21
measurements. An invalid monotonic interval or exhausted zero medians panics
before construction rather than weakening the writer factor or reader policy.
This is an explicit native availability change: callers must not assume it is
returned as an ordinary crypto error. Browser/WASM calibration remains the
original upstream implementation. The independent reader's `target + 4`
pre-derivation limit and explicit setters remain unchanged; no writer-authorized
cap or secret/calibration cache is introduced.

The approximately 64 MiB maximum primary scrypt workspace describes calibration
only, plus implementation overhead. Actual encryption and admitted decryption
can require substantially more. Finite measurement counts do not promise a hard
wall-time bound or acceptance across real capacity changes. Median calibration
still needs representative native strength/overhead evidence; pure timing tests
alone do not establish that evidence.

## Verification and upstream reproduction

From the repository root:

```sh
python3 codex-rs/third-party/verify_age.py
python3 -m unittest discover -s codex-rs/third-party -p test_verify_age.py
```

For archive-backed provenance, produce the public source archive from an
independently verified checkout at the pinned revision:

```sh
git archive --format=tar --output=age-upstream.tar \
  b5b68c4f4f7ea5c368bc060f6fe0237549c3a13f \
  age LICENSE-MIT LICENSE-APACHE Cargo.toml
python3 codex-rs/third-party/verify_age.py --archive age-upstream.tar
```

Its SHA256 is
`3c04247d64d18b001abcf25b77ae0e3a5e1eb4ecd1d1b1622223c681a87acce0`.
Verification reads the archive without extracting it or accessing the network.

The explicit default unit-test library preserves upstream empty default features.
Original integration requirements remain feature-qualified: `test_vectors`
requires `ssh` and `cli-common`: Cargo declares `ssh`, and the vector function
itself is gated by `cfg(feature = "cli-common")`. Both are enabled only in the
SSH test metadata/library/binary; zero registered vector tests is a failure.
`testkit` requires `armor` and `async`. They read fixture paths
relative to this crate root. Never enable all features on the production library
to make integration tests compile, and never claim omitted or pending
feature-qualified tests as passed. Default and armor-enabled inline unit tests, SSH vectors and
armor/async testkit are mapped here. Async unit tests, SSH unit tests and
benchmarks remain unqualified; this does not claim all upstream features.
The test binaries request normal dependencies plus only additional
same-hub, source-required dev dependencies. i18n-embed is inherited once from
each normal closure; its original test-only desktop/fluent features remain in
the standalone manifest. Explicit extras must not duplicate normal labels.
Benchmark-only criterion, pprof and inferno
must be absent from their actual configured/action closures before qualification.
All upstream benchmark files and original dev dependency requirements remain.

The standalone test lock starts from upstream Git commit
`b5b68c4f4f7ea5c368bc060f6fe0237549c3a13f` Cargo.lock blob
`a0ec1011123ada718ca3edfba4292b8e9a82f9fd`, SHA256
`b0a8b4ae058e50916e3361dc6cd03096b85e2d674bfa4c38605ff9cf4fa8b2b4`.
It is minimally pruned/remapped for this standalone crate and the existing
age-core Git pin. Four explicit existing-application-version test-only
exceptions replace yanked versions and a required companion: chacha20 0.10.2, crypto-bigint
0.7.10, its required ctutils 0.4.6 companion, and wnaf 0.14.1 (whose added
primefield edge uses the existing application version/source/checksum).
The same-version tempfile 3.27.0 edge selects already-present getrandom 0.3.4,
compatible with its original >=0.3,<0.5 requirement. Other original upstream
package identities are preserved; this lock is not byte-identical to upstream.
These test-lock exceptions require Rust 1.87; retained upstream metadata 1.85
has not been validated. Offline metadata used pinned toolchain 1.99.1.

Full standalone metadata includes benchmark-only inferno under CDDL-1.0.
It is a preserved reference dependency, not a standalone license-gate pass.
The existing application cargo-deny scope and license policy are unchanged.
Accepted application metadata parity excludes the standalone dev graph.
Generated Bazel metadata, configured closures and actual compiler inputs still
require separate resolution/audit/build gates; metadata and code checks alone
are not a passing or publishable build candidate.

Rust formatting preserves upstream import style and checks vendor bytes without
writing them. First-party source remains subject to strict formatting. Only
these exact integrity-verified vendor paths receive separate policy; unknown
third-party crates and orphan source remain covered by the normal checks.

The source, resolver metadata, build targets and actual compiler inputs must be
validated together before delivery. Reverting this integration means restoring
the former immutable `age` Git override, removing the local path override,
standalone exclusion and scoped integration/policy additions, and regenerating reviewed lock
metadata. Do not remove `age-core`'s existing pin or unrelated vendor guards.

Refs #65, #35, #18.
