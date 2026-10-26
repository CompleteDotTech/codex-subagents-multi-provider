//! Wire aliases must resolve before production runtime scheduling and lifecycle lookup.
use super::*;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolRouter;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_extension_api::ToolDispatchInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_model_provider_info::ModelProviderCapabilities;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::ResponsesCompatibility;
use codex_protocol::openai_models::ToolMode;
use codex_tools::ToolName;
use futures::FutureExt;
use futures::future::BoxFuture;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Default)]
struct Gates {
    waiting: Notify,
    ready: Notify,
    entered: Notify,
    cancelled: Notify,
    finish: Notify,
    terminal: AtomicBool,
    executions: AtomicUsize,
}

struct NamespacedRuntime {
    name: ToolName,
    gates: Arc<Gates>,
    finishes: bool,
}
impl ToolExecutor<ToolInvocation> for NamespacedRuntime {
    fn tool_name(&self) -> ToolName {
        self.name.clone()
    }
    fn spec(&self) -> codex_tools::ToolSpec {
        let function = codex_tools::ResponsesApiTool {
            name: self.name.name.clone(),
            description: "Lifecycle fixture".into(),
            strict: true,
            defer_loading: None,
            parameters: serde_json::from_value(
                serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
            )
            .unwrap(),
            output_schema: None,
        };
        match &self.name.namespace {
            Some(namespace) => {
                codex_tools::ToolSpec::Namespace(codex_tools::ResponsesApiNamespace {
                    name: namespace.clone(),
                    description: "Lifecycle fixture namespace".into(),
                    tools: vec![codex_tools::ResponsesApiNamespaceTool::Function(function)],
                })
            }
            None => codex_tools::ToolSpec::Function(function),
        }
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            assert_eq!(invocation.tool_name, self.tool_name());
            assert!(
                matches!(&invocation.payload,ToolPayload::Function{arguments} if arguments=="{}")
            );
            self.gates.entered.notify_one();
            self.gates.executions.fetch_add(1, Ordering::SeqCst);
            if self.finishes {
                invocation.cancellation_token.cancelled().await;
                self.gates.cancelled.notify_one();
                self.gates.finish.notified().await;
                self.gates.terminal.store(true, Ordering::SeqCst);
            }
            Ok(Box::new(FunctionToolOutput::from_text(
                "terminal native result".into(),
                Some(true),
            )) as Box<dyn crate::tools::context::ToolOutput>)
        })
    }
}
impl CoreToolRuntime for NamespacedRuntime {
    fn finishes_on_cancellation(&self) -> bool {
        self.finishes
    }
    fn wait_until_ready<'a>(&'a self, _session: &'a Arc<Session>) -> Option<BoxFuture<'a, ()>> {
        Some(Box::pin(async {
            self.gates.waiting.notify_one();
            self.gates.ready.notified().await;
        }))
    }
}

#[derive(Default)]
struct Observer(Mutex<Vec<ToolName>>);
impl ToolLifecycleContributor for Observer {
    fn on_tool_dispatch(&self, input: ToolDispatchInput<'_>) {
        self.0.lock().unwrap().push(input.tool_name.clone());
    }
}

async fn fixture(finishes: bool) -> (ToolCallRuntime, Arc<Gates>, Arc<Observer>) {
    let (mut session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.provider = codex_model_provider::create_model_provider(
        ModelProviderInfo {
            name: "stateless-lifecycle".into(),
            capabilities: Some(ModelProviderCapabilities {
                responses: Some(ResponsesCompatibility::StatelessFunctions),
                ..Default::default()
            }),
            ..Default::default()
        },
        /*auth_manager*/ None,
    );
    let observer = Arc::new(Observer::default());
    let mut extensions =
        codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    extensions.tool_lifecycle_contributor(observer.clone());
    session.services.extensions = Arc::new(extensions.build());
    let gates = Arc::new(Gates::default());
    let handler = Arc::new(NamespacedRuntime {
        name: ToolName::namespaced("client", "lookup"),
        gates: gates.clone(),
        finishes,
    }) as Arc<dyn CoreToolRuntime>;
    let specs = vec![handler.spec()];
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([handler]),
        specs,
        ToolMode::Direct,
        BTreeMap::new(),
        /*tool_namespaces_info*/ None,
        &[],
    ));
    let step = StepContext::for_test(Arc::new(turn)).with_tool_router_for_test(router);
    let runtime = ToolCallRuntime::new(
        Arc::new(session),
        step,
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    (runtime, gates, observer)
}

