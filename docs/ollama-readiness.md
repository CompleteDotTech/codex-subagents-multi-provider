# Manual Ollama qualification

`scripts/qualify_ollama.py` is an explicit Linux/WSL manual validator for the existing
`gemma4:31b-cloud` Ollama route at `http://127.0.0.1:11434/v1`. It uses the fork's
native CLI, its actual model session and a streamed stateless Responses function
tool/result round trip. It never pulls a model or changes services or global
configuration. Python 3.11 or later and a source-identified native Linux package
with its normal helper resources are required. CI runs deterministic tests only.
The default invocation validates identities without starting processes or making
network requests. Only `--run-live` permits inference; this cloud tag may incur
provider charges and requires existing Ollama cloud access.

## Inputs and provenance

Review the native build receipt independently. The validator verifies agreement
between operator-pinned SHA-256 manifests; matching hashes do not independently
attest that a binary was built from a claimed source. Keep manifests and catalogs
in a host-controlled location outside child-writable scopes. A directory name or
being outside enumerated roots does not establish a read-only trust boundary.

Supply a source manifest with `source_sha` (40 lowercase hexadecimal characters),
`source_tree` (the Git tree identity), and `target` (`x86_64-unknown-linux-gnu`).
The package manifest must repeat those fields and contain `files`, mapping each
of `bin/codex`, `bin/codex-code-mode-host`, `codex-resources/bwrap`, `codex-path/rg`,
and `codex-package.json` to `{ "sha256": "...", "bytes": 123 }`. Paths are relative
to the supplied package root; resolved resources must remain inside that root.
Record the build command, clean source state, native toolchain, native SDK inputs
and `STABLE_GIT_COMMIT` separately with the build receipt. The CLI version string
alone does not establish commit identity.

The reviewed example catalog and source facts are in `docs/examples/ollama/`.
The catalog is loaded through normal `model_catalog_json` configuration and the
native typed `ModelsResponse` loader. It never supplies the internal fallback
marker. The facts file pins the catalog hash. Provider metadata advertised context
262144 and completion/thinking/tools/vision on Ollama 0.21.1. The request tag is
`gemma4:31b-cloud`; observed responses identify its parent `gemma4:31b`.
This is cloud-backed coverage, not downloaded local weights or offline inference.
Metadata is an observation, not a permanent provider guarantee.

The explicit catalog conservatively enables text and the observed direct function
shell tool path. Empty reasoning effort levels and disabled search, verbosity,
experimental context, Node and additional tools are host policy; they do not
assert that the provider lacks those capabilities. Generic first-party agent
instructions are host prompting policy, not vendor model facts. The configured
context window is advertised metadata, not a capacity or compaction stress test.
No vision, reasoning effort, parallel tools or remote compaction is qualified.

For example, from a Linux shell, replace every placeholder with independently
reviewed paths and hashes, and create an empty output parent directory first:

```sh
python3 scripts/qualify_ollama.py \
  --package "$PACKAGE" --binary "$PACKAGE/bin/codex" --binary-sha "$BINARY_SHA256" \
  --package-manifest "$PACKAGE_MANIFEST" --package-manifest-sha "$PACKAGE_MANIFEST_SHA256" \
  --source-manifest "$SOURCE_MANIFEST" --source-manifest-sha "$SOURCE_MANIFEST_SHA256" \
  --catalog docs/examples/ollama/gemma4-catalog.json --catalog-sha "$CATALOG_SHA256" \
  --catalog-facts docs/examples/ollama/source-facts.json --catalog-facts-sha "$FACTS_SHA256" \
  --artifact-root "$OUTPUT_PARENT"
```

After inspecting the validated identities, add `--run-live` to the same command
for one explicitly authorized probe. The validator creates a fresh temporary
home/workspace with no copied auth, user configuration, history or environment
credentials. It passes the catalog and provider process-locally, retains the
native read-only sandbox and disables web search and multi-agent tools. Do not
persist an Ollama-only root catalog for an OpenAI parent. For native child use,
configure the reviewed host-controlled catalog under the existing
`agents.provider_defaults.ollama.model_catalog_json` and model entry, preserving
other provider defaults and allowed providers.

## Evidence and bounds

At most four POSTs are forwarded, with 150 seconds for the CLI and bounded teardown
within a 180-second qualification limit. The proxy only accepts fixed Responses
and model-list paths, rejects credential-bearing and unexpected header names before upstream contact,
and forwards only Content-Type/Accept plus identity encoding. It never forwards
OpenAI or Z.ai auth. The native process environment is an explicit minimal
whitelist. Body size is capped at 4 MiB. Cleanup targets only its own CLI process
group, proxy and tracked upstream connections. No unrelated process is changed.

Success requires matching native ELF/process hashes, two completed streamed
responses, a real successful `cat -- input.txt`, an initially unseen UUID in the
matching function output and exact final answer, unchanged input/catalog, no
fallback, off-scope tools, terminal errors or cleanup errors. The command/path
restriction is a post-run qualification audit in addition to the read-only
sandbox; the proxy does not proactively block shell commands. A model departing
from the requested benign read fails qualification. The validator is not a
security boundary for arbitrary untrusted model actions.

SSE parsing ends at the first complete successful semantic terminal frame, not a
substring or transport EOF. Prior failed/incomplete/error events cannot be
overridden by later completion. Raw received bytes are retained, including any
partial post-terminal tail in the final read; its byte count/hash are recorded.
Malformed or truncated pre-terminal frames fail. Reader-owned cleanup avoids
closing a response underneath its active reader.

A new timestamped output directory preserves JSONL, stderr, raw SSE, wire events,
identities and a result receipt; temporary isolated inputs are retained for audit.
Receipts may contain local paths and model output: review and sanitize before
publication. A failed receipt remains failed even when a later probe succeeds.
After review, remove only the exact owned temporary/output directories listed in
the receipt, once no process is using them. No automatic cleanup deletes evidence.

This qualifies the bounded provider protocol and tool path only. It does not
establish installed delivery, Windows inference, mixed-provider child orchestration,
or the epic's final acceptance. The historical qualification ELF came from
accepted source `dd3a1a72259a177632fb782029de2c13ce721fd5`; changes in this delivery
are validator/docs/CI plus a test-only import-format correction. That ELF must not
be described as rebuilt from this validator revision.

Run the no-provider regressions with:

```sh
python3 -m unittest discover -s .github/scripts -p 'test_qualify_ollama.py'
```
