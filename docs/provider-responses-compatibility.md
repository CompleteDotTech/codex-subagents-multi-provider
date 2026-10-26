# Responses compatibility for configured providers

Provider capabilities limit request formats; they do not grant tool access,
filesystem access, or approval authority. Existing providers keep the full
Responses dialect unless their trusted provider definition declares otherwise:

```toml
[model_providers.compatible.capabilities]
responses = "stateless_functions"
```

The built-in `ollama` provider uses this profile. Any configured provider may
select it, independently of its name or model slug. The profile uses HTTP with
complete portable history for each request; it never uses WebSocket continuation,
`previous_response_id`, conversation state, remote compaction, encrypted reasoning
replay, or hosted web/image tools. Model-declared reasoning effort remains
available; reasoning summaries, internal metadata, prompt-cache keys, and text
format controls are not sent. Structured-output requests and stateful history
items that cannot be represented safely fail explicitly before credentials or
transport setup. Start a fresh child when stored history contains incompatible
provider-owned state.

Function declarations remain ordinary function tools. Namespaced tools receive a
deterministic `namespace__name` wire name; unrepresentable or colliding names are
rejected. A freeform tool is advertised as a function whose only argument is the
required string `input`. The exact raw string reaches its original registered
handler before normal hooks, argument checks, sandboxing and approvals. Extra,
missing or incorrectly typed wrapper fields are rejected. Tool calls and results
are replayed as paired function items; incomplete pairs and unsupported non-text
tool results fail explicitly. Text-only content arrays retain an exact single
text string or an ordered JSON array of all original text chunks. Native Code
Mode calls keep their registered names and payloads; only external wire calls use
alias and wrapper decoding. The adapter modifies request copies, not stored
history, registrations or another session's configuration.

Malformed SSE, unfinished streamed tool calls, structured errors carried in HTTP
200, and incomplete responses cannot become successful completions. Existing
provider-scoped retry and cancellation mechanisms remain responsible for recovery;
the adapter does not introduce another retry loop or select another endpoint.

[Ollama's official compatibility documentation](https://docs.ollama.com/api/openai-compatibility)
documents non-stateful Responses and function tools, and excludes custom/freeform
tool-call replay for its cloud endpoint. The profile bridges freeform execution
through functions instead of replaying an unsupported custom wire item. This
describes protocol behavior; mock conformance tests do not establish live model,
tool, account-quota, or offline-inference acceptance.
