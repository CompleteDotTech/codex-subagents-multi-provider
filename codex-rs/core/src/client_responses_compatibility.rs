//! A request-copy adapter for stateless, function-only Responses endpoints.
//!
//! Stored history and tool registrations remain unchanged. This module cannot
//! authorize a tool: execution still resolves the original registered runtime.

use codex_protocol::error::CodexErr;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;

pub(crate) fn unsupported(detail: &str) -> CodexErr {
    CodexErr::InvalidRequest(format!("provider_responses_unsupported: {detail}"))
}

pub(crate) fn wire_tool_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        None | Some(codex_protocol::DEFAULT_FUNCTION_NAMESPACE) => name.to_string(),
        Some(namespace) => format!("{namespace}__{name}"),
    }
}

pub(crate) fn function_tools(tools: &[ToolSpec]) -> Result<Vec<Value>, CodexErr> {
    fn append(
        tool: Value,
        namespace: Option<&str>,
        result: &mut Vec<Value>,
        names: &mut HashSet<String>,
    ) -> Result<(), CodexErr> {
        let kind = tool.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind == "namespace" {
            let namespace = tool["name"]
                .as_str()
                .ok_or_else(|| unsupported("invalid tool namespace"))?;
            for nested in tool["tools"]
                .as_array()
                .ok_or_else(|| unsupported("invalid namespace tools"))?
            {
                append(nested.clone(), Some(namespace), result, names)?;
            }
            return Ok(());
        }
        let name = tool["name"]
            .as_str()
            .ok_or_else(|| unsupported("only function and bridged freeform tools are supported"))?;
        let name = wire_tool_name(namespace, name);
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
            || !names.insert(name.clone())
        {
            return Err(unsupported(
                "tool names cannot be represented unambiguously as functions",
            ));
        }
        let mut function = match kind {
            "function" => tool,
            "custom" => json!({
                "type": "function", "description": tool["description"], "strict": true,
                "parameters": {"type":"object", "properties":{"input":{"type":"string","description":"Exact unmodified raw tool input."}}, "required":["input"], "additionalProperties":false}
            }),
            _ => {
                return Err(unsupported(
                    "server-hosted and tool-search schemas require full Responses support",
                ));
            }
        };
        function["name"] = json!(name);
        // Deferred tool search is not part of this dialect. Advertise the actual
        // callable schema rather than a provider-owned loading protocol.
        if let Some(function) = function.as_object_mut() {
            function.remove("defer_loading");
        }
        result.push(function);
        Ok(())
    }
    let mut result = Vec::new();
    let mut names = HashSet::new();
    for tool in tools {
        append(
            serde_json::to_value(tool).map_err(|_| unsupported("invalid tool schema"))?,
            None,
            &mut result,
            &mut names,
        )?;
    }
    Ok(result)
}

pub(crate) fn portable_history(input: Vec<ResponseItem>) -> Result<Vec<ResponseItem>, CodexErr> {
    let mut result = Vec::with_capacity(input.len());
    let mut calls = HashSet::new();
    let mut completed = HashSet::new();
    for mut item in input {
        item.set_id(None);
        item.clear_internal_chat_message_metadata_passthrough();
        item.clear_content_item_metadata();
        match &mut item {
            // Opaque reasoning is provider-owned state, not portable dialogue.
            ResponseItem::Reasoning { .. } => continue,
            ResponseItem::Message {
                content,
                encrypted_content,
                phase,
                status,
                ..
            } => {
                if encrypted_content.is_some() && content.is_empty() {
                    return Err(unsupported(
                        "encrypted-only messages cannot be replayed statelessly",
                    ));
                }
                *encrypted_content = None;
                *phase = None;
                *status = None;
                for part in content {
                    if matches!(part, ContentItem::InputAudio { .. }) {
                        return Err(unsupported("audio history requires full Responses support"));
                    }
                    if let ContentItem::OutputText {
                        annotations,
                        logprobs,
                        ..
                    } = part
                    {
                        *annotations = None;
                        *logprobs = None;
                    }
                }
            }
            ResponseItem::CustomToolCall {
                call_id,
                name,
                namespace,
                input,
                ..
            } => {
                if !calls.insert(call_id.clone()) || completed.contains(call_id) {
                    return Err(unsupported("duplicate tool call in portable history"));
                }
                item = ResponseItem::FunctionCall {
                    id: None,
                    encrypted_content: None,
                    status: None,
                    name: wire_tool_name(namespace.as_deref(), name),
                    namespace: None,
                    arguments: serde_json::to_string(&json!({"input":input}))
                        .map_err(|_| unsupported("invalid freeform input"))?,
                    encrypted_function_args: None,
                    call_id: call_id.clone(),
                    internal_chat_message_metadata_passthrough: None,
                };
            }
            ResponseItem::FunctionCall {
                call_id,
                name,
                namespace,
                encrypted_content,
                encrypted_function_args,
                status,
                ..
            } => {
                if !calls.insert(call_id.clone()) || completed.contains(call_id) {
                    return Err(unsupported("duplicate tool call in portable history"));
                }
                *name = wire_tool_name(namespace.as_deref(), name);
                *namespace = None;
                *encrypted_content = None;
                *encrypted_function_args = None;
                *status = None;
            }
            ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                if !calls.remove(call_id) {
                    return Err(unsupported(
                        "unpaired or non-text tool result cannot be replayed",
                    ));
                }
                portable_output(output)?;
                completed.insert(call_id.clone());
                item = ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some(call_id.clone()),
                    name: None,
                    namespace: None,
                    output: output.clone(),
                    internal_chat_message_metadata_passthrough: None,
                };
            }
            ResponseItem::FunctionCallOutput {
                call_id,
                output,
                name,
                namespace,
                ..
            } => {
                let Some(call_id) = call_id else {
                    return Err(unsupported("tool result requires a call_id"));
                };
                if !calls.remove(call_id) {
                    return Err(unsupported(
                        "unpaired or non-text tool result cannot be replayed",
                    ));
                }
                portable_output(output)?;
                completed.insert(call_id.clone());
                *name = None;
                *namespace = None;
            }
            _ => {
                return Err(unsupported(
                    "history contains stateful or server-hosted items; start a fresh child",
                ));
            }
        }
        result.push(item);
    }
    if !calls.is_empty() {
        return Err(unsupported("tool history is incomplete"));
    }
    Ok(result)
}

fn portable_output(output: &mut FunctionCallOutputPayload) -> Result<(), CodexErr> {
    if let FunctionCallOutputBody::ContentItems(items) = &output.body {
        if !items
            .iter()
            .all(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
        {
            return Err(unsupported(
                "non-text tool results cannot be replayed as function output",
            ));
        }
        let text = match items.as_slice() {
            [FunctionCallOutputContentItem::InputText { text }] => text.clone(),
            // Preserve ordering, boundaries and exact strings rather than
            // silently joining multiple chunks or dropping content.
            _ => serde_json::to_string(items).map_err(|_| unsupported("invalid tool output"))?,
        };
        output.body = FunctionCallOutputBody::Text(text);
    }
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FreeformArguments {
    input: String,
}

pub(crate) fn decode_freeform(arguments: &str) -> Result<String, String> {
    serde_json::from_str::<FreeformArguments>(arguments)
        .map(|arguments| arguments.input)
        .map_err(|_| {
            "freeform function arguments must contain only a string input field".to_string()
        })
}

#[cfg(test)]
#[path = "client_responses_compatibility_tests.rs"]
mod tests;
