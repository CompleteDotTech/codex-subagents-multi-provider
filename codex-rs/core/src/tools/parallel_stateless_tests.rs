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
use futures::future::BoxFuture;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
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
}

struct NamespacedRuntime {
    gates: Arc<Gates>,
    finishes: bool,
}
impl ToolExecutor<ToolInvocation> for NamespacedRuntime {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced("client", "lookup")
    }
    fn spec(&self) -> codex_tools::ToolSpec {
        codex_tools::ToolSpec::Namespace(codex_tools::ResponsesApiNamespace {
            name:"client".into(), description:"Lifecycle fixture namespace".into(),
            tools:vec![codex_tools::ResponsesApiNamespaceTool::Function(codex_tools::ResponsesApiTool {
            name: "lookup".into(),
            description: "Lifecycle fixture".into(),
            strict: true,
            defer_loading: None,
            parameters: serde_json::from_value(
                serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
            )
            .unwrap(),
            output_schema: None,
            })],
        })
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
        None,
    );
    let observer = Arc::new(Observer::default());
    let mut extensions =
        codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    extensions.tool_lifecycle_contributor(observer.clone());
    session.services.extensions = Arc::new(extensions.build());
    let gates = Arc::new(Gates::default());
    let handler = Arc::new(NamespacedRuntime {
        gates: gates.clone(),
        finishes,
    }) as Arc<dyn CoreToolRuntime>;
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([handler]),
        Vec::new(),
        ToolMode::Direct,
        BTreeMap::new(),
        None,
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

#[tokio::test]
async fn stateless_alias_waits_for_registered_runtime_readiness_and_records_canonical_name() {
    let (runtime, gates, observer) = fixture(false).await;
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
    let (runtime, gates, _) = fixture(true).await;
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