fn wire_call() -> ToolCall {
    ToolCall {
        tool_name: ToolName::plain("client__lookup").with_default_namespace(),
        call_id: "namespaced-call".into(),
        payload: ToolPayload::Function {
            arguments: "{}".into(),
        },
        encrypted_function_args: None,
    }
}

async fn shadow_fixture(
    exposure: crate::tools::registry::ToolExposure,
    advertise_both: bool,
    finishes: bool,
) -> (
    ToolCallRuntime,
    Arc<Gates>,
    Arc<Gates>,
    Arc<Observer>,
    Vec<codex_tools::ToolSpec>,
) {
    let (mut runtime, _, observer) = fixture(/*finishes*/ false).await;
    let visible_gates = Arc::new(Gates::default());
    let hidden_gates = Arc::new(Gates::default());
    let visible = Arc::new(NamespacedRuntime {
        name: ToolName::plain("a__b"),
        gates: visible_gates.clone(),
        finishes,
    }) as Arc<dyn CoreToolRuntime>;
    let hidden = Arc::new(NamespacedRuntime {
        name: ToolName::namespaced("a", "b"),
        gates: hidden_gates.clone(),
        finishes: false,
    }) as Arc<dyn CoreToolRuntime>;
    let mut specs = vec![visible.spec()];
    if advertise_both {
        specs.push(hidden.spec());
    }
    let mut registry = ToolRegistry::default();
    registry.register_trusted_with_exposure(visible, crate::tools::registry::ToolExposure::Direct);
    registry.register_trusted_with_exposure(hidden, exposure);
    let router = Arc::new(ToolRouter::from_parts(
        registry,
        specs.clone(),
        ToolMode::Direct,
        BTreeMap::new(),
        /*tool_namespaces_info*/ None,
        &[],
    ));
    runtime.step_context = runtime.step_context.with_tool_router_for_test(router);
    (runtime, visible_gates, hidden_gates, observer, specs)
}

fn shadow_call() -> ToolCall {
    ToolCall {
        tool_name: ToolName::plain("a__b"),
        ..wire_call()
    }
}

#[tokio::test]
async fn advertised_plain_alias_dispatches_without_hidden_shadow_and_keeps_runtime_lifecycle() {
    for exposure in [
        crate::tools::registry::ToolExposure::Hidden,
        crate::tools::registry::ToolExposure::CodeModeOnly,
    ] {
        for finishes in [false, true] {
            let (runtime, visible, hidden, observer, specs) =
                shadow_fixture(exposure, /*advertise_both*/ false, finishes).await;
            let wire = crate::client::responses_compatibility::function_tools(&specs).unwrap();
            assert_eq!(wire.len(), 1);
            assert_eq!(wire[0]["name"], "a__b");
            assert!(
                runtime
                    .step_context
                    .tool_router
                    .normalize_wire_call(
                        ToolCall {
                            tool_name: ToolName::namespaced("a", "b"),
                            ..wire_call()
                        },
                        &ToolCallSource::Direct,
                        /*stateless*/ true
                    )
                    .is_err()
            );
            let cancellation = CancellationToken::new();
            let mut task =
                tokio::spawn(runtime.handle_tool_call(shadow_call(), cancellation.clone()));
            tokio::time::timeout(Duration::from_secs(2), visible.waiting.notified())
                .await
                .unwrap();
            assert_eq!(visible.executions.load(Ordering::SeqCst), 0);
            assert_eq!(hidden.executions.load(Ordering::SeqCst), 0);
            assert!(
                !task.is_finished(),
                "advertised handler must retain readiness"
            );
            visible.ready.notify_one();
            tokio::time::timeout(Duration::from_secs(2), visible.entered.notified())
                .await
                .unwrap();
            if finishes {
                cancellation.cancel();
                tokio::time::timeout(Duration::from_secs(2), visible.cancelled.notified())
                    .await
                    .unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), &mut task)
                        .await
                        .is_err()
                );
                visible.finish.notify_one();
            }
            let result = tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let codex_protocol::models::ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } = &result.item
            else {
                panic!("unexpected handler result: {:?}", result.item);
            };
            assert_eq!(call_id.as_deref(), Some("namespaced-call"));
            if finishes {
                assert!(
                    visible.terminal.load(Ordering::SeqCst),
                    "native cancellation owner must finish before the runtime returns"
                );
                assert!(
                    matches!(&output.body, codex_protocol::models::FunctionCallOutputBody::Text(text) if text.starts_with("aborted by user after ")),
                    "unexpected cancelled output: {output:?}"
                );
                assert_eq!(output.success, None);
            } else {
                assert_eq!(
                    output.body,
                    codex_protocol::models::FunctionCallOutputBody::Text(
                        "terminal native result".into()
                    )
                );
                assert_eq!(output.success, Some(true));
            }
            assert_eq!(visible.executions.load(Ordering::SeqCst), 1);
            assert_eq!(hidden.executions.load(Ordering::SeqCst), 0);
            assert_eq!(*observer.0.lock().unwrap(), vec![ToolName::plain("a__b")]);
        }
    }
}

