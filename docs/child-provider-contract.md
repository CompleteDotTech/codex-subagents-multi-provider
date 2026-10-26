# ADR: trusted provider selection for native child agents

Status: accepted implementation contract for issues #8–#23; not a claim that the
runtime or live-provider acceptance is implemented. Refs #8, #2, #1.

Baseline: `rust-v0.164.0`, commit
`6aaac694803004a63c6f393d153c96d22521a6b9`.

## Scope and invariants

Any user-delegated ordinary/default, built-in-role, custom-role, or nested child
can select a host-authorized configured provider. The OpenAI parent need not
change profile. The same resolver governs native V1/V2 spawning and supported
app-server entry points; direct internal spawn/reload paths must not bypass it.
Host-owned safety, review, and control agents retain their host policy and cannot
be rerouted by user-delegated settings. Parent and sibling configuration objects,
credentials, sessions, catalogs, and caches remain independent.

This is an opt-in extension. With no provider selector or new policy fields,
children inherit the invoking parent's effective provider and existing role,
model, reasoning, permission, and history rules remain intact.

## Configuration and API

The following are the new, exact field names. Implementations must add them to
configuration schema and generated protocol types before advertising support.
`agents.allowed_model_providers`, `agents.default_subagent_model_provider`, and
`agents.provider_defaults` are reserved settings, not flattened role names.

```toml
# Trusted user/host config.toml only. Existing OpenAI authentication stays in
# its existing credential store; this file contains no credential values.
model_provider = "openai"

[agents]
allowed_model_providers = ["openai", "zai", "ollama"]
# Optional: absent here, so an ordinary child inherits its immediate parent.
# default_subagent_model_provider = "zai"

[agents.provider_defaults.zai]
model = "glm-5.3"
model_catalog_json = "C:/Users/timot/.codex/catalogs/zai.json"

[agents.provider_defaults.ollama]
model = "gemma4:31b-cloud"
model_catalog_json = "C:/Users/timot/.codex/catalogs/ollama.json"

[model_providers.zai]
name = "Z.ai"
base_url = "https://api.z.ai/api/v1"
wire_api = "responses"
env_key = "ZAI_KEY" # Existing host credential source supplies the value.
```

`ollama` is the existing built-in provider ID, not a new entry under
`model_providers` (reserved built-in IDs cannot be redefined). Its existing
host-owned endpoint configuration supplies the local server address. Named
configured providers also work: authorization is by configured ID, never model
name or a hard-coded Z.ai role. Catalog paths resolve relative to the trusted
file that declares them; a catalog URL in a trusted provider definition is an
alternative discovery source. A provider default entry does not authorize its ID.

An optional custom role file may contain `model_provider = "zai"` and
`model = "glm-5.3"`. It chooses only from the host registry; it never supplies
provider definitions, endpoints, authentication, or catalog paths. Ordinary
children require no role file:

```json
{"message":"Use a benign workspace tool and report the result.","task_name":"zai-check","model_provider":"zai","model":"glm-5.3","fork_turns":"none"}
```

This example is the V2 `spawn_agent` argument shape. For V1 omit `task_name` and
`fork_turns`, and use its existing `fork_context: false`. An ordinary Ollama
child uses `model_provider: "ollama"`, its qualified model, and fresh history.
Omitting `model` uses the target's trusted provider default when switching
provider. Existing role selectors remain `agent_type`. App-server request
fields use the protocol's generated camelCase spelling `modelProvider`; native
tool arguments and role TOML use `model_provider`. All selectors are optional
strings containing an ID, never a provider object. Resume selects the persisted
provider; it does not expose an implicit provider-switch argument.

## Resolution order

Authorization and history checks precede inference, remote catalog fetching,
credential helper execution, or starting a child. Resolve the final tuple once:
provider ID and trusted definition, model, catalog identity, capabilities,
reasoning, service tier, transport, and authentication policy. Validate final
settings against that tuple, not the parent's models manager.

| Setting | Highest to lowest priority |
| --- | --- |
| Provider | Explicit spawn `model_provider`; applicable role `model_provider`; trusted `agents.default_subagent_model_provider`; invoking parent's effective provider |
| Model, same provider | Applicable role `model`; explicit spawn `model`; existing `agents.default_subagent_model`; captured invoking-step model |
| Reasoning, same provider | Applicable role effort; explicit spawn effort; existing default subagent effort; captured invoking-step effort |
| Model, changed provider | Applicable role `model`; explicit spawn `model`; trusted target `agents.provider_defaults.<id>.model`; otherwise fail |
| Reasoning, changed provider | Applicable role effort; explicit spawn effort; existing default subagent effort if supported by target; target model default |

The model/effort order preserves `child_config.rs`: role projection follows
explicit/default overrides. Provider order intentionally differs: an explicit
provider wins over the role. Thus a role's incompatible model can cause an error
even when an explicit provider is valid; do not silently discard role settings.
An explicit model equal in spelling to the parent's is valid only when it is
also in the target's authoritative catalog. A provider change never implicitly
inherits the parent's model, global default subagent model, effort, context
limits, model-owned prompts, service-tier capability, or catalog metadata.

