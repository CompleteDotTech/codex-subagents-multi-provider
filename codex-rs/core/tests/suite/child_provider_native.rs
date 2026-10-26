//! Real native tool dispatch, provider routing, and rejection-before-I/O coverage.
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_models_manager::bundled_models_response;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_with_timeout;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn body(request: &Request) -> Value {
    let compressed = request
        .headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        == Some("zstd");
    let bytes = if compressed {
        zstd::stream::decode_all(request.body.as_slice()).unwrap()
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&bytes).unwrap()
}

fn completion(id: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(sse(vec![
            ev_response_created(id),
            ev_assistant_message(id, "done"),
            ev_completed(id),
        ]))
}

fn spawn_response(version: MultiAgentVersion, id: &str, mut args: Value) -> ResponseTemplate {
    let namespace = if version == MultiAgentVersion::V2 {
        "collaboration"
    } else {
        "multi_agent_v1"
    };
    if version == MultiAgentVersion::V2 {
        args["task_name"] = json!(id);
        if args.get("fork_turns").is_none() {
            args["fork_turns"] = json!("none");
        }
    } else if args.get("fork_context").is_none() {
        args["fork_context"] = json!(false);
    }
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(sse(vec![
            ev_response_created(id),
            ev_function_call_with_namespace(id, namespace, "spawn_agent", &args.to_string()),
            ev_completed(id),
        ]))
}

async fn scripted_spawn(server: &MockServer, version: MultiAgentVersion, id: &str, args: Value) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(spawn_response(version, id, args))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(completion("complete"))
        .mount(server)
        .await;
}

struct Fixture {
    root: MockServer,
    a: MockServer,
    b: MockServer,
    test: TestCodex,
    home: Arc<TempDir>,
}

impl Fixture {
    async fn new(version: MultiAgentVersion, default_provider: Option<&str>) -> Result<Self> {
        Self::with_host_setup(version, default_provider, |_| {}).await
    }

    async fn with_host_setup(
        version: MultiAgentVersion,
        default_provider: Option<&str>,
        setup: fn(&mut codex_core::config::Config),
    ) -> Result<Self> {
        let root = start_mock_server().await;
        let a = start_mock_server().await;
        let b = start_mock_server().await;
        let home = Arc::new(tempfile::tempdir()?);
        let mut info = bundled_models_response()?
            .models
            .into_iter()
            .find(|m| m.slug == "gpt-5.5")
            .unwrap();
        info.slug = "shared".into();
        info.multi_agent_version = Some(version);
        // Exercise native direct tools; deferred tool_search advertisement is
        // covered by the separate Responses capability suites.
        info.supports_search_tool = false;
        info.used_fallback_model_metadata = false;
        std::fs::write(
            home.path().join("catalog.json"),
            serde_json::to_vec(&json!({"models":[info]}))?,
        )?;
        std::fs::write(
            home.path().join("role.toml"),
            "model_provider = 'b'\nmodel = 'shared'\n",
        )?;
        let helper = home.path().join(if cfg!(windows) {
            "blocked.cmd"
        } else {
            "blocked.sh"
        });
        std::fs::write(
            &helper,
            if cfg!(windows) {
                "@echo off\r\necho invoked>helper-marker\r\necho synthetic-blocked\r\n"
            } else {
                "touch helper-marker\nprintf synthetic-blocked\n"
            },
        )?;
        let helper_auth = if cfg!(windows) {
            json!({"command":"cmd.exe","args":["/D","/C",helper],"cwd":home.path(),"timeout_ms":5000,"refresh_interval_ms":0})
        } else {
            json!({"command":"sh","args":[helper],"cwd":home.path(),"timeout_ms":5000,"refresh_interval_ms":0})
        };
        let default = default_provider
            .map(|id| format!("default_subagent_model_provider = '{id}'\n"))
            .unwrap_or_default();
        let config = format!(
            r#"
[agents]
allowed_model_providers = ["a", "b", "openai"]
{default}
[agents.provider_defaults.a]
model = "shared"
model_catalog_json = "catalog.json"
[agents.provider_defaults.b]
model = "shared"
model_catalog_json = "catalog.json"
[agents.provider_defaults.blocked]
model = "shared"
model_catalog_json = "catalog.json"
[agents.provider_role]
description = "A bounded provider role"
config_file = "role.toml"
[model_providers.a]
name = "First native provider"
base_url = "{}/v1"
experimental_bearer_token = "synthetic-a"
request_max_retries = 0
[model_providers.b]
name = "Second native provider"
base_url = "{}/v1"
experimental_bearer_token = "synthetic-b"
request_max_retries = 0
[model_providers.blocked]
name = "Unauthorized helper provider"
base_url = "{}/v1"
request_max_retries = 0
"#,
            a.uri(),
            b.uri(),
            b.uri()
        );
        let mut config: toml::Value = toml::from_str(&config)?;
        config["model_providers"]["blocked"]["auth"] = toml::Value::try_from(helper_auth)?;
        std::fs::write(home.path().join("config.toml"), toml::to_string(&config)?)?;
        let mut builder = test_codex()
            .with_home(home.clone())
            .with_auth(CodexAuth::from_api_key("synthetic-root"))
            .with_model_info_override("gpt-5.5", move |info| {
                info.multi_agent_version = Some(version);
                info.supports_search_tool = false;
            })
            .with_config(move |config| {
                config.features.enable(Feature::Collab).unwrap();
                if version == MultiAgentVersion::V2 {
                    config.features.enable(Feature::MultiAgentV2).unwrap();
                } else {
                    config.features.disable(Feature::MultiAgentV2).unwrap();
                }
                config.agent_max_depth = 5;
                setup(config);
            });
        let test = builder.build_with_auto_env(&root).await?;
        Ok(Self {
            root,
            a,
            b,
            test,
            home,
        })
    }

