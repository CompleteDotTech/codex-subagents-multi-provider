//! Stateless wire adaptation must not rewrite native Code Mode invocations.
use super::*;
use codex_model_provider_info::ModelProviderCapabilities;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::ResponsesCompatibility;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;

struct NamespacedEcho {
    calls: Arc<AtomicUsize>,
}

impl ToolContributor for NamespacedEcho {
    fn tools(
        &self,
        _session: &ExtensionData,
        _thread: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        vec![Arc::new(Self {
            calls: self.calls.clone(),
        })]
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for NamespacedEcho {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced("audit", "echo")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name:"audit".into(),description:"Audit fixture".into(),
            tools:vec![ResponsesApiNamespaceTool::Function(codex_tools::ResponsesApiTool {
                name:"echo".into(),description:"Return the supplied value".into(),strict:true,defer_loading:None,
                parameters:serde_json::from_value(serde_json::json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false})).unwrap(),output_schema:None,
            })],
        })
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            if !matches!(&call.source, codex_tools::ToolCallSource::CodeMode { .. }) {
                return Err(FunctionCallError::Fatal(
                    "expected an internal Code Mode call".into(),
                ));
            }
            let ToolPayload::Function { arguments } = call.payload else {
                return Err(FunctionCallError::Fatal(
                    "expected native function arguments".into(),
                ));
            };
            let arguments: Value = serde_json::from_str(&arguments)
                .map_err(|_| FunctionCallError::RespondToModel("invalid JSON".into()))?;
            if arguments.as_object().is_none_or(|args| args.len() != 1)
                || !arguments["value"].is_string()
            {
                return Err(FunctionCallError::RespondToModel(
                    "only a string value is permitted".into(),
                ));
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(JsonToolOutput::new(serde_json::json!({
                "namespace":call.tool_name.namespace,"name":call.tool_name.name,"value":arguments["value"],
            }))) as Box<dyn ToolOutput>)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stateless_code_mode_keeps_nested_freeform_and_namespaced_function_calls_native()
-> Result<()> {
    let server = responses::start_mock_server().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_contributor(Arc::new(NamespacedEcho {
        calls: calls.clone(),
    }));
    extensions.tool_contributor(Arc::new(NamespacedCustomTool {
        generation: 0,
        generations: Arc::new(AtomicUsize::new(0)),
    }));
    let base_url = format!("{}/v1", server.uri());
    let mut builder = test_codex()
        .with_model("portable-code-mode")
        .with_model_info_override("portable-code-mode", |model| {
            model.tool_mode = Some(ToolMode::CodeMode);
            model.use_responses_lite = false;
        })
        .with_extensions(Arc::new(extensions.build()))
        .with_config(move |config| {
            config.features.enable(Feature::CodeMode).unwrap();
            config.features.disable(Feature::ToolSearch).unwrap();
            config.model_provider_id = "configured-stateless-code".into();
            config.model_provider = ModelProviderInfo {
                name: "configured-stateless-code".into(),
                base_url: Some(base_url),
                supports_websockets: true,
                capabilities: Some(ModelProviderCapabilities {
                    responses: Some(ResponsesCompatibility::StatelessFunctions),
                    ..Default::default()
                }),
                request_max_retries: Some(0),
                stream_max_retries: Some(0),
                ..Default::default()
            };
        });
    let test = builder.build_with_auto_env(&server).await?;
    let patch =
        "*** Begin Patch\n*** Add File: native-code-mode.txt\n+real nested patch\n*** End Patch\n";
    let code = format!(
        r#"
await tools.apply_patch({patch:?});
let denied=false;
try {{ await tools.audit__echo({{ value:"invalid", extra:true }}); }} catch (_) {{ denied=true; }}
const echo=await tools.audit__echo({{ value:"native namespace" }});
text(JSON.stringify({{ echo, denied, patchCompleted:true }}));
"#
    );
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("direct-editor"),
                responses::ev_function_call(
                    "editor-direct",
                    "editor__apply_patch",
                    &serde_json::json!({"input":"direct raw patch\n"}).to_string(),
                ),
                ev_completed("direct-editor"),
            ]),
            sse(vec![
                ev_response_created("cell"),
                responses::ev_function_call(
                    "cell-call",
                    "exec",
                    &serde_json::json!({"input":code}).to_string(),
                ),
                ev_completed("cell"),
            ]),
            sse(vec![
                ev_assistant_message("answer", "done"),
                ev_completed("complete"),
            ]),
        ],
    )
    .await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Use Code Mode to patch and invoke the namespaced tool".into(),
            text_elements: vec![],
        }]))
        .await?;
    let event = wait_for_event_with_timeout(
        &test.codex,
        |event| matches!(event, EventMsg::TurnComplete(_)),
        Duration::from_secs(30),
    )
    .await;
    let EventMsg::TurnComplete(completed) = event else {
        unreachable!()
    };
    assert!(
        completed.error.is_none(),
        "Code Mode turn failed: {:?}",
        completed.error
    );
    assert_eq!(
        tokio::fs::read_to_string(test.cwd.path().join("native-code-mode.txt")).await?,
        "real nested patch\n"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "only validated native invocation executes"
    );
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    let direct: Value = serde_json::from_str(
        &requests[1]
            .function_call_output_text("editor-direct")
            .expect("bridged namespaced custom output"),
    )?;
    assert_eq!(direct["namespace"], "editor");
    assert_eq!(direct["name"], "apply_patch");
    assert_eq!(direct["input"], "direct raw patch\n");
    let body = requests[2].body_json();
    assert!(body.get("previous_response_id").is_none());
    assert!(
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["type"] == "function")
    );
    let output = requests[2]
        .function_call_output_text("cell-call")
        .expect("portable Code Mode result");
    let items: Vec<Value> = serde_json::from_str(&output)?;
    let native = items
        .iter()
        .filter_map(|item| item["text"].as_str())
        .find(|text| text.contains("native namespace"))
        .expect("nested function output");
    let native: Value = serde_json::from_str(native)?;
    assert_eq!(
        native,
        serde_json::json!({"echo":{"namespace":"audit","name":"echo","value":"native namespace"},"denied":true,"patchCompleted":true})
    );
    Ok(())
}