#[tokio::test]
async fn genuinely_advertised_alias_collision_fails_preflight_and_before_runtime_side_effects() {
    let (runtime, visible, other, observer, specs) = shadow_fixture(
        crate::tools::registry::ToolExposure::Direct,
        /*advertise_both*/ true,
        /*finishes*/ false,
    )
    .await;
    assert!(crate::client::responses_compatibility::function_tools(&specs).is_err());
    let result = runtime
        .handle_tool_call(shadow_call(), CancellationToken::new())
        .await
        .unwrap();
    assert!(
        matches!(&result.item, codex_protocol::models::ResponseItem::FunctionCallOutput { output, .. }
        if matches!(&output.body, codex_protocol::models::FunctionCallOutputBody::Text(text) if text.contains("ambiguous")))
    );
    assert_eq!(visible.executions.load(Ordering::SeqCst), 0);
    assert_eq!(other.executions.load(Ordering::SeqCst), 0);
    assert!(observer.0.lock().unwrap().is_empty());
    assert!(visible.waiting.notified().now_or_never().is_none());
    assert!(other.waiting.notified().now_or_never().is_none());
}

#[tokio::test]
async fn stateless_alias_waits_for_registered_runtime_readiness_and_records_canonical_name() {
    let (runtime, gates, observer) = fixture(/*finishes*/ false).await;
    let mut task = tokio::spawn(runtime.handle_tool_call(wire_call(), CancellationToken::new()));
    tokio::time::timeout(Duration::from_secs(2), gates.waiting.notified())
        .await
        .expect("registered readiness must be used");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), gates.entered.notified())
            .await
            .is_err(),
        "handler cannot bypass readiness"
    );
    assert!(!task.is_finished());
    gates.ready.notify_one();
    tokio::time::timeout(Duration::from_secs(2), &mut task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        *observer.0.lock().unwrap(),
        vec![ToolName::namespaced("client", "lookup")]
    );
}

#[tokio::test]
async fn stateless_alias_preserves_finish_on_cancel_and_waits_for_native_terminal_result() {
    let (runtime, gates, _) = fixture(/*finishes*/ true).await;
    let cancellation = CancellationToken::new();
    let mut task = tokio::spawn(runtime.handle_tool_call(wire_call(), cancellation.clone()));
    tokio::time::timeout(Duration::from_secs(2), gates.waiting.notified())
        .await
        .unwrap();
    gates.ready.notify_one();
    tokio::time::timeout(Duration::from_secs(2), gates.entered.notified())
        .await
        .unwrap();
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(2), gates.cancelled.notified())
        .await
        .expect("native handler must observe cancellation");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut task)
            .await
            .is_err(),
        "dispatch must await its registered terminal owner"
    );
    assert!(!gates.terminal.load(Ordering::SeqCst));
    gates.finish.notify_one();
    tokio::time::timeout(Duration::from_secs(2), &mut task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(gates.terminal.load(Ordering::SeqCst));
}