    async fn turn(&self, text: &str) -> Result<()> {
        self.test
            .codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: text.into(),
                text_elements: vec![],
            }]))
            .await?;
        let EventMsg::TurnComplete(done) = wait_for_event_with_timeout(
            &self.test.codex,
            |event| matches!(event, EventMsg::TurnComplete(_)),
            Duration::from_secs(30),
        )
        .await
        else {
            unreachable!()
        };
        assert!(
            done.error.is_none(),
            "root turn must succeed: {:?}",
            done.error
        );
        Ok(())
    }

    async fn children(&self, expected: usize) -> Result<Vec<Arc<codex_core::CodexThread>>> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let ids = self.test.thread_manager.list_thread_ids().await;
                if ids.len() >= expected + 1 {
                    let mut children = vec![];
                    for id in ids {
                        if id != self.test.session_configured.thread_id {
                            children.push(self.test.thread_manager.get_thread(id).await?);
                        }
                    }
                    return Ok(children);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?
    }

    async fn assert_provider_headers(&self) {
        let mut expected_a = std::collections::HashSet::new();
        let mut expected_b = std::collections::HashSet::new();
        for id in self.test.thread_manager.list_thread_ids().await {
            let child = self.test.thread_manager.get_thread(id).await.unwrap();
            match child.config_snapshot().await.model_provider_id.as_str() {
                "a" => {
                    expected_a.insert(id.to_string());
                }
                "b" => {
                    expected_b.insert(id.to_string());
                }
                _ => {}
            }
        }
        // Registration precedes the child's first request. Wait for the wire
        // evidence for each actual child identity, so a parent's second request
        // cannot stand in for a nested child's first request.
        for (server, expected) in [(&self.a, expected_a), (&self.b, expected_b)] {
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let requests = server
                        .received_requests()
                        .await
                        .expect("mock request recording must be enabled");
                    let observed = requests
                        .iter()
                        .filter(|request| request.method.as_str() == "POST")
                        .filter_map(|request| request.headers.get("thread-id"))
                        .filter_map(|value| value.to_str().ok())
                        .map(str::to_owned)
                        .collect::<std::collections::HashSet<_>>();
                    if expected.iter().all(|id| observed.contains(id)) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("selected provider receives each native child request");
        }
        for (server, token) in [
            (&self.a, "Bearer synthetic-a"),
            (&self.b, "Bearer synthetic-b"),
        ] {
            for request in server.received_requests().await.unwrap() {
                if request.method.as_str() != "POST" {
                    continue;
                }
                assert_eq!(request.headers.get("authorization").unwrap(), token);
                assert!(!request.headers.contains_key("chatgpt-account-id"));
                assert!(!request.headers.contains_key("x-openai-actor-id"));
                assert_eq!(body(&request)["model"], "shared");
            }
        }
        for request in self.root.received_requests().await.unwrap() {
            if request.method.as_str() == "POST" {
                assert_eq!(
                    request.headers.get("authorization").unwrap(),
                    "Bearer synthetic-root"
                );
                assert_eq!(body(&request)["model"], "gpt-5.5");
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_and_builtin_role_native_children_select_provider_in_both_backends() -> Result<()>
{
    for version in [MultiAgentVersion::V1, MultiAgentVersion::V2] {
        for role in [None, Some("default")] {
            let fixture = Fixture::new(version, None).await?;
            let mut args = json!({"message":"ordinary child", "model_provider":"a"});
            if let Some(role) = role {
                args["agent_type"] = json!(role);
            }
            scripted_spawn(&fixture.root, version, "worker", args).await;
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(completion("a-done"))
                .mount(&fixture.a)
                .await;
            fixture.turn("delegate an ordinary child").await?;
            let children = fixture.children(1).await?;
            assert_eq!(children.len(), 1);
            assert_eq!(children[0].config_snapshot().await.model_provider_id, "a");
            assert_eq!(
                fixture.test.codex.config_snapshot().await.model_provider_id,
                "openai"
            );
            fixture.turn("parent subsequent turn").await?;
            fixture.assert_provider_headers().await;
            assert!(
                !fixture
                    .a
                    .received_requests()
                    .await
                    .expect("mock request recording must be enabled")
                    .is_empty()
            );
            assert!(
                fixture
                    .b
                    .received_requests()
                    .await
                    .expect("mock request recording must be enabled")
                    .is_empty()
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_native_omission_inherits_parent_and_explicit_override_selects_another_provider()
-> Result<()> {
    for version in [MultiAgentVersion::V1, MultiAgentVersion::V2] {
        for target in [None, Some("b")] {
            let fixture = Fixture::new(version, None).await?;
            scripted_spawn(
                &fixture.root,
                version,
                "worker",
                json!({"message":"delegate a nested child", "model_provider":"a"}),
            )
            .await;
            let mut nested = json!({"message":"nested ordinary child"});
            if let Some(target) = target {
                nested["model_provider"] = json!(target);
            }
            scripted_spawn(&fixture.a, version, "grandchild", nested).await;
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(completion("b-done"))
                .mount(&fixture.b)
                .await;
            fixture.turn("delegate a nested tree").await?;
            let children = fixture.children(2).await?;
            assert_eq!(children.len(), 2);
            let mut providers = vec![];
            for child in children {
                providers.push(child.config_snapshot().await.model_provider_id);
            }
            providers.sort();
            assert_eq!(
                providers,
                if target.is_some() {
                    vec!["a", "b"]
                } else {
                    vec!["a", "a"]
                }
            );
            fixture.turn("parent remains usable").await?;
            fixture.assert_provider_headers().await;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_selector_beats_role_provider_and_configured_default_applies_at_each_depth()
-> Result<()> {
    for default in [None, Some("b")] {
        let fixture = Fixture::new(MultiAgentVersion::V2, default).await?;
        scripted_spawn(
            &fixture.root,
            MultiAgentVersion::V2,
            "worker",
            json!({"message":"nested task", "model_provider":"a", "agent_type":"provider_role"}),
        )
        .await;
        scripted_spawn(
            &fixture.a,
            MultiAgentVersion::V2,
            "grandchild",
            json!({"message":"nested ordinary task"}),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(completion("b-done"))
            .mount(&fixture.b)
            .await;
        fixture
            .turn("delegate with explicit provider and a role")
            .await?;
        let children = fixture.children(2).await?;
        let mut providers = vec![];
        for child in children {
            providers.push(child.config_snapshot().await.model_provider_id);
        }
        providers.sort();
        assert_eq!(
            providers,
            if default.is_some() {
                vec!["a", "b"]
            } else {
                vec!["a", "a"]
            }
        );
        fixture.assert_provider_headers().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_provider_is_resolved_by_both_native_backends_without_explicit_selector() -> Result<()>
{
    for version in [MultiAgentVersion::V1, MultiAgentVersion::V2] {
        let fixture = Fixture::new(version, Some("a")).await?;
        scripted_spawn(
            &fixture.root,
            version,
            "role_worker",
            json!({"message":"role-selected child", "agent_type":"provider_role"}),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(completion("role-done"))
            .mount(&fixture.b)
            .await;
        fixture.turn("delegate using role provider").await?;
        let children = fixture.children(1).await?;
        assert_eq!(children[0].config_snapshot().await.model_provider_id, "b");
        fixture.assert_provider_headers().await;
        assert!(
            fixture
                .a
                .received_requests()
                .await
                .expect("mock request recording must be enabled")
                .is_empty()
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_history_and_authority_rejections_happen_before_catalog_helper_or_child_network()
-> Result<()> {
    for version in [MultiAgentVersion::V1, MultiAgentVersion::V2] {
        for (provider, history, expected) in [
            (
                "a",
                Some(json!("all")),
                "child_provider_history_unsupported",
            ),
            ("a", Some(json!("1")), "child_provider_history_unsupported"),
            ("blocked", None, "child_provider_not_authorized"),
            ("missing", None, "child_provider_not_authorized"),
        ] {
            let fixture = Fixture::new(version, None).await?;
            let mut args = json!({"message":"rejected native child", "model_provider":provider});
            if let Some(history) = history {
                if version == MultiAgentVersion::V2 {
                    args["fork_turns"] = history;
                } else {
                    args["fork_context"] = json!(true);
                }
                std::fs::remove_file(fixture.home.path().join("catalog.json"))?;
            }
            scripted_spawn(&fixture.root, version, "rejected", args).await;
            fixture.turn("try a rejected delegation").await?;
            assert_eq!(fixture.test.thread_manager.list_thread_ids().await.len(), 1);
            assert!(
                fixture
                    .a
                    .received_requests()
                    .await
                    .expect("mock request recording must be enabled")
                    .is_empty()
            );
            assert!(
                fixture
                    .b
                    .received_requests()
                    .await
                    .expect("mock request recording must be enabled")
                    .is_empty()
            );
            assert!(!fixture.home.path().join("helper-marker").exists());
            let requests = fixture
                .root
                .received_requests()
                .await
                .expect("mock request recording must be enabled");
            assert!(
                requests
                    .iter()
                    .filter(|request| request.method.as_str() == "POST")
                    .any(|request| body(request).to_string().contains(expected)),
                "expected explicit native rejection {expected}"
            );
        }
    }
    Ok(())
}

fn allow_helper_provider_setup(config: &mut codex_core::config::Config) {
    let stack = &config.config_layer_stack;
    let mut layers = stack.layers_low_to_high().cloned().collect::<Vec<_>>();
    for layer in &mut layers {
        if matches!(layer.name, codex_config::ConfigLayerSource::User { .. }) {
            layer.config["agents"]["allowed_model_providers"]
                .as_array_mut()
                .unwrap()
                .push(toml::Value::String("blocked".into()));
        }
    }
    config.config_layer_stack = codex_config::ConfigLayerStack::new(
        layers,
        stack.requirements().clone(),
        stack.requirements_toml().clone(),
    )
    .unwrap();
}

fn managed_provider_setup(config: &mut codex_core::config::Config) {
    let stack = &config.config_layer_stack;
    let mut required = stack.requirements_toml().clone();
    required.model_provider = Some("openai".into());
    config.config_layer_stack = codex_config::ConfigLayerStack::new(
        stack.layers_low_to_high().cloned().collect(),
        stack.requirements().clone(),
        required,
    )
    .unwrap();
}

fn managed_allowlist_setup(config: &mut codex_core::config::Config) {
    let stack = &config.config_layer_stack;
    let mut layers = stack.layers_low_to_high().cloned().collect::<Vec<_>>();
    layers.push(codex_config::ConfigLayerEntry::new(
        codex_config::ConfigLayerSource::Mdm {
            domain: "native-test".into(),
            key: "providers".into(),
        },
        toml::from_str("[agents]\nallowed_model_providers=['b']").unwrap(),
    ));
    layers.sort_by(|left, right| left.name.partial_cmp(&right.name).unwrap());
    config.config_layer_stack = codex_config::ConfigLayerStack::new(
        layers,
        stack.requirements().clone(),
        stack.requirements_toml().clone(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_managed_constraints_override_user_authorization_before_child_io() -> Result<()> {
    for version in [MultiAgentVersion::V1, MultiAgentVersion::V2] {
        for (setup, expected) in [
            (
                managed_provider_setup as fn(&mut codex_core::config::Config),
                "child_provider_policy_conflict",
            ),
            (
                managed_allowlist_setup as fn(&mut codex_core::config::Config),
                "child_provider_not_authorized",
            ),
        ] {
            let fixture = Fixture::with_host_setup(version, None, setup).await?;
            std::fs::remove_file(fixture.home.path().join("catalog.json"))?;
            scripted_spawn(
                &fixture.root,
                version,
                "managed_rejection",
                json!({"message":"disallowed by managed policy", "model_provider":"a"}),
            )
            .await;
            fixture.turn("try managed-restricted delegation").await?;
            assert_eq!(fixture.test.thread_manager.list_thread_ids().await.len(), 1);
            assert!(
                fixture
                    .a
                    .received_requests()
                    .await
                    .expect("mock request recording must be enabled")
                    .is_empty()
            );
            assert!(
                fixture
                    .b
                    .received_requests()
                    .await
                    .expect("mock request recording must be enabled")
                    .is_empty()
            );
            assert!(!fixture.home.path().join("helper-marker").exists());
            assert!(
                fixture
                    .root
                    .received_requests()
                    .await
                    .expect("mock request recording must be enabled")
                    .iter()
                    .filter(|request| request.method.as_str() == "POST")
                    .any(|request| body(request).to_string().contains(expected))
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_runtime_cwd_and_write_scope_revoke_host_file_authority() -> Result<()> {
    for version in [MultiAgentVersion::V1, MultiAgentVersion::V2] {
        for change_cwd in [true, false] {
            let fixture =
                Fixture::with_host_setup(version, None, allow_helper_provider_setup).await?;
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(completion("baseline"))
                .mount(&fixture.root)
                .await;
            fixture
                .turn("advertise initially trusted providers")
                .await?;
            assert!(
                fixture
                    .root
                    .received_requests()
                    .await
                    .expect("mock request recording must be enabled")
                    .iter()
                    .filter(|request| request.method.as_str() == "POST")
                    .any(|request| body(request)["tools"].to_string().contains("blocked"))
            );
            fixture.root.reset().await;
            let mut overrides = codex_protocol::protocol::ThreadSettingsOverrides::default();
            if change_cwd {
                // Moving cwd above the host file makes it project-owned even
                // without widening the permission profile.
                let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
                    fixture.home.path(),
                )?;
                let uri = codex_utils_path_uri::PathUri::from_abs_path(&cwd);
                let requests = fixture
                    .test
                    .codex
                    .environment_selections()
                    .await
                    .into_iter()
                    .map(|selection| {
                        let mut request = selection.into_request();
                        request.cwd = uri.clone();
                        request.workspace_roots = vec![uri.clone()];
                        request
                    })
                    .collect();
                overrides.environments = Some(
                    codex_protocol::protocol::TurnEnvironmentRequests::new(cwd.clone(), requests),
                );
                overrides.runtime_workspace_roots = Some(vec![cwd]);
            } else {
                overrides.permission_profile =
                    Some(codex_protocol::models::PermissionProfile::Disabled);
            }
            fixture.test.codex.update_thread_settings(overrides).await?;
            // Source trust must fail before attempting even a catalog read.
            std::fs::remove_file(fixture.home.path().join("catalog.json"))?;
            scripted_spawn(&fixture.root, version, "scope_rejection",
                json!({"message":"source became writable/project-owned", "model_provider":"blocked"})).await;
            // blocked owns an executable helper, providing a direct before-I/O marker.
            fixture
                .turn("try delegation after runtime scope change")
                .await?;
            assert_eq!(fixture.test.thread_manager.list_thread_ids().await.len(), 1);
            assert!(
                fixture
                    .a
                    .received_requests()
                    .await
                    .expect("mock request recording must be enabled")
                    .is_empty()
            );
            assert!(
                fixture
                    .b
                    .received_requests()
                    .await
                    .expect("mock request recording must be enabled")
                    .is_empty()
            );
            assert!(!fixture.home.path().join("helper-marker").exists());
            assert!(
                fixture
                    .root
                    .received_requests()
                    .await
                    .expect("mock request recording must be enabled")
                    .iter()
                    .filter(|request| request.method.as_str() == "POST")
                    .any(|request| {
                        let text = body(request).to_string();
                        text.contains("child_provider_not_authorized")
                            || text.contains("child_provider_untrusted_source")
                    })
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_resumed_child_history_cannot_be_reclassified_by_source_labels() -> Result<()> {
    use codex_core::StartThreadOptions;
    use codex_history::InitialHistory;
    use codex_history::ResumedHistory;
    use codex_history::RolloutItem;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::InternalSessionSource;
    use codex_protocol::protocol::SessionMeta;
    use codex_protocol::protocol::SessionMetaLine;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    let fixture = Fixture::with_host_setup(MultiAgentVersion::V2, None, |config| {
        config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    })
    .await?;
    let parent = fixture.test.session_configured.thread_id;
    let native = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: parent,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    for source in [
        SessionSource::Internal(InternalSessionSource::Guardian),
        SessionSource::Internal(InternalSessionSource::MemoryConsolidation),
        SessionSource::Exec,
        SessionSource::SubAgent(SubAgentSource::Review),
    ] {
        let child = ThreadId::new();
        let history = InitialHistory::Resumed(ResumedHistory {
            conversation_id: child,
            history_revision: None,
            rollout_path: None,
            history: Arc::new(vec![RolloutItem::SessionMeta(Box::new(SessionMetaLine {
                meta: SessionMeta {
                    id: child,
                    parent_thread_id: Some(parent),
                    source: native.clone(),
                    model_provider: Some("blocked".into()),
                    ..Default::default()
                },
                git: None,
            }))]),
        });
        let mut config = fixture.test.config.clone();
        config.model_provider_id = "blocked".into();
        config.model_provider = config.model_providers["blocked"].clone();
        let result = fixture
            .test
            .thread_manager
            .start_thread(StartThreadOptions {
                initial_history: history,
                session_source: Some(source),
                ..StartThreadOptions::new(config)
            })
            .await;
        assert!(
            result.is_err_and(|error| error.to_string().contains("private native launch path"))
        );
        assert_eq!(fixture.test.thread_manager.list_thread_ids().await.len(), 1);
        assert!(!fixture.home.path().join("helper-marker").exists());
        assert!(
            fixture
                .a
                .received_requests()
                .await
                .expect("mock request recording must be enabled")
                .is_empty()
        );
        assert!(
            fixture
                .b
                .received_requests()
                .await
                .expect("mock request recording must be enabled")
                .is_empty()
        );
    }
    let internal = fixture
        .test
        .thread_manager
        .spawn_internal_session(
            parent,
            StartThreadOptions {
                session_source: Some(SessionSource::Internal(
                    InternalSessionSource::MemoryConsolidation,
                )),
                ..StartThreadOptions::new(fixture.test.config.clone())
            },
        )
        .await?;
    assert!(matches!(
        internal.thread.config_snapshot().await.session_source,
        SessionSource::Internal(InternalSessionSource::MemoryConsolidation)
    ));
    internal.thread.shutdown_and_wait().await?;
    assert_eq!(fixture.test.thread_manager.list_thread_ids().await.len(), 1);
    assert!(!fixture.home.path().join("helper-marker").exists());
    // A subsequent real child demonstrates that rejected attempts did not
    // consume the controller's child slot or corrupt its parent registration.
    scripted_spawn(
        &fixture.root,
        MultiAgentVersion::V2,
        "real_child",
        json!({"message":"valid child", "model_provider":"a"}),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(completion("valid-done"))
        .mount(&fixture.a)
        .await;
    fixture.turn("delegate after forged attempts").await?;
    assert_eq!(fixture.children(1).await?.len(), 1);
    fixture.assert_provider_headers().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_provider_does_not_replace_an_incompatible_role_model_or_fall_back() -> Result<()>
{
    for version in [MultiAgentVersion::V1, MultiAgentVersion::V2] {
        let fixture = Fixture::with_host_setup(version, None, |config| {
            std::fs::write(
                config.codex_home.join("role.toml"),
                "model_provider='b'\nmodel='role-only-b'\n",
            )
            .unwrap();
        })
        .await?;
        scripted_spawn(
            &fixture.root,
            version,
            "role_model_rejection",
            json!({"message":"incompatible role model", "agent_type":"provider_role",
                "model_provider":"a", "model":"shared"}),
        )
        .await;
        fixture
            .turn("select provider A while retaining role model precedence")
            .await?;
        assert_eq!(fixture.test.thread_manager.list_thread_ids().await.len(), 1);
        assert!(
            fixture
                .a
                .received_requests()
                .await
                .expect("recording enabled")
                .is_empty()
        );
        assert!(
            fixture
                .b
                .received_requests()
                .await
                .expect("recording enabled")
                .is_empty()
        );
        assert!(!fixture.home.path().join("helper-marker").exists());
        assert!(
            fixture
                .root
                .received_requests()
                .await
                .expect("recording enabled")
                .iter()
                .filter(|request| request.method.as_str() == "POST")
                .any(|request| body(request)
                    .to_string()
                    .contains("child_provider_model_unavailable"))
        );
    }
    Ok(())
}
