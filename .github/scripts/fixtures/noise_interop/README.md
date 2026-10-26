This regression runs unmodified upstream Clatter 2.2.0 and maintained Clatter 3
revision 9a8d15c4f80d5911ca0403aa22e0de99ea59df08 in separate processes.
Their Cargo dependency graphs cannot share one resolver because the former needs
kem 0.3.0-pre.0 while the latter uses kem 0.3 stable. The old fixture pins ML-KEM 0.2.3
(the working old graph), which fixes its dependency to that prerelease.

Run explicitly after installing the repository Rust toolchain:

```sh
python3 .github/scripts/test_noise_interop.py --cargo cargo
```

The runner creates temporary Cargo projects outside the workspace, copies the
exact manifest/lock/source fixtures, and builds with --locked and two jobs.
--work-dir can retain the generated projects for inspection. CARGO_TARGET_DIR
is honored. The runner selects the exact executable from Cargo JSON artifact
messages, so configured target directories cannot select a stale guessed binary.
A native host toolchain is required to execute both artifacts; a cross target
that cannot run fails explicitly. Normal dependency fetch access is needed; no API credentials,
external authentication service, or paid call is used.

One shared test-only endpoint uses upstream public APIs directly. Both old/new
role combinations authenticate the initiator static identity, carry the registry
authorization handshake payload, finish the empty responder handshake, exchange
encrypted application records in both directions, and reject a modified next
record. This is provider wire interoperability evidence; the production
exec-server test suite separately exercises the actual channel and relay paths.
It does not copy cryptographic math or replace the production channel tests.

Public ML-KEM-768 keys remain 1184 bytes and ciphertexts 1088 bytes. The upstream 3
adapter stores a 64 byte private seed instead of the old expanded 2400 byte key;
NoiseChannelIdentity keeps this private in memory and serializes only public
material. The production suite string remains
Noise_hybridIK_X25519+MLKEM768_AESGCM_SHA256. Each run prints exact fixture SHA256
hashes. Separate locks retain the precise old/new provider dependency graphs.

The old lock is seeded from accepted d18506a; the new lock is seeded from the
current production lock. Cargo metadata trims each graph offline, retaining
production provider versions instead of resolving unrelated latest releases.