An applicable role means exactly what the selected native history/version path
already applies: fresh V1/V2 apply the requested/default role; V1 full history
rejects an explicit role and does not apply a default role; V2 full history
applies an explicit role but skips an implicit default role. A nested child uses
its immediate parent's effective provider as the last fallback, not the root's.
A configured child provider default applies at each nesting depth.

Explicit or role-selected unsupported reasoning/service tier is an error.
Existing inherited controller tier behavior may omit a tier the target does not
support; preserve the existing `default` sentinel behavior, without inventing a
paid tier. All emitted tiers must be supported by target capabilities. Resolve
same-slug models separately for each provider; never use fallback OpenAI metadata
to validate a switched provider. Missing required target metadata is an error.

## Trust and authority

Construct a separate trusted provider registry and child policy from managed,
system, and user layers, with existing precedence within those layers. Do not
reuse the fully merged project/session provider map for child switching. Managed
requirements are an intersection/constraint, never broadened by lower layers.
An exact managed required provider rejects a conflicting selector, including a
conflicting configured default; it does not silently rewrite the request.

| Source | Select provider ID | Authorize IDs / define provider, endpoint, credential helper, catalog |
| --- | --- | --- |
| Managed/system/user configuration | Yes | Yes, subject to higher managed constraints |
| Project configuration or project-discovered role | Only an authorized ID | No |
| User role file / built-in role | Only an authorized ID | No; role projection stays bounded even if the file is user-owned |
| Model-generated spawn / ordinary CLI/session override | Only an authorized ID | No; cannot extend host allowlist or replace registry definitions |
| Persisted child metadata | Restore identity subject to current policy | No |
| Host-owned control agent | Only as its host policy permits | Delegated settings do not apply |

Absent/empty `allowed_model_providers` allows same-provider inheritance and
same-provider explicit selection only. Switching requires target membership in
the trusted allowlist. At every use, the target must exist and satisfy current
managed policy. Inheritance never bypasses policy revocation. An allowed ID does
not authorize arbitrary configuration attached to that ID: project/session
shadowing of `model_providers.zai`, built-in endpoint overrides, catalog paths,
capabilities, or auth headers must not replace its trusted definition. Definitions
and catalog contents used by a child must be captured from trusted provenance;
relative paths or user-owned configuration pointing into project-controlled
files do not establish trust in those files. Reject untrusted catalog/definition
sources for switching rather than follow them.

Ignore forbidden role authority fields as the existing bounded projection does;
retain hostile-role tests for sandbox, approval, managed policy binding, MCP,
apps, feature restrictions, and provider definitions. When a role actually
requests a disallowed provider ID, fail explicitly at the resolver. A role's
ignored authority fields must never be projected into session flags. Requested
provider changes do not grant tools, alter workspace access, weaken approvals,
or bypass permission prompts or managed network policy.

## History and lifecycle matrix

| Native operation | Same provider | Changed provider |
| --- | --- | --- |
| V1 fresh (`fork_context: false` / absent) | Existing behavior | Supported with resolved target tuple |
| V1 full (`fork_context: true`) | Existing behavior, including role restriction | Reject before network: use fresh child and an explicit textual task summary |
| V2 fresh (`fork_turns: "none"`) | Existing behavior | Supported with resolved target tuple |
| V2 full (`fork_turns: "all"` / absent) | Existing behavior, including prefix handling | Reject before network: explicitly choose `"none"` |
| V2 numeric legacy `fork_turns` | Existing full-history alias, not partial history | Same full-history rejection |
| Partial/selected-turn history | No new partial mode in this change; reject unsupported API modes | Reject unsupported API modes; never pretend a full fork is a partial one |
| Warm/cold resume, eviction/reload | Restore stored provider/model and revalidate current authority | No implicit switch during resume; explicit migration is unsupported |
| Steering, result collection, cancellation | Existing child identity and routing | Route to that child's resolved provider; parent remains unchanged |

Cross-provider full-history rejection is deliberate: foreign response IDs,
opaque reasoning, tool declarations, and freeform tool records cannot safely be
replayed merely because the model names match. A fresh child can receive a
plain-text task summary as user input, not fabricated historical authorization.
Same-provider Ollama history is still subject to its stateless serializer: send
portable client-maintained context, strip server continuation IDs and opaque
reasoning, and reject unsupported tool records rather than blindly replay them.
Apply the same validation to compacted replacement histories and resumed
rollouts. Local compaction uses the child's model/capabilities; remote compaction
is allowed only when that provider advertises the supported protocol.

Persist only nonsecret provider ID, model ID, effective identity/provenance
version, and history compatibility information needed to reconstruct the child.
Do not serialize tokens, auth headers, resolved helper output, or full provider
objects into child metadata. Reconstruct transport/auth from current trusted
configuration on reload; recheck revocation before sending. If a trusted
definition's endpoint or compatibility changes, invalidate cached transport and
history compatibility; refuse unsafe replay instead of treating the old ID as
proof of compatibility. Never recreate a missing provider as OpenAI. Permission
and managed-policy revalidation also applies to warm resume.

## Provider and transport requirements

