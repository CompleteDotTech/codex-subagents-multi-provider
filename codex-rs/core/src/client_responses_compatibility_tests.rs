use super::*;
use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;

fn item(value: Value) -> ResponseItem {
    serde_json::from_value(value).unwrap()
}

#[test]
fn freeform_bridge_preserves_exact_input_and_rejects_extra_fields() {
    let patch = "*** Begin Patch\n*** Add File: x\n+雪 \\\"\n*** End Patch\n";
    assert_eq!(
        decode_freeform(&json!({"input":patch}).to_string()).unwrap(),
        patch
    );
    for invalid in [
        json!({}),
        json!({"input":4}),
        json!({"input":"patch","bypass":true}),
    ] {
        assert!(decode_freeform(&invalid.to_string()).is_err());
    }
    let tools = function_tools(&[ToolSpec::Freeform(FreeformTool {
        name: "apply_patch".into(),
        description: "Patch".into(),
        defer_loading: None,
        format: FreeformToolFormat {
            r#type: "grammar".into(),
            syntax: "lark".into(),
            definition: "start: TEXT".into(),
        },
    })])
    .unwrap();
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["parameters"]["additionalProperties"], false);
    assert_eq!(tools[0]["parameters"]["required"], json!(["input"]));
}

#[test]
fn portable_history_bridges_calls_and_removes_foreign_state_without_mutating_source() {
    let original = vec![
        item(
            json!({"type":"reasoning","id":"rs_old","summary":[],"encrypted_content":"opaque-foreign"}),
        ),
        item(
            json!({"type":"custom_tool_call","id":"ctc_old","call_id":"call1","name":"apply_patch","input":"raw\npatch"}),
        ),
        item(json!({"type":"custom_tool_call_output","call_id":"call1","output":"result"})),
        item(
            json!({"type":"message","id":"msg_old","role":"assistant","content":[{"type":"output_text","text":"done","annotations":[]}]}),
        ),
    ];
    let source = serde_json::to_value(&original).unwrap();
    let portable = serde_json::to_value(portable_history(original.clone()).unwrap()).unwrap();
    assert_eq!(portable.as_array().unwrap().len(), 3);
    assert_eq!(portable[0]["type"], "function_call");
    assert_eq!(
        serde_json::from_str::<Value>(portable[0]["arguments"].as_str().unwrap()).unwrap(),
        json!({"input":"raw\npatch"})
    );
    assert_eq!(portable[1]["type"], "function_call_output");
    assert_eq!(portable[1]["call_id"], "call1");
    assert!(!portable.to_string().contains("opaque-foreign"));
    assert!(
        portable
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item.get("id").is_none())
    );
    assert_eq!(serde_json::to_value(original).unwrap(), source);
}

#[test]
fn portable_history_rejects_unpaired_duplicate_or_stateful_history() {
    for input in [
        vec![item(
            json!({"type":"function_call_output","call_id":"missing","output":"result"}),
        )],
        vec![item(
            json!({"type":"function_call","call_id":"c","name":"f","arguments":"{}"}),
        )],
        vec![
            item(json!({"type":"function_call","call_id":"c","name":"f","arguments":"{}"})),
            item(json!({"type":"function_call","call_id":"c","name":"f","arguments":"{}"})),
        ],
        vec![item(
            json!({"type":"compaction","encrypted_content":"foreign"}),
        )],
    ] {
        assert!(portable_history(input).is_err());
    }
}

#[test]
fn function_tools_reject_hosted_tools_and_ambiguous_namespace_aliases() {
    assert!(
        function_tools(&[ToolSpec::WebSearch {
            external_web_access: Some(false),
            indexed_web_access: None,
            filters: None,
            user_location: None,
            search_context_size: None,
            search_content_types: None,
        }])
        .is_err()
    );
    let function = codex_tools::ResponsesApiTool {
        name: "x".into(),
        description: "x".into(),
        strict: true,
        defer_loading: None,
        parameters: serde_json::from_value(
            json!({"type":"object","properties":{},"additionalProperties":false}),
        )
        .unwrap(),
        output_schema: None,
    };
    let namespace = ToolSpec::Namespace(codex_tools::ResponsesApiNamespace {
        name: "group".into(),
        description: String::new(),
        tools: vec![codex_tools::ResponsesApiNamespaceTool::Function(
            function.clone(),
        )],
    });
    let mut conflicting = function;
    conflicting.name = "group__x".into();
    assert!(function_tools(&[namespace, ToolSpec::Function(conflicting)]).is_err());
}
