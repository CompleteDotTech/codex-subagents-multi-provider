# AWS configuration security compatibility patch

`aws-config/` retains the crates.io aws-config 1.8.12 source under its original
Apache-2.0 license. `aws-config.upstream.json` pins its archive checksum and every
original file. `aws-config.test-data.json` pins 235 fixtures restored from that
crate's recorded upstream Git revision f031610a8cbb4ac2dac3229bfaeaa88182787ab4.
The fixtures contain public upstream test credentials, not operational secrets.

Only Cargo.toml, Cargo.toml.orig and src/login/dpop.rs differ from the archive.
`aws-config.patch` exposes their complete delta; `aws-config.patched.json` pins
those reviewed results. The source patch moves P-256 to maintained 0.14 and its
RNG to compatible rand 0.10, retaining SEC1 keys, ES256, JWK coordinates and raw
64-byte JWS signatures. OS entropy/signing failures propagate as provider errors.
The dev-test runtime minimum is 1.15.0, whose maintained test-util feature
no longer enables the obsolete HTTP/2 0.3 connector; the full provider suite
remains enabled. Tokio 1.53.3 addresses the separately discovered I/O-safety
advisory. No signing arithmetic is implemented here. Remove this override when upstream
aws-config provides the maintained P-256 path without losing credentials-login.

The local package is a workspace member so its original provider tests and new
signature regression remain executable. Its default features and publishing
metadata are retained; this repository does not publish the vendored crate.
The local BUILD.bazel includes the same authentication features and test data.
Its local rustfmt.toml preserves upstream formatting rather than rewriting the
snapshot according to the first-party edition/import conventions.

Verify offline from the repository root:

```sh
python3 codex-rs/third-party/verify_aws_config.py
```

For an exact patch reconstruction, also pass `--archive /path/aws-config-1.8.12.crate`.
That archive is verified against the pinned registry checksum and read in memory;
the verification never extracts or executes downloaded files.

From codex-rs, validate the authentication path and security closure:

```sh
cargo test --locked -p aws-config --features credentials-login
cargo test --locked -p codex-aws-auth
cargo deny check
```

The entire migration also pins maintained upstream age/age-core at
b5b68c4f4f7ea5c368bc060f6fe0237549c3a13f, updates user-verification to P-256
0.14, and RTC to 0.21.1. Their old elliptic-curve edges required vulnerable
crypto-bigint 0.5; fixing only one would leave the other paths. The resolved
lock uses crypto-bigint 0.7.10 and ctutils 0.4.6. Source trust still requires
immutable revisions and allows only the specific upstream rage repository.

Refs #31.
