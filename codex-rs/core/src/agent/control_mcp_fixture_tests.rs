// Native MCP cache fixtures live here so idle children use the real private launch path.
// No permit or test flag is exported to integration/production callers.
mod native_mcp_cache_tests {
    use super::*;
    use anyhow::Context as _;
    use anyhow::ensure;
    use codex_config::Constrained;
    use codex_exec_server::ExecutorFileSystem;
    use codex_extension_api::ExtensionRegistry;
    use core_test_support::test_codex::TestEnv;
    use std::sync::Arc;
    use wiremock::MockServer;

    use futures::FutureExt as _;
    use std::sync::Mutex;

    #[derive(Default)]
    struct OwnedMcpCase {
        fixtures: Vec<Arc<NativeMcpFixture>>,
        threads: Vec<Arc<CodexThread>>,
        http_gates: Vec<std::sync::mpsc::Sender<()>>,
        file_gates: Vec<(Arc<dyn ExecutorFileSystem>, PathUri)>,
        observers: Vec<Option<tokio::task::JoinHandle<()>>>,
    }
    tokio::task_local! {
        static OWNED_MCP_CASE: Arc<Mutex<OwnedMcpCase>>;
    }
    fn retain_thread(thread: &Arc<CodexThread>) {
        OWNED_MCP_CASE.with(|owned| {
            let mut owned = owned.lock().unwrap();
            if !owned.threads.iter().any(|entry| Arc::ptr_eq(entry, thread)) {
                owned.threads.push(Arc::clone(thread));
            }
        });
    }
    struct OwnedHttpGate(std::sync::mpsc::Sender<()>);
    impl OwnedHttpGate {
        fn send(self, value: ()) -> Result<(), std::sync::mpsc::SendError<()>> {
            self.0.send(value)
        }
    }
    impl Drop for OwnedHttpGate {
        fn drop(&mut self) {
            // Unblock a dedicated HTTP responder before its body-local server drops.
            let _ = self.0.send(());
        }
    }
    fn retain_http_gate(gate: std::sync::mpsc::Sender<()>) -> OwnedHttpGate {
        OWNED_MCP_CASE.with(|owned| owned.lock().unwrap().http_gates.push(gate.clone()));
        OwnedHttpGate(gate)
    }
    fn retain_file_gate(fs: &Arc<dyn ExecutorFileSystem>, path: &PathUri) {
        OWNED_MCP_CASE.with(|owned| {
            owned
                .lock()
                .unwrap()
                .file_gates
                .push((Arc::clone(fs), path.clone()))
        });
    }
    fn spawn_owned_observer<T: Send + 'static>(
        future: impl Future<Output = anyhow::Result<T>> + Send + 'static,
    ) -> tokio::sync::oneshot::Receiver<anyhow::Result<T>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let observer = tokio::spawn(async move {
            let result = future.await;
            let _ = sender.send(result);
        });
        OWNED_MCP_CASE.with(|owned| owned.lock().unwrap().observers.push(Some(observer)));
        receiver
    }
    async fn run_native_mcp_case(
        body: impl Future<Output = anyhow::Result<()>>,
    ) -> anyhow::Result<()> {
        let owned = Arc::new(Mutex::new(OwnedMcpCase::default()));
        let outcome = OWNED_MCP_CASE
            .scope(
                Arc::clone(&owned),
                std::panic::AssertUnwindSafe(body).catch_unwind(),
            )
            .await;
        let body_failed = !matches!(&outcome, Ok(Ok(())));
        let mut resources = std::mem::take(&mut *owned.lock().unwrap());
        // Release every test-owned startup gate before asking a live client to stop.
        // This also covers assertion/Err paths preceding the body's normal release.
        for gate in resources.http_gates.drain(..) {
            let _ = gate.send(()); // A consumed gate legitimately has no receiver.
        }
        let cleanup = async {
            let mut errors = Vec::new();
            for (fs, path) in resources.file_gates.drain(..) {
                if let Err(error) = fs
                    .write_file(
                        &path,
                        b"ready".to_vec(),
                        Default::default(),
                        /*sandbox*/ None,
                    )
                    .await
                {
                    errors.push(format!("release owned MCP file gate: {error}"));
                }
            }
            for slot in &mut resources.observers {
                let Some(observer) = slot.as_mut() else {
                    continue;
                };
                if body_failed && !observer.is_finished() {
                    // Only this test's event observer is aborted, never a production task.
                    observer.abort();
                }
                // On success, join even if the sender's final poll has not returned yet.
                // Receiving its result is not itself proof that the task completed.
                if let Err(error) = observer.await
                    && !error.is_cancelled()
                {
                    errors.push(format!("owned MCP observer panic: {error}"));
                }
                *slot = None;
            }
            for thread in resources.threads.iter().rev() {
                if let Err(error) = thread.shutdown_and_wait().await {
                    errors.push(format!("owned thread shutdown: {error}"));
                }
                // Invoke/join the exact MCP cleanup even after an earlier session error.
                thread.session.services.mcp_runtime.shutdown().await;
                let (config, _) = thread.current_mcp_config_and_runtime_context().await;
                let statuses = thread.mcp_connection_statuses(&config).await;
                if statuses
                    .values()
                    .any(|status| *status != McpServerConnectionStatus::Cancelled)
                {
                    errors.push("owned MCP client was not cancelled after joined shutdown".into());
                }
            }
            anyhow::ensure!(errors.is_empty(), "MCP owned cleanup failed: {errors:?}");
            anyhow::Ok(())
        };
        let cleanup = tokio::time::timeout(Duration::from_secs(10), cleanup).await;
        // A timed out cleanup still owns every not-yet-joined observer handle.
        // Cancel and join those exact observers before releasing this scope.
        for observer in resources.observers.iter_mut().filter_map(Option::take) {
            observer.abort();
            if let Err(error) = observer.await
                && !error.is_cancelled()
            {
                eprintln!("MCP_FIXTURE_OBSERVER_CLEANUP_ERROR: {error}");
            }
        }
        let cleanup_error = match cleanup {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(_) => Some("owned MCP cleanup deadline exceeded; ownership unresolved".into()),
        };
        if let Some(error) = &cleanup_error {
            eprintln!(
                "MCP_FIXTURE_CLEANUP_UNQUALIFIED: {error}; owned_threads={:?}",
                resources
                    .threads
                    .iter()
                    .map(|thread| thread.session.thread_id)
                    .collect::<Vec<_>>()
            );
        }
        match outcome {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(body_result) => {
                body_result?;
                anyhow::ensure!(cleanup_error.is_none(), "{cleanup_error:?}");
                eprintln!(
                    "MCP_FIXTURE_OWNED_CLEANUP_COMPLETE threads={}",
                    resources.threads.len()
                );
                Ok(())
            }
        }
    }

    struct NativeMcpBuilder {
        mutators: Vec<Box<dyn FnOnce(&mut Config) + Send>>,
        auth: CodexAuth,
        extensions: Arc<ExtensionRegistry<Config>>,
    }

    fn test_codex() -> NativeMcpBuilder {
        NativeMcpBuilder {
            mutators: vec![Box::new(|config| {
                config.features.disable(Feature::Apps).unwrap();
                config.features.disable(Feature::ShellSnapshot).unwrap();
            })],
            auth: CodexAuth::from_api_key("dummy"),
            extensions: empty_extension_registry(),
        }
    }

    fn apps_enabled_builder(url: impl Into<String>) -> NativeMcpBuilder {
        let url = url.into();
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(move |config| {
                config.features.enable(Feature::Apps).unwrap();
                config.chatgpt_base_url = url;
            })
    }

    impl NativeMcpBuilder {
        fn with_auth(mut self, auth: CodexAuth) -> Self {
            self.auth = auth;
            self
        }
        fn with_config(mut self, configure: impl FnOnce(&mut Config) + Send + 'static) -> Self {
            self.mutators.push(Box::new(configure));
            self
        }
        fn with_extensions(mut self, extensions: Arc<ExtensionRegistry<Config>>) -> Self {
            self.extensions = extensions;
            self
        }
        fn with_model_info_override(
            mut self,
            model: &str,
            configure: impl FnOnce(&mut codex_protocol::openai_models::ModelInfo) + Send + 'static,
        ) -> Self {
            let model = model.to_owned();
            self.mutators.push(Box::new(move |config| {
                let mut catalog = config
                    .model_catalog
                    .clone()
                    .unwrap_or_else(|| codex_models_manager::bundled_models_response().unwrap());
                if !catalog.models.iter().any(|info| info.slug == model) {
                    let mut info = catalog
                        .models
                        .iter()
                        .find(|info| info.slug == "gpt-5.5")
                        .unwrap()
                        .clone();
                    info.slug = model.clone();
                    info.display_name = model.clone();
                    catalog.models.push(info);
                }
                configure(
                    catalog
                        .models
                        .iter_mut()
                        .find(|info| info.slug == model)
                        .unwrap(),
                );
                config.model = Some(model);
                config.model_catalog = Some(catalog);
            }));
            self
        }
        async fn build_with_auto_env(
            self,
            server: &MockServer,
        ) -> anyhow::Result<Arc<NativeMcpFixture>> {
            let home = Arc::new(TempDir::new()?);
            let environment = core_test_support::test_codex::test_env().await?;
            let mut config = ConfigBuilder::without_managed_config_for_tests()
                .codex_home(home.path().to_path_buf())
                .harness_overrides(crate::config::ConfigOverrides {
                    cwd: Some(environment.cwd().to_path_buf()),
                    ..Default::default()
                })
                .build()
                .await?;
            config.model = Some("gpt-5.5".into());
            config.model_provider =
                codex_model_provider_info::ModelProviderInfo {
                    base_url: Some(format!("{}/v1", server.uri())),
                    supports_websockets: false,
                    ..codex_model_provider_info::built_in_model_providers(
                        /*openai_base_url*/ None,
                    )["openai"]
                        .clone()
                };
            for configure in self.mutators {
                configure(&mut config);
            }
            #[cfg(target_os = "linux")]
            let sandbox = Some(core_test_support::find_codex_linux_sandbox_exe()?);
            #[cfg(not(target_os = "linux"))]
            let sandbox = None;
            let runtime = codex_exec_server::ExecServerRuntimeOptions::new(
                std::env::current_exe()?,
                sandbox,
            )?;
            let environments = Arc::new(
                codex_exec_server::EnvironmentManager::create_for_tests(
                    environment.exec_server_url().map(str::to_owned),
                    Some(runtime),
                )
                .await,
            );
            let auth = AuthManager::from_auth_for_testing(self.auth);
            let state_db = crate::init_state_db(&config).await;
            let store = crate::thread_manager::thread_store_from_config(&config, state_db.clone());
            let models = crate::thread_manager::build_models_manager(&config, Arc::clone(&auth));
            let extensions = self.extensions;
            let manager = Arc::new_cyclic(|weak| {
                let mut extensions = extensions.to_builder();
                crate::install_agent_message_board(&mut extensions, weak.clone());
                let manager = ThreadManager::new(
                    &config,
                    Arc::clone(&auth),
                    models,
                    crate::CodexAppsToolsCache::default(),
                    SessionSource::Exec,
                    environments,
                    Arc::new(extensions.build()),
                    Arc::new(crate::test_support::EmptyUserInstructionsProvider),
                    /*analytics_events_client*/ None,
                    crate::thread_manager::passthrough_image_store(),
                    store,
                    crate::thread_manager::local_agent_graph_store_from_state_db(state_db.as_ref()),
                    uuid::Uuid::new_v4().to_string(),
                    /*attestation_provider*/ None,
                    /*external_time_provider*/ None,
                );
                if config.features.enabled(Feature::CodeModeHost) {
                    if let Ok(program) = codex_utils_cargo_bin::cargo_bin("codex-code-mode-host") {
                        return crate::test_support::with_code_mode_host_program(
                            manager, program, &config,
                        );
                    }
                }
                manager
            });
            let started = manager
                .start_thread(StartThreadOptions {
                    environments: Some(vec![environment.request()]),
                    ..StartThreadOptions::new(config.clone())
                })
                .await?;
            retain_thread(&started.thread);
            let fixture = Arc::new(NativeMcpFixture {
                _home: home,
                config,
                codex: started.thread,
                session_configured: started.session_configured,
                thread_manager: manager,
                environment,
            });
            OWNED_MCP_CASE.with(|owned| owned.lock().unwrap().fixtures.push(Arc::clone(&fixture)));
            Ok(fixture)
        }
    }

    struct NativeMcpFixture {
        _home: Arc<TempDir>,
        config: Config,
        codex: Arc<CodexThread>,
        session_configured: codex_protocol::protocol::SessionConfiguredEvent,
        thread_manager: Arc<ThreadManager>,
        environment: TestEnv,
    }
    impl NativeMcpFixture {
        fn fs(&self) -> Arc<dyn ExecutorFileSystem> {
            self.environment.environment().get_filesystem()
        }
        async fn native_idle_child(&self, role: Option<&str>) -> anyhow::Result<Arc<CodexThread>> {
            let parent = &self.codex;
            let id = parent.session.thread_id;
            let control = self.thread_manager.agent_control();
            let turn = parent.session.new_default_turn().await;
            let version = turn.multi_agent_version;
            let prepared = crate::agent::child_config::prepare_agent_spawn_config(
                &parent.session,
                &StepContext::for_test(turn),
                crate::agent::child_config::SpawnConfigOptions {
                    resolved_provider: None,
                    model_provider: None,
                    version: if version == MultiAgentVersion::V2 {
                        crate::agent::child_config::SpawnConfigVersion::V2
                    } else {
                        crate::agent::child_config::SpawnConfigVersion::V1
                    },
                    full_history_fork: false,
                    role_name: role,
                    model: None,
                    reasoning_effort: None,
                },
            )
            .await
            .map_err(anyhow::Error::msg)?;
            let mut config = prepared.config;
            let membership = control.runtime.admit_start()?;
            let state = control.runtime.upgrade()?;
            let slot = if version == MultiAgentVersion::V2 {
                Some(
                    control
                        .reserve_v2_residency_slot(
                            &state,
                            &config,
                            &membership,
                            /*protected_thread_id*/ Some(id),
                        )
                        .await?,
                )
            } else {
                None
            };
            let mut reservation = control.runtime.registry.reserve_spawn_slot(
                if version == MultiAgentVersion::V2 {
                    None
                } else {
                    config.effective_agent_max_threads(version)
                },
            )?;
            let (source, mut metadata) = control.prepare_thread_spawn(
                &mut reservation,
                &config,
                id,
                next_thread_spawn_depth(&parent.session_source),
                /*agent_path*/ None,
                prepared.role_name,
                /*preferred_agent_nickname*/ None,
            )?;
            let inherited_environments = control
                .inherited_environments_for_source(&state, Some(&source))
                .await;
            let inherited_exec_policy = control
                .inherited_exec_policy_for_source(&state, Some(&source), &config)
                .await;
            let environments = parent
                .environment_selections()
                .await
                .into_iter()
                .map(TurnEnvironmentSelection::into_request)
                .collect();
            let permit = config
                .prepared_child_authorization
                .take()
                .context("real native preparation proof")?
                .consume(
                    &control.runtime,
                    id,
                    &config,
                    /*full_history*/ false,
                    &source,
                )
                .await?;
            let child = state
                .spawn_child_thread(
                    StartThreadOptions {
                        session_source: Some(source),
                        thread_source: Some(codex_protocol::protocol::ThreadSource::Subagent),
                        history_mode: matches!(
                            parent.config_snapshot().await.history_mode,
                            ThreadHistoryMode::Paginated
                        )
                        .then_some(ThreadHistoryMode::Paginated),
                        environments: Some(environments),
                        inherited_environments,
                        ..StartThreadOptions::new(config)
                    },
                    control.clone(),
                    Some(id),
                    inherited_exec_policy,
                    permit,
                )
                .await?;
            retain_thread(&child.thread);
            let pending = super::super::spawn_guard::PendingSpawn::new(
                Arc::clone(&state),
                Arc::clone(&child.thread),
                membership,
            );
            let authority = control
                .runtime
                .retained_authority(child.thread_id)
                .context("actual child registration")?;
            ensure!(
                child.thread.session.active_turn.lock().await.is_none(),
                "native fixture must be idle before first binding capture"
            );
            ensure!(
                authority.matches_session(&child.thread.session),
                "actual private pipeline registered this child allocation"
            );
            let threads = state.threads.write().await;
            ensure!(
                threads
                    .get(&child.thread_id)
                    .is_some_and(|registered| Arc::ptr_eq(registered, &child.thread))
                    && child.thread.is_running(),
                "actual native child remains loaded and live"
            );
            metadata.agent_id = Some(child.thread_id);
            reservation.commit_if_current(metadata, &authority)?;
            if let Some(slot) = slot {
                slot.commit(child.thread_id);
            }
            let _membership = pending.disarm();
            drop(threads);
            Ok(child.thread)
        }
    }

    async fn wait_for_event<F>(thread: &CodexThread, mut predicate: F) -> EventMsg
    where
        F: FnMut(&EventMsg) -> bool,
    {
        loop {
            let event = timeout(Duration::from_secs(10), thread.next_event())
                .await
                .expect("timeout waiting for event")
                .expect("stream ended unexpectedly");
            if predicate(&event.msg) {
                return event.msg;
            }
        }
    }
    async fn wait_for_mcp_server(codex: &CodexThread, server_name: &str) -> anyhow::Result<()> {
        use codex_protocol::protocol::EventMsg;

        // Wait for the startup summary regardless of outcome, then interpret the
        // requested server's ready, failed, or cancelled entry below.
        let summary = loop {
            let event = codex
                .next_event()
                .await
                .expect("stream ended unexpectedly while waiting for MCP startup");
            if let EventMsg::McpStartupComplete(summary) = event.msg {
                break summary;
            }
        };
        if let Some(failure) = summary
            .failed
            .iter()
            .find(|failure| failure.server == server_name)
        {
            let error = &failure.error;
            anyhow::bail!("MCP server {server_name} failed to start: {error}");
        }
        if summary.cancelled.iter().any(|server| server == server_name) {
            anyhow::bail!("MCP server {server_name} startup was cancelled");
        }
        assert!(
            summary.ready.iter().any(|server| server == server_name),
            "expected MCP server {server_name} to be ready; startup summary: {summary:?}"
        );
        Ok(())
    }

    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use codex_config::types::McpServerTransportConfig;
    use codex_exec_server::RemoveOptions;
    use codex_extension_api::ExtensionFuture;
    use codex_extension_api::ExtensionRegistryBuilder;
    use codex_extension_api::McpServerContribution;
    use codex_extension_api::McpServerContributionContext;
    use codex_extension_api::McpServerContributor;
    use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
    use codex_protocol::mcp::McpServerConnectionStatus;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::McpInvocation;
    use codex_protocol::protocol::McpStartupStatus;
    use codex_protocol::protocol::Op;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    use codex_protocol::protocol::ThreadSettingsOverrides;
    use codex_protocol::user_input::UserInput;
    use codex_utils_path_uri::PathUri;
    use core_test_support::apps_test_server::AppsTestServer;
    use core_test_support::apps_test_server::SEARCH_CALENDAR_CREATE_TOOL;
    use core_test_support::apps_test_server::SEARCH_CALENDAR_NAMESPACE;
    use core_test_support::is_remote_test_environment;
    use core_test_support::responses;
    use core_test_support::responses::ResponseMock;
    use core_test_support::responses::mount_sse_once;
    use core_test_support::skip_if_no_network;
    use core_test_support::skip_if_wine_exec;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;
    use test_case::test_case;
    use tracing::Subscriber;
    use tracing::span::Attributes;
    use tracing::span::Id;
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::Context as LayerContext;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    use crate::TurnInputRequest;
    use crate::thread_manager::NewThread;
    const SERVER_NAME: &str = "cached_rmcp";
    const NAMESPACE: &str = "mcp__cached_rmcp";

    struct ChildHttpHeadersHelper;
    impl McpServerContributor<Config> for ChildHttpHeadersHelper {
        fn id(&self) -> &'static str {
            "native_child_http_headers_fixture"
        }
        fn contribute<'a>(
            &'a self,
            context: McpServerContributionContext<'a, Config>,
        ) -> ExtensionFuture<'a, Vec<McpServerContribution>> {
            Box::pin(async move {
                if !matches!(
                    context.session_source(),
                    Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }))
                ) {
                    return Vec::new();
                }
                let mut server = context.config().mcp_servers.get()[SERVER_NAME].clone();
                let McpServerTransportConfig::StreamableHttp {
                    http_headers_helper,
                    ..
                } = &mut server.transport
                else {
                    panic!("configured parent server must be HTTP")
                };
                *http_headers_helper = Some(if cfg!(windows) {
                    r#"echo {"X-Cache-Test":"helper"}"#.to_string()
                } else {
                    r#"printf '{"X-Cache-Test":"helper"}'"#.to_string()
                });
                vec![McpServerContribution::Set {
                    name: SERVER_NAME.into(),
                    config: Box::new(server),
                }]
            })
        }
    }

    struct McpBindingCaptureCounter(Arc<AtomicUsize>);

    impl<S: Subscriber> Layer<S> for McpBindingCaptureCounter {
        fn on_new_span(
            &self,
            attributes: &Attributes<'_>,
            _id: &Id,
            _context: LayerContext<'_, S>,
        ) {
            if attributes.metadata().target() == "codex_mcp::connection_manager::tool_catalog"
                && attributes.metadata().name() == "capture_binding_with_metadata"
            {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    fn user_turn(prompt: &str) -> TurnInputRequest {
        TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.to_string(),
            text_elements: Vec::new(),
        }])
        .with_thread_settings(ThreadSettingsOverrides {
            approval_policy: Some(AskForApproval::Never),
            permission_profile: Some(PermissionProfile::Disabled),
            ..Default::default()
        })
    }

    fn process_label(pid: &str) -> String {
        format!("rmcp-test-process-{pid}")
    }

    fn assert_definition(
        response: &ResponseMock,
        namespace_description: &str,
        tool_description: &str,
    ) {
        let body = response.single_request().body_json();
        let namespace = body
            .get("tools")
            .and_then(Value::as_array)
            .and_then(|tools| {
                tools
                    .iter()
                    .find(|tool| tool.get("name").and_then(Value::as_str) == Some(NAMESPACE))
            })
            .expect("request should contain the MCP namespace");
        assert_eq!(
            namespace.get("description").and_then(Value::as_str),
            Some(namespace_description)
        );
        assert_eq!(
            responses::namespace_child_tool(&body, NAMESPACE, "echo")
                .and_then(|tool| tool.get("description"))
                .and_then(Value::as_str),
            Some(tool_description)
        );
    }

    async fn wait_for_new_pid(
        fs: &dyn ExecutorFileSystem,
        path: &PathUri,
        previous_pid: Option<&str>,
    ) -> anyhow::Result<String> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(contents) = fs
                    .read_file_text(path, Default::default(), /*sandbox*/ None)
                    .await
                {
                    let pid = contents.trim();
                    if !pid.is_empty() && Some(pid) != previous_pid {
                        return pid.to_string();
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("timed out waiting for a new MCP server process")
    }

    use core_test_support::stdio_server_bin;
    use core_test_support::test_docker_container_name;
    use std::path::Path;
    use std::process::Command as StdCommand;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    const REMOTE_MCP_ENVIRONMENT: &str = "remote";

    pub(super) fn remote_aware_environment_id() -> String {
        if is_remote_test_environment() {
            REMOTE_MCP_ENVIRONMENT.to_string()
        } else {
            codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string()
        }
    }

    pub(super) fn remote_aware_stdio_server_bin() -> anyhow::Result<String> {
        let bin = stdio_server_bin()?;
        let Some(container_name) = test_docker_container_name() else {
            return Ok(bin);
        };

        // Keep the Docker path rewrite scoped to tests that use `build_remote_aware`.
        // Other MCP tests still start their stdio server from the orchestrator test
        // process, even when the full-ci remote env is present.
        //
        // Remote-aware MCP tests run the executor inside Docker. The stdio test
        // server is built on the host, so hand the executor a copied in-container
        // path instead of the host build artifact path.
        // Several remote-aware MCP tests can run in parallel; give each copied
        // binary its own path so one test cannot replace another test's executable.
        copy_binary_to_remote_env(&container_name, Path::new(&bin), "test_stdio_server")
    }

    fn unique_remote_path(binary_name: &str) -> anyhow::Result<String> {
        let unique_suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(format!(
            "/tmp/codex-remote-env/{binary_name}-{}-{unique_suffix}",
            std::process::id()
        ))
    }

    fn copy_binary_to_remote_env(
        container_name: &str,
        host_path: &Path,
        binary_name: &str,
    ) -> anyhow::Result<String> {
        let remote_path = unique_remote_path(binary_name)?;
        let mkdir_output = StdCommand::new("docker")
            .args([
                "exec",
                container_name,
                "mkdir",
                "-p",
                "/tmp/codex-remote-env",
            ])
            .output()
            .context("create remote MCP test binary directory")?;
        ensure!(
            mkdir_output.status.success(),
            "docker mkdir remote MCP test binary directory failed: stdout={} stderr={}",
            String::from_utf8_lossy(&mkdir_output.stdout).trim(),
            String::from_utf8_lossy(&mkdir_output.stderr).trim()
        );

        let container_target = format!("{container_name}:{remote_path}");
        let copy_output = StdCommand::new("docker")
            .arg("cp")
            .arg(host_path)
            .arg(&container_target)
            .output()
            .with_context(|| {
                format!(
                    "copy {} to remote MCP test env",
                    host_path.to_string_lossy()
                )
            })?;
        ensure!(
            copy_output.status.success(),
            "docker cp {binary_name} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&copy_output.stdout).trim(),
            String::from_utf8_lossy(&copy_output.stderr).trim()
        );

        let chmod_output = StdCommand::new("docker")
            .args(["exec", container_name, "chmod", "+x", remote_path.as_str()])
            .output()
            .with_context(|| format!("mark remote {binary_name} executable"))?;
        ensure!(
            chmod_output.status.success(),
            "docker chmod {binary_name} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&chmod_output.stdout).trim(),
            String::from_utf8_lossy(&chmod_output.stderr).trim()
        );

        Ok(remote_path)
    }

    #[tokio::test]
    async fn apps_cache_filled_during_binding_capture_reaches_the_model() -> anyhow::Result<()> {
        run_native_mcp_case(apps_cache_filled_during_binding_capture_reaches_the_model_body()).await
    }

    async fn apps_cache_filled_during_binding_capture_reaches_the_model_body() -> anyhow::Result<()>
    {
        skip_if_no_network!(Ok(()));

        struct ChildAppsEndpoint(String);

        impl McpServerContributor<Config> for ChildAppsEndpoint {
            fn id(&self) -> &'static str {
                "child_apps_cache_test"
            }

            fn contribute<'a>(
                &'a self,
                context: McpServerContributionContext<'a, Config>,
            ) -> ExtensionFuture<'a, Vec<McpServerContribution>> {
                Box::pin(async move {
                    if !matches!(context.session_source(), Some(SessionSource::SubAgent(_))) {
                        return Vec::new();
                    }
                    vec![McpServerContribution::HostedApps {
                        config: Box::new(
                            serde_json::from_value(json!({ "url": self.0 }))
                                .expect("child Apps MCP config"),
                        ),
                        protocol_mode: None,
                    }]
                })
            }
        }

        tokio::time::timeout(Duration::from_secs(30), async {
            let server = responses::start_mock_server().await;
            let tools_available = Arc::new(AtomicBool::new(false));
            let apps = AppsTestServer::mount_with_tools_available_when(
                &server,
                Arc::clone(&tools_available),
            )
            .await?;
            let waiting_server = responses::start_mock_server().await;
            let (waiting, waiting_startup) =
                AppsTestServer::mount_with_startup_control(&waiting_server).await?;
            let pending_server = responses::start_mock_server().await;
            let (pending_apps, pending_startup) =
                AppsTestServer::mount_with_startup_control(&pending_server).await?;
            // Gate only the child's MCP endpoint, leaving backend directory requests unblocked.
            let mut extensions = ExtensionRegistryBuilder::new();
            extensions.mcp_server_contributor(Arc::new(ChildAppsEndpoint(format!(
                "{}/api/codex/ps/mcp",
                pending_apps.chatgpt_base_url
            ))));
            let test = apps_enabled_builder(apps.chatgpt_base_url)
                .with_model_info_override("gpt-5.5", |model| model.supports_search_tool = false)
                .with_extensions(Arc::new(extensions.build()))
                .with_config(move |config| {
                    // Keep the gated HTTP request on the app host. Wine's executor serializes RPCs,
                    // so blocking it here would also block the peer startup that releases this gate.
                    config
                        .mcp_servers
                        .set(std::collections::HashMap::from([(
                            SERVER_NAME.to_string(),
                            serde_json::from_value(json!({
                                "url": format!("{}/api/codex/ps/mcp", waiting.chatgpt_base_url),
                                "http_headers": { "Authorization": "Bearer cache-test-token" },
                                "enabled_tools": ["calendar_list_events"],
                            }))
                            .expect("cacheable MCP config"),
                        )]))
                        .expect("test MCP config");
                })
                .build_with_auto_env(&server)
                .await?;
            // Startup emits one summary for both servers, not one event per server.
            let EventMsg::McpStartupComplete(startup) = wait_for_event(&test.codex, |event| {
                matches!(event, EventMsg::McpStartupComplete(_))
            })
            .await
            else {
                unreachable!("event predicate guarantees the startup summary");
            };
            assert_eq!(
                startup
                    .ready
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>(),
                std::collections::BTreeSet::from([
                    CODEX_APPS_MCP_SERVER_NAME.to_string(),
                    SERVER_NAME.to_string(),
                ])
            );

            let release_apps = retain_http_gate(pending_startup.hold_next_successful_initialize());
            let release_waiting =
                retain_http_gate(waiting_startup.hold_next_successful_initialize());
            let child = test.native_idle_child(/*role*/ None).await?;
            assert_eq!(waiting_startup.initialize_attempts(), 1);
            let response = mount_sse_once(
                &server,
                responses::sse(vec![
                    responses::ev_response_created("cache-refresh"),
                    responses::ev_assistant_message("done", "done"),
                    responses::ev_completed("cache-refresh"),
                ]),
            )
            .await;
            child
                .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Mention {
                    name: SERVER_NAME.to_string(),
                    path: format!("mcp://{SERVER_NAME}"),
                }]))
                .await?;
            wait_for_event(&child, |event| {
                matches!(event, EventMsg::McpStartupUpdate(update)
            if update.server == SERVER_NAME && matches!(update.status, McpStartupStatus::Starting))
            })
            .await;

            // A peer fills the initially empty shared Apps cache while capture waits for the named server.
            tools_available.store(true, Ordering::SeqCst);
            let mut peer_config = test.config.clone();
            peer_config
                .mcp_servers
                .set(std::collections::HashMap::new())?;
            let NewThread { thread: peer, .. } = test
                .thread_manager
                .start_thread(StartThreadOptions::new(peer_config))
                .await?;
            retain_thread(&peer);
            wait_for_mcp_server(&peer, CODEX_APPS_MCP_SERVER_NAME).await?;
            release_waiting.send(())?;
            wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
            // The child's own Apps client must still be pending when its request reaches inference.
            release_apps.send(())?;
            let body = response.single_request().body_json();
            assert!(
                responses::namespace_child_tool(
                    &body,
                    SEARCH_CALENDAR_NAMESPACE,
                    SEARCH_CALENDAR_CREATE_TOOL,
                )
                .is_some(),
                "the first model request must include the peer's Apps tools: {body}"
            );

            child.shutdown_and_wait().await?;
            peer.shutdown_and_wait().await?;
            Ok(())
        })
        .await
        .context("timed out exercising an Apps cache fill during binding capture")?
    }

    #[test_case(false, false, 1; "optional server uses cache")]
    #[test_case(true, false, 1; "required server uses cache")]
    #[test_case(false, true, 2; "headers helper bypasses cache")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_http_mcp_starts_lazily_for_subagents(
        required: bool,
        with_headers_helper: bool,
        expected_startup_attempts: usize,
    ) -> anyhow::Result<()> {
        run_native_mcp_case(cached_http_mcp_starts_lazily_for_subagents_body(
            required,
            with_headers_helper,
            expected_startup_attempts,
        ))
        .await
    }

    async fn cached_http_mcp_starts_lazily_for_subagents_body(
        required: bool,
        with_headers_helper: bool,
        expected_startup_attempts: usize,
    ) -> anyhow::Result<()> {
        skip_if_no_network!(Ok(()));
        if with_headers_helper && is_remote_test_environment() {
            return Ok(());
        }

        let responses_server = responses::start_mock_server().await;
        let (http_server, startup_control) =
            AppsTestServer::mount_with_startup_control(&responses_server).await?;
        let server_url = format!("{}/api/codex/ps/mcp", http_server.chatgpt_base_url);
        let mut extensions = ExtensionRegistryBuilder::new();
        if with_headers_helper {
            extensions.mcp_server_contributor(Arc::new(ChildHttpHeadersHelper));
        }
        let fixture = test_codex()
            .with_extensions(Arc::new(extensions.build()))
            .with_model_info_override("gpt-5.4", |model| model.supports_search_tool = false)
            .with_config(move |config| {
                config.permissions.approval_policy = Constrained::allow_any(AskForApproval::Never);
                config
                    .permissions
                    .set_permission_profile(PermissionProfile::Disabled)
                    .expect("test config should allow disabled permissions");
                let mut servers = config.mcp_servers.get().clone();
                servers.insert(
                    SERVER_NAME.to_string(),
                    serde_json::from_value(json!({
                        "url": server_url,
                        "http_headers": { "Authorization": "Bearer cached-http-test-token" },
                        "enabled_tools": ["calendar_create_event"],
                        "required": required,
                        "startup_timeout_sec": 10,
                    }))
                    .expect("HTTP MCP server configuration"),
                );
                config
                    .mcp_servers
                    .set(servers)
                    .expect("test MCP server configuration");
            })
            .build_with_auto_env(&responses_server)
            .await?;
        wait_for_mcp_server(&fixture.codex, SERVER_NAME).await?;
        assert_eq!(startup_control.initialize_attempts(), 1);

        let parent_server = fixture.codex.config().await.mcp_servers.get()[SERVER_NAME].clone();
        let subagent = fixture.native_idle_child(/*role*/ None).await?;
        assert_eq!(
            fixture.codex.config().await.mcp_servers.get()[SERVER_NAME],
            parent_server
        );
        let (child_mcp_config, _) = subagent.current_mcp_config_and_runtime_context().await;
        let effective = child_mcp_config
            .mcp_server_catalog
            .server(SERVER_NAME)
            .expect("actual registered child MCP server")
            .config();
        let McpServerTransportConfig::StreamableHttp {
            http_headers_helper: parent_helper,
            ..
        } = &parent_server.transport
        else {
            unreachable!()
        };
        let McpServerTransportConfig::StreamableHttp {
            http_headers_helper: child_helper,
            ..
        } = &effective.transport
        else {
            unreachable!()
        };
        assert!(
            parent_helper.is_none(),
            "host contributor must leave actual parent configuration unchanged"
        );
        assert_eq!(
            child_helper.is_some(),
            with_headers_helper,
            "actual native child resolves the intended host contribution"
        );
        if with_headers_helper {
            wait_for_mcp_server(&subagent, SERVER_NAME).await?;
        }
        assert_eq!(
            startup_control.initialize_attempts(),
            expected_startup_attempts
        );

        let call_id = "http-call";
        let call_response = mount_sse_once(
            &responses_server,
            responses::sse(vec![
                responses::ev_response_created(call_id),
                responses::ev_function_call_with_namespace(
                    call_id,
                    NAMESPACE,
                    "calendar_create_event",
                    r#"{"title":"cached","starts_at":"2026-01-01T00:00:00Z"}"#,
                ),
                responses::ev_completed(call_id),
            ]),
        )
        .await;
        let completion_response = mount_sse_once(
            &responses_server,
            responses::sse(vec![
                responses::ev_assistant_message("http-call-message", "done"),
                responses::ev_completed("http-call-done"),
            ]),
        )
        .await;
        subagent
            .start_or_steer_turn(user_turn("Call the cached HTTP tool."))
            .await?;
        wait_for_event(&subagent, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        let request = call_response.single_request();
        assert!(
            request
                .tool_by_name(NAMESPACE, "calendar_create_event")
                .is_some()
        );
        let output = completion_response.function_call_output_text(call_id);
        assert!(output.is_some());
        assert_eq!(startup_control.initialize_attempts(), 2);

        fixture.codex.shutdown_and_wait().await?;
        subagent.shutdown_and_wait().await?;
        responses_server.verify().await;
        Ok(())
    }

    #[test_case(false; "parent MCP shutdown preserves native child cached runtime")]
    #[test_case(true; "root shutdown fences native child followup at MCP gate")]
    #[tokio::test(flavor = "current_thread")]
    async fn cached_mcp_startup_is_eager_for_root_and_lazy_for_subagents(
        root_shutdown: bool,
    ) -> anyhow::Result<()> {
        run_native_mcp_case(
            cached_mcp_startup_is_eager_for_root_and_lazy_for_subagents_body(root_shutdown),
        )
        .await
    }

    async fn cached_mcp_startup_is_eager_for_root_and_lazy_for_subagents_body(
        root_shutdown: bool,
    ) -> anyhow::Result<()> {
        skip_if_wine_exec!(
            Ok(()),
            "requires a Windows test_stdio_server in the Wine-exec environment"
        );
        skip_if_no_network!(Ok(()));

        let binding_captures = Arc::new(AtomicUsize::new(0));
        let _tracing = tracing_subscriber::registry()
            .with(McpBindingCaptureCounter(Arc::clone(&binding_captures)))
            .set_default();

        let responses_server = responses::start_mock_server().await;
        let command = remote_aware_stdio_server_bin()?;
        let environment_id = remote_aware_environment_id();
        let fixture = test_codex()
            .with_model_info_override("gpt-5.4", |model| model.supports_search_tool = false)
            .with_config(move |config| {
                config.update_plan_enabled = true;
                config.permissions.approval_policy = Constrained::allow_any(AskForApproval::Never);
                config
                    .permissions
                    .set_permission_profile(PermissionProfile::Disabled)
                    .expect("test config should allow disabled permissions");
                let app_only_cwd_marker_file = config.cwd.join("cwd-app-only");
                let barrier_file = config.cwd.join("allow-initialize");
                let pid_file = config.cwd.join("mcp.pid");
                let mut servers = config.mcp_servers.get().clone();
                servers.insert(
                    SERVER_NAME.to_string(),
                    serde_json::from_value(json!({
                        "command": command,
                        "environment_id": environment_id,
                        "cwd": config.cwd,
                        "env": {
                            "MCP_TEST_APP_ONLY_CWD_MARKER_FILE": app_only_cwd_marker_file,
                            "MCP_TEST_INITIALIZE_BARRIER_FILE": barrier_file,
                            "MCP_TEST_DYNAMIC_SERVER_METADATA": "1",
                            "MCP_TEST_PID_FILE": pid_file,
                        },
                        "enabled_tools": ["cwd", "echo"],
                        "startup_timeout_sec": 10,
                    }))
                    .expect("test MCP server configuration"),
                );
                config
                    .mcp_servers
                    .set(servers)
                    .expect("test MCP server configuration");
            })
            .build_with_auto_env(&responses_server)
            .await?;
        let fs = fixture.fs();
        let app_only_cwd_marker_file =
            PathUri::from_host_native_path(fixture.config.cwd.join("cwd-app-only"))?;
        let barrier_file =
            PathUri::from_host_native_path(fixture.config.cwd.join("allow-initialize"))?;
        let pid_file = PathUri::from_host_native_path(fixture.config.cwd.join("mcp.pid"))?;
        retain_file_gate(&fs, &barrier_file);

        let cold_response = mount_sse_once(
            &responses_server,
            responses::sse(vec![
                responses::ev_response_created("cold"),
                responses::ev_assistant_message("cold-message", "done"),
                responses::ev_completed("cold"),
            ]),
        )
        .await;
        fixture
            .codex
            .start_or_steer_turn(user_turn("use the echo tool"))
            .await?;
        let first_pid = wait_for_new_pid(fs.as_ref(), &pid_file, /*previous_pid*/ None).await?;
        fs.write_file(
            &barrier_file,
            b"ready".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        wait_for_event(&fixture.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        let first_process = process_label(&first_pid);
        assert_definition(
            &cold_response,
            &format!("Use the tools from {first_process}."),
            &format!("Echo from {first_process}."),
        );

        let NewThread {
            thread: eager_thread,
            ..
        } = fixture
            .thread_manager
            .start_thread(StartThreadOptions::new(fixture.config.clone()))
            .await?;
        retain_thread(&eager_thread);
        let eager_pid = wait_for_new_pid(fs.as_ref(), &pid_file, Some(&first_pid)).await?;
        wait_for_mcp_server(&eager_thread, SERVER_NAME).await?;
        eager_thread.shutdown_and_wait().await?;
        let cached_process = process_label(&eager_pid);

        fs.remove(
            &barrier_file,
            RemoveOptions {
                recursive: false,
                force: false,
                follow_symlinks: true,
            },
            /*sandbox*/ None,
        )
        .await?;
        fs.write_file(
            &app_only_cwd_marker_file,
            b"app-only".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        let second_thread = fixture.native_idle_child(/*role*/ None).await?;
        second_thread.submit(Op::Interrupt).await?;

        let root_revision = *fixture.codex.session.child_policy_revision.lock().unwrap();
        let root_epoch = fixture
            .thread_manager
            .agent_control()
            .runtime
            .retained_authority(fixture.session_configured.thread_id)
            .unwrap();
        binding_captures.store(0, Ordering::SeqCst);
        let unused_response = mount_sse_once(
            &responses_server,
            responses::sse(vec![
                responses::ev_response_created("unused"),
                responses::ev_function_call(
                    "unused-plan",
                    "update_plan",
                    r#"{"plan":[{"step":"Work without MCP tools","status":"in_progress"}]}"#,
                ),
                responses::ev_completed("unused"),
            ]),
        )
        .await;
        let unused_done_response = mount_sse_once(
            &responses_server,
            responses::sse(vec![
                responses::ev_response_created("unused-done"),
                responses::ev_assistant_message("unused-message", "done"),
                responses::ev_completed("unused-done"),
            ]),
        )
        .await;
        second_thread
            .start_or_steer_turn(user_turn("Do not call any MCP tools."))
            .await?;
        let mut reported_ready_before_startup = false;
        wait_for_event(&second_thread, |event| {
            if let EventMsg::McpStartupUpdate(update) = event
                && update.server == SERVER_NAME
                && matches!(update.status, McpStartupStatus::Ready)
            {
                reported_ready_before_startup = true;
            }
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        assert!(
            !reported_ready_before_startup,
            "a dormant MCP server must not be reported as ready"
        );
        for response in [&unused_response, &unused_done_response] {
            assert_definition(
                response,
                &format!("Use the tools from {cached_process}."),
                &format!("Echo from {cached_process}."),
            );
        }
        assert_eq!(
            binding_captures.load(Ordering::SeqCst),
            1,
            "both model steps must share one binding while the cached server stays dormant"
        );
        let (mcp_config, _) = second_thread.current_mcp_config_and_runtime_context().await;
        assert_eq!(
            second_thread
                .mcp_connection_statuses(&mcp_config)
                .await
                .get(SERVER_NAME),
            Some(&McpServerConnectionStatus::NotStarted),
        );
        assert_eq!(
            fs.read_file_text(&pid_file, Default::default(), /*sandbox*/ None)
                .await?
                .trim(),
            eager_pid,
            "cached tool definitions should not start an unused subagent-owned MCP process"
        );

        let app_only_call_id = "cached-app-only-call";
        let unrelated_call_id = "cached-unrelated-plan";
        let cached_response = mount_sse_once(
            &responses_server,
            responses::sse(vec![
                responses::ev_response_created("cached-call"),
                responses::ev_function_call_with_namespace(
                    "cached-call",
                    NAMESPACE,
                    "echo",
                    r#"{"message":"hello"}"#,
                ),
                responses::ev_function_call_with_namespace(
                    app_only_call_id,
                    NAMESPACE,
                    "cwd",
                    "{}",
                ),
                responses::ev_function_call(
                    unrelated_call_id,
                    "update_plan",
                    r#"{"plan":[{"step":"Continue while MCP starts","status":"in_progress"}]}"#,
                ),
                responses::ev_completed("cached-call"),
            ]),
        )
        .await;
        let cached_done_response = mount_sse_once(
            &responses_server,
            responses::sse(vec![
                responses::ev_response_created("cached-done"),
                responses::ev_assistant_message("cached-message", "done"),
                responses::ev_completed("cached-done"),
            ]),
        )
        .await;
        let (unrelated_finished_tx, unrelated_finished_rx) = tokio::sync::oneshot::channel();
        let second_for_turn = Arc::clone(&second_thread);
        let cached_turn = spawn_owned_observer(async move {
            second_for_turn
                .start_or_steer_turn(user_turn("call the echo and cwd tools"))
                .await?;
            if root_shutdown {
                let mut signal = Some(unrelated_finished_tx);
                let mut policy_denial = None;
                wait_for_event(&second_for_turn, |event| {
                    if matches!(event, EventMsg::PlanUpdate(_))
                        && let Some(sender) = signal.take()
                    {
                        let _ = sender.send(());
                    }
                    if let EventMsg::Error(error) = event {
                        if error.message.contains("child_provider_policy_conflict") {
                            policy_denial = Some(error.message.clone());
                        }
                    }
                    if let EventMsg::McpToolCallEnd(end) = event
                        && end.call_id == "cached-call"
                        && let Err(error) = &end.result
                    {
                        let error = error.to_string();
                        if error.contains("child_provider_policy_conflict") {
                            policy_denial = Some(error);
                        }
                    }
                    matches!(event, EventMsg::TurnComplete(_))
                })
                .await;
                return anyhow::Ok((None, policy_denial));
            }
            let mut unrelated_finished_tx = Some(unrelated_finished_tx);
            let mut saw_starting = false;
            let mut saw_ready = false;
            let end = wait_for_event(&second_for_turn, |event| {
                if matches!(event, EventMsg::PlanUpdate(_))
                    && let Some(sender) = unrelated_finished_tx.take()
                {
                    let _ = sender.send(());
                }
                if let EventMsg::McpStartupUpdate(update) = event
                    && update.server == SERVER_NAME
                {
                    saw_starting |= matches!(update.status, McpStartupStatus::Starting);
                    saw_ready |= matches!(update.status, McpStartupStatus::Ready);
                }
                matches!(
                    event,
                    EventMsg::McpToolCallEnd(end) if end.call_id == "cached-call"
                )
            })
            .await;
            assert!(
                saw_starting,
                "deferred startup should emit its starting status"
            );
            assert!(saw_ready, "deferred startup should emit its ready status");
            let EventMsg::McpToolCallEnd(end) = end else {
                unreachable!("event predicate guarantees an MCP tool result");
            };
            let called_process = end
                .result
                .expect("echo call should succeed")
                .structured_content
                .and_then(|content| content.get("echo").cloned())
                .and_then(|echo| echo.as_str().map(ToString::to_string))
                .expect("echo result should identify its live server process");
            wait_for_event(&second_for_turn, |event| {
                matches!(event, EventMsg::TurnComplete(_))
            })
            .await;
            anyhow::Ok((Some(called_process), None))
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while cached_response.requests().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("cached MCP definitions should reach inference before initialization")?;
        assert_definition(
            &cached_response,
            &format!("Use the tools from {cached_process}."),
            &format!("Echo from {cached_process}."),
        );
        let second_pid = wait_for_new_pid(fs.as_ref(), &pid_file, Some(&eager_pid)).await?;
        let second_process = process_label(&second_pid);
        tokio::time::timeout(Duration::from_secs(2), unrelated_finished_rx)
            .await
            .context("an unrelated tool should complete while cached MCP startup is pending")?
            .context("the unrelated tool should emit its plan update")?;

        let parent_mcp_shutdown_facts = if root_shutdown {
            fixture.codex.shutdown_and_wait().await?;
            None
        } else {
            let (parent_mcp_config, _) =
                fixture.codex.current_mcp_config_and_runtime_context().await;
            // Close only the actual parent MCP runtime. Native ownership stays live.
            fixture.codex.session.services.mcp_runtime.shutdown().await;
            let cancelled = fixture
                .codex
                .mcp_connection_statuses(&parent_mcp_config)
                .await
                .get(SERVER_NAME)
                == Some(&McpServerConnectionStatus::Cancelled);
            let live = fixture.codex.is_running()
                && *fixture
                    .codex
                    .session
                    .fresh_child_admission_live
                    .lock()
                    .unwrap();
            let revision_unchanged =
                *fixture.codex.session.child_policy_revision.lock().unwrap() == root_revision;
            let same_allocation = fixture
                .thread_manager
                .get_thread(fixture.session_configured.thread_id)
                .await
                .ok()
                .is_some_and(|parent| Arc::ptr_eq(&fixture.codex, &parent));
            let current = fixture
                .thread_manager
                .agent_control()
                .runtime
                .with_resume_caller(&root_epoch, /*live*/ live, || Ok(()))
                .is_ok();
            Some((
                cancelled,
                live,
                revision_unchanged,
                same_allocation,
                current,
            ))
        };
        fs.write_file(
            &barrier_file,
            b"ready".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        let (called_process, policy_denial) = cached_turn.await??;
        if root_shutdown {
            let first_step_count = cached_response.requests().len();
            let next_step_absent = cached_done_response.requests().is_empty();
            let local_history = second_thread.conversation_history_snapshot().await;
            let history_collectible = local_history.review_items().next().is_some();
            second_thread.shutdown_and_wait().await?;
            // Release/await the gate and complete owned cleanup before causal assertions.
            assert!(
                policy_denial.is_some(),
                "root shutdown must produce specific native policy denial, not a timeout"
            );
            assert_eq!(
                first_step_count, 1,
                "the first actual child model step already ran"
            );
            assert!(
                next_step_absent,
                "no forbidden followup model request after root shutdown"
            );
            assert!(
                history_collectible,
                "purely local history remains collectible after root shutdown"
            );
            responses_server.verify().await;
            return Ok(());
        }
        assert_eq!(
            parent_mcp_shutdown_facts,
            Some((true, true, true, true, true)),
            "actual parent MCP cancellation preserves live current native root ownership"
        );
        assert!(policy_denial.is_none());
        let expected_error = format!("MCP tool `{SERVER_NAME}/cwd` is not available to the model");
        assert_eq!(
            called_process.expect("successful native child MCP result"),
            second_process
        );
        assert!(
            binding_captures.load(Ordering::SeqCst) > 1,
            "starting the cached server must invalidate the dormant binding"
        );
        assert_definition(
            &cached_done_response,
            &format!("Use the tools from {second_process}."),
            &format!("Echo from {second_process}."),
        );
        let output_item = cached_done_response
            .single_request()
            .function_call_output(app_only_call_id);
        let output = output_item["output"][1]["text"]
            .as_str()
            .expect("app-only tool error should be returned to the model");
        assert!(
            output.contains(&expected_error),
            "model-visible tool output should contain the live visibility error: {output}"
        );
        let output = cached_done_response
            .single_request()
            .function_call_output_text("cached-call")
            .expect("successful tool output should be returned to the model");
        assert!(
            output.contains(&second_process),
            "model-visible tool output should come from the live server: {output}"
        );
        assert_eq!(
            cached_done_response
                .single_request()
                .function_call_output_text(unrelated_call_id)
                .as_deref(),
            Some("Plan updated")
        );

        second_thread.shutdown_and_wait().await?;
        let mut filtered_config = fixture.config.clone();
        let mut filtered_servers = filtered_config.mcp_servers.get().clone();
        filtered_servers
            .get_mut(SERVER_NAME)
            .expect("cached MCP server should remain configured")
            .enabled_tools = Some(vec!["cwd".to_string()]);
        filtered_config.mcp_servers.set(filtered_servers)?;
        let NewThread {
            thread: filtered_thread,
            ..
        } = fixture
            .thread_manager
            .start_thread(StartThreadOptions {
                session_source: Some(SessionSource::SubAgent(SubAgentSource::Other(
                    "filtered-cached-startup".to_string(),
                ))),
                ..StartThreadOptions::new(filtered_config)
            })
            .await?;
        retain_thread(&filtered_thread);
        let filtered_response = mount_sse_once(
            &responses_server,
            responses::sse(vec![
                responses::ev_response_created("filtered"),
                responses::ev_assistant_message("filtered-message", "done"),
                responses::ev_completed("filtered"),
            ]),
        )
        .await;
        filtered_thread
            .start_or_steer_turn(user_turn("Do not use tools."))
            .await?;
        wait_for_event(&filtered_thread, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        assert!(
            responses::namespace_child_tool(
                &filtered_response.single_request().body_json(),
                NAMESPACE,
                "cwd",
            )
            .is_none()
        );
        let (mcp_config, _) = filtered_thread
            .current_mcp_config_and_runtime_context()
            .await;
        assert_eq!(
            filtered_thread
                .mcp_connection_statuses(&mcp_config)
                .await
                .get(SERVER_NAME),
            Some(&McpServerConnectionStatus::NotStarted),
        );
        let result = filtered_thread
            .call_mcp_tool(SERVER_NAME, "cwd", Some(json!({})), /*meta*/ None)
            .await?;
        assert_ne!(result.is_error, Some(true));
        let filtered_pid = wait_for_new_pid(fs.as_ref(), &pid_file, Some(&second_pid)).await?;
        filtered_thread.shutdown_and_wait().await?;
        fs.remove(
            &barrier_file,
            RemoveOptions {
                recursive: false,
                force: false,
                follow_symlinks: true,
            },
            /*sandbox*/ None,
        )
        .await?;
        let NewThread {
            thread: interrupted_thread,
            ..
        } = fixture
            .thread_manager
            .start_thread(StartThreadOptions {
                session_source: Some(SessionSource::SubAgent(SubAgentSource::Other(
                    "interrupted-cached-startup".to_string(),
                ))),
                ..StartThreadOptions::new(fixture.config.clone())
            })
            .await?;
        retain_thread(&interrupted_thread);
        mount_sse_once(
            &responses_server,
            responses::sse(vec![
                responses::ev_response_created("interrupted-startup"),
                responses::ev_function_call_with_namespace(
                    "interrupted-startup-call",
                    NAMESPACE,
                    "echo",
                    r#"{"message":"interrupted"}"#,
                ),
                responses::ev_completed("interrupted-startup"),
            ]),
        )
        .await;
        interrupted_thread
            .start_or_steer_turn(user_turn("Start the cached MCP tool."))
            .await?;
        let interrupted_pid = wait_for_new_pid(fs.as_ref(), &pid_file, Some(&filtered_pid)).await?;
        wait_for_event(&interrupted_thread, |event| {
            matches!(
                event,
                EventMsg::McpStartupUpdate(update)
                    if update.server == SERVER_NAME
                        && matches!(update.status, McpStartupStatus::Starting)
            )
        })
        .await;
        interrupted_thread.submit(Op::Interrupt).await?;
        wait_for_event(&interrupted_thread, |event| {
            matches!(event, EventMsg::TurnAborted(_))
        })
        .await;
        fs.write_file(
            &barrier_file,
            b"ready".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_event(&interrupted_thread, |event| {
                matches!(
                    event,
                    EventMsg::McpStartupUpdate(update)
                        if update.server == SERVER_NAME
                            && matches!(update.status, McpStartupStatus::Ready)
                )
            }),
        )
        .await
        .context("deferred MCP startup should survive an interrupted first tool call")?;
        mount_sse_once(
            &responses_server,
            responses::sse(vec![
                responses::ev_response_created("retried-startup"),
                responses::ev_function_call_with_namespace(
                    "retried-startup-call",
                    NAMESPACE,
                    "echo",
                    r#"{"message":"retried"}"#,
                ),
                responses::ev_completed("retried-startup"),
            ]),
        )
        .await;
        let retry_done = mount_sse_once(
            &responses_server,
            responses::sse(vec![
                responses::ev_response_created("retried-done"),
                responses::ev_assistant_message("retried-message", "done"),
                responses::ev_completed("retried-done"),
            ]),
        )
        .await;
        interrupted_thread
            .start_or_steer_turn(user_turn("Retry the cached MCP tool."))
            .await?;
        wait_for_event(&interrupted_thread, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        let retry_output = retry_done
            .single_request()
            .function_call_output_text("retried-startup-call")
            .expect("the retried MCP tool call should return its output");
        assert!(
            retry_output.contains(&process_label(&interrupted_pid)),
            "the retry should use the server started by the interrupted call"
        );
        interrupted_thread.shutdown_and_wait().await?;
        responses_server.verify().await;
        fixture.codex.shutdown_and_wait().await?;
        Ok(())
    }
}