| Provider | Catalog / authentication owner | Required compatibility and evidence |
| --- | --- | --- |
| OpenAI | Trusted host discovery/catalog; existing OpenAI auth store | Existing stream/tool behavior; parent turn succeeds after other-provider children |
| Z.ai (example configured ID `zai`) | Trusted provider catalog/default; existing `ZAI_KEY` environment/helper | Bounded real Responses function-tool call, result consumption, and completion; quota errors remain blockers |
| Built-in Ollama or configured compatible endpoint | Trusted host endpoint/catalog; no foreign bearer credentials | Stateless Responses, streamed function tools and result consumption; exact server/model recorded |

Catalog/cache keys include provider identity and trusted definition/catalog
fingerprint, model ID, and relevant auth/tenant scope. Equal model slugs on two
providers do not share metadata, context windows, reasoning, connection pools,
auth refresh, server response IDs, or compaction state. A child gets only its
selected provider's credential source. An unauthenticated Ollama endpoint must
never receive OpenAI or Z.ai tokens, cookies, actor headers, or helper output.
Retries remain on the selected provider and respect existing retry limits.

For stateless Responses, retain portable messages and complete supported
function call/output pairs; omit `previous_response_id`, unsupported stateful
continuation, opaque reasoning, and unsupported storage fields. Native
custom/freeform tools must be converted losslessly to an explicitly supported
function schema with matching results, or the affected request/history must fail
before transmission. Text-only output or HTTP 200 is not tool acceptance.
Unsupported server-hosted tools are excluded by capability policy or rejected;
never silently run a different tool. `gemma4:31b-cloud` is a cloud-backed example:
a loopback Ollama endpoint does not prove local or offline inference.

## Failures and observability

Error codes below are stable resolver categories; adapters may add context but
must retain the category and sanitized provider/model IDs. Failures before
network must not execute a credential helper or attempt an alternate endpoint.

| Condition | Category / result |
| --- | --- |
| Unknown provider ID | `child_provider_unknown`; list how to configure a host ID |
| Switching not allowed, disabled, or revoked | `child_provider_not_authorized`; identify required host authorization |
| Exact managed-provider conflict / control-agent override | `child_provider_policy_conflict`; preserve existing required policy |
| Definition/catalog from untrusted shadow layer | `child_provider_untrusted_source`; identify the offending layer, without secrets |
| Changed provider without eligible model/default | `child_provider_model_required`; request explicit model or trusted target default |
| Model missing from authoritative catalog / missing metadata | `child_provider_model_unavailable`; do not use parent fallback metadata |
| Invalid effort / explicit tier / unsupported tools | `child_provider_capability_unsupported`; identify rejected setting |
| Cross-provider full fork / unsafe resumed history | `child_provider_history_unsupported`; suggest fresh child |
| Missing/expired credentials | `child_provider_auth_unavailable`; preserve selected ID, give sanitized source guidance |
| Rate limit / quota / upstream failure | Existing provider error plus selected ID; bounded retry only, no provider fallback |
| Persisted provider absent or newly prohibited | Fail resume with the relevant category; preserve resumable metadata |

Status/events expose effective `model_provider_id` and model, including after
spawn, reload, resume, cancellation and failure. App-server uses generated
`modelProvider` identity fields where that surface uses camelCase. Report the
resolved child rather than the requested or parent's ID; retain existing hidden
metadata/privacy settings. Diagnostics never include auth values, helper output,
full credential-bearing URLs, or headers. Failed resolution must not emit a
misleading started/success event.

## Implementation and acceptance map

- #9: typed fields, trusted-layer provenance, shared resolver, managed-policy and
  hostile-input tests, schema generation. Preserve omitted-field compatibility.
- #10–#12: provider-scoped model managers, auth/transport isolation, stateless
  Responses and function-tool handling; test same model slug with different
  metadata and distinct endpoint credentials.
- #13–#16: all native entry points, history gates, persistence/revalidation,
  cancellation, and effective identity. Test ordinary, role, and nested children,
  explicit override versus immediate-parent inheritance, and unchanged siblings.
- #17–#18: deterministic three-endpoint positive/negative regression suites,
  denial-before-network assertions, real build/CI on the exact reviewed revision.
- #19–#20: identifiable native artifacts, documented host-only setup and rollback,
  Windows/WSL routing; no implied stock-desktop binary override.
- #21–#23: bounded live readiness and installed function-tool/result workflows for
  Z.ai and Ollama, nested overrides, resume/history/cancel paths, and subsequent
  OpenAI parent turn. Separate mocks, HTTP access, real tool use, local inference,
  and cloud inference evidence. Keep external quota blockers open.

Source anchors at the pinned revision: `codex-rs/core/src/agent/child_config.rs`
(resolution and existing precedence), `agent/role.rs` and `agent/role_tests.rs`
(bounded projection), `agent/control/spawn.rs` (history/reload),
`core/src/tools/handlers/multi_agents_v2/spawn.rs` (numeric full-history alias),
`config/src/config_toml.rs` (flattened roles),
`config/src/model_provider_requirements.rs` (managed definitions), and
`model-provider-info/src/lib.rs` (built-in IDs and provider authentication).
