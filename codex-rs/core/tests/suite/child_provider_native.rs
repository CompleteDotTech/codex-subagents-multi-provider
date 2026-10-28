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
        zstd::stream::decode_all(request.body.as_slice())
            .expect("native mock request zstd body must decode")
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&bytes).expect("native mock request body must be valid JSON")
}

fn advertised_provider_ids(value: &Value) -> Vec<String> {
    let mut ids = vec![];
    match value {
        Value::Object(object) => {
            if let Some(values) = object
                .get("model_provider")
                .and_then(|schema| schema.get("enum"))
                .and_then(Value::as_array)
            {
                ids.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            for value in object.values() {
                ids.extend(advertised_provider_ids(value));
            }
        }
        Value::Array(values) => {
            for value in values {
                ids.extend(advertised_provider_ids(value));
            }
        }
        _ => {}
    }
    ids.sort();
    ids.dedup();
    ids
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
            .expect("bundled catalog must contain the native fixture model");
        info.slug = "shared".into();
        info.multi_agent_version = Some(version);
        // Exercise native direct tools; deferred tool_search advertisement is
        // covered by the separate Responses capability suites.
        info.supports_search_tool = false;
        info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
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
        // Build TOML integers directly: serde_json's arbitrary-precision Number
        // serialization uses an internal map that is not a TOML integer.
        let helper_arg = helper.to_string_lossy().into_owned();
        let args = if cfg!(windows) {
            vec!["/D".to_owned(), "/C".to_owned(), helper_arg]
        } else {
            vec![helper_arg]
        };
        let helper_auth = toml::Value::Table(toml::Table::from_iter([
            (
                "command".into(),
                toml::Value::String(if cfg!(windows) { "cmd.exe" } else { "sh" }.into()),
            ),
            (
                "args".into(),
                toml::Value::Array(args.into_iter().map(toml::Value::String).collect()),
            ),
            (
                "cwd".into(),
                toml::Value::String(home.path().to_string_lossy().into_owned()),
            ),
            ("timeout_ms".into(), toml::Value::Integer(5000)),
            ("refresh_interval_ms".into(), toml::Value::Integer(0)),
        ]));
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
        let blocked = config
            .get_mut("model_providers")
            .and_then(toml::Value::as_table_mut)
            .expect("fixture declares the model_providers table")
            .get_mut("blocked")
            .and_then(toml::Value::as_table_mut)
            .expect("fixture declares the blocked provider table");
        assert!(
            blocked.insert("auth".into(), helper_auth).is_none(),
            "the helper is configured exactly once"
        );
        std::fs::write(home.path().join("config.toml"), toml::to_string(&config)?)?;
        let mut builder = test_codex()
            .with_home(home.clone())
            .with_auth(CodexAuth::from_api_key("synthetic-root"))
            .with_model_info_override("gpt-5.5", move |info| {
                info.multi_agent_version = Some(version);
                info.supports_search_tool = false;
                info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
            })
            .with_config(move |config| {
                config
                    .features
                    .enable(Feature::Collab)
                    .expect("enable native fixture collaboration");
                if version == MultiAgentVersion::V2 {
                    config
                        .features
                        .enable(Feature::MultiAgentV2)
                        .expect("enable native fixture V2");
                } else {
                    config
                        .features
                        .disable(Feature::MultiAgentV2)
                        .expect("select native fixture V1");
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
                if ids.len() > expected {
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
            let child = self
                .test
                .thread_manager
                .get_thread(id)
                .await
                .expect("actual spawned native child must be registered");
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
            for request in server
                .received_requests()
                .await
                .expect("native provider request recording must be enabled")
            {
                if request.method.as_str() != "POST" {
                    continue;
                }
                assert_eq!(
                    request
                        .headers
                        .get("authorization")
                        .expect("native provider request must carry its configured authorization"),
                    token
                );
                assert!(!request.headers.contains_key("chatgpt-account-id"));
                assert!(!request.headers.contains_key("x-openai-actor-id"));
                assert_eq!(body(&request)["model"], "shared");
            }
        }
        for request in self
            .root
            .received_requests()
            .await
            .expect("native root request recording must be enabled")
        {
            if request.method.as_str() == "POST" {
                assert_eq!(
                    request
                        .headers
                        .get("authorization")
                        .expect("native provider request must carry its configured authorization"),
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
            let fixture = Fixture::new(version, /*default_provider*/ None).await?;
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
            let children = fixture.children(/*expected*/ 1).await?;
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
            let fixture = Fixture::new(version, /*default_provider*/ None).await?;
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
            let children = fixture.children(/*expected*/ 2).await?;
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
        let children = fixture.children(/*expected*/ 2).await?;
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
        let children = fixture.children(/*expected*/ 1).await?;
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
            let fixture = Fixture::new(version, /*default_provider*/ None).await?;
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
                .expect("native fixture allowlist must be an array")
                .push(toml::Value::String("blocked".into()));
        }
    }
    config.config_layer_stack = codex_config::ConfigLayerStack::new(
        layers,
        stack.requirements().clone(),
        stack.requirements_toml().clone(),
    )
    .expect("native fixture configuration layers must be valid");
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
    .expect("native fixture configuration layers must be valid");
}

fn managed_allowlist_setup(config: &mut codex_core::config::Config) {
    let stack = &config.config_layer_stack;
    let mut layers = stack.layers_low_to_high().cloned().collect::<Vec<_>>();
    layers.push(codex_config::ConfigLayerEntry::new(
        codex_config::ConfigLayerSource::Mdm {
            domain: "native-test".into(),
            key: "providers".into(),
        },
        toml::from_str("[agents]\nallowed_model_providers=['b']")
            .expect("native managed allowlist TOML must be valid"),
    ));
    layers.sort_by(|left, right| {
        left.name
            .partial_cmp(&right.name)
            .expect("native fixture layer precedence must be comparable")
    });
    config.config_layer_stack = codex_config::ConfigLayerStack::new(
        layers,
        stack.requirements().clone(),
        stack.requirements_toml().clone(),
    )
    .expect("native fixture configuration layers must be valid");
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
            let fixture =
                Fixture::with_host_setup(version, /*default_provider*/ None, setup).await?;
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
            let fixture = Fixture::with_host_setup(
                version,
                /*default_provider*/ None,
                allow_helper_provider_setup,
            )
            .await?;
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
                    .any(|request| advertised_provider_ids(&body(request)["tools"])
                        .contains(&"blocked".into()))
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
                    .expect("recording enabled")
                    .iter()
                    .filter(|request| request.method.as_str() == "POST")
                    .all(|request| !advertised_provider_ids(&body(request)["tools"])
                        .contains(&"blocked".into()))
            );
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
    let fixture = Fixture::with_host_setup(
        MultiAgentVersion::V2,
        /*default_provider*/ None,
        |config| {
            config.multi_agent_v2.max_concurrent_threads_per_session = 2;
        },
    )
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
    assert_eq!(fixture.children(/*expected*/ 1).await?.len(), 1);
    fixture.assert_provider_headers().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_provider_does_not_replace_an_incompatible_role_model_or_fall_back() -> Result<()>
{
    for version in [MultiAgentVersion::V1, MultiAgentVersion::V2] {
        let fixture = Fixture::with_host_setup(version, /*default_provider*/ None, |config| {
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

async fn native_terminal_status(
    child: &codex_core::CodexThread,
) -> codex_protocol::protocol::AgentStatus {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let status = child.agent_status().await;
            if matches!(
                status,
                codex_protocol::protocol::AgentStatus::Completed(_)
                    | codex_protocol::protocol::AgentStatus::Errored(_)
            ) {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual child reaches terminal turn status")
}

fn revoke_native_fixture_a(home: &std::path::Path) -> Result<()> {
    let path = home.join("config.toml");
    let mut value: toml::Value = toml::from_str(&std::fs::read_to_string(&path)?)?;
    let definition = value["model_providers"]["a"].clone();
    let defaults = value["agents"]["provider_defaults"]["a"].clone();
    value["agents"]["allowed_model_providers"] = toml::Value::Array(vec![
        toml::Value::String("openai".into()),
        toml::Value::String("b".into()),
    ]);
    std::fs::write(path, toml::to_string(&value)?)?;
    assert_eq!(value["model_providers"]["a"], definition);
    assert_eq!(value["agents"]["provider_defaults"]["a"], defaults);
    Ok(())
}

fn native_fixture_policy_denial(error: &str) -> bool {
    [
        "child_provider_not_authorized",
        "child_provider_untrusted_source",
        "child_provider_policy_conflict",
    ]
    .iter()
    .any(|code| error.contains(code))
}

fn assert_native_fixture_published_revocation(config: &codex_core::config::Config) {
    let effective = config.config_layer_stack.effective_config();
    let allowed = effective["agents"]["allowed_model_providers"]
        .as_array()
        .expect("published owned allowlist");
    assert!(!allowed.iter().any(|id| id.as_str() == Some("a")));
    assert!(allowed.iter().any(|id| id.as_str() == Some("openai")));
    assert!(allowed.iter().any(|id| id.as_str() == Some("b")));
    assert!(
        effective["model_providers"]["a"].as_table().is_some(),
        "revocation does not delete the provider definition"
    );
    assert!(
        effective["agents"]["provider_defaults"]["a"]["model_catalog_json"]
            .as_str()
            .is_some(),
        "catalog selection remains present"
    );
}

async fn publish_native_fixture_host_config(
    fixture: &Fixture,
    target: Option<&codex_core::CodexThread>,
) -> Result<()> {
    // A file write is not a publication. Load the real owned user source and use
    // the same public compare-and-publish operation as an ordinary host refresh.
    let next = codex_core::config::ConfigBuilder::default()
        .codex_home(fixture.home.path().to_path_buf())
        .loader_overrides(codex_config::LoaderOverrides::without_managed_config_for_tests())
        .harness_overrides(codex_core::config::ConfigOverrides {
            cwd: Some(fixture.test.cwd.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await?;
    let current = fixture.test.codex.config().await;
    let root_provider = current.model_provider.clone();
    let root_model = current.model.clone();
    assert_eq!(
        fixture
            .test
            .codex
            .refresh_runtime_config(current, next.clone())
            .await,
        codex_core::ConfigRefreshOutcome::Published
    );
    let published = fixture.test.codex.config().await;
    assert_native_fixture_published_revocation(&published);
    assert_eq!(published.model_provider_id, "openai");
    assert!(
        published.model_provider == root_provider && published.model == root_model,
        "actual root model and transport stay unchanged"
    );
    if let Some(target) = target {
        let current = target.config().await;
        let provider = current.model_provider.clone();
        let model = current.model.clone();
        assert_eq!(
            target.refresh_runtime_config(current, next).await,
            codex_core::ConfigRefreshOutcome::Published
        );
        let published = target.config().await;
        assert_native_fixture_published_revocation(&published);
        assert_eq!(published.model_provider_id, "a");
        assert!(
            published.model_provider == provider && published.model == model,
            "publishing host inputs does not reroute the existing selected child"
        );
    }
    tracing::info!(
        target_published = target.is_some(),
        "NATIVE_HOST_POLICY_PUBLICATION_OBSERVED"
    );
    Ok(())
}

async fn widen_native_fixture_executor(fixture: &Fixture) -> Result<()> {
    let cwd = fixture.test.codex.thread_settings_snapshot().await.cwd;
    let host_file = fixture.home.path().join("config.toml");
    let catalog = fixture.home.path().join("catalog.json");
    let current = fixture.test.codex.environment_selections().await;
    let current_config = fixture.test.codex.config().await;
    assert!(
        !current.is_empty(),
        "actual invoking environment is required"
    );
    let requests = current
        .into_iter()
        .map(|mut selection| {
            // Configured local selections normally inherit FromThread; they
            // are not the active turn's resolved Ready environment snapshot.
            if matches!(
                selection.config,
                codex_protocol::protocol::EnvironmentConfigState::FromThread
            ) {
                selection.config = codex_protocol::protocol::EnvironmentConfigState::Ready(
                    core_test_support::test_codex::environment_config_for_selection(
                        &current_config,
                        &selection,
                    ),
                );
            }
            let codex_protocol::protocol::EnvironmentConfigState::Ready(owner) =
                &mut selection.config
            else {
                panic!("actual invoking environment must be ready")
            };
            owner.permission_profile = codex_protocol::models::PermissionProfileSnapshot::legacy(
                codex_protocol::models::PermissionProfile::Disabled,
            );
            selection.into_request()
        })
        .collect();
    fixture
        .test
        .codex
        .update_thread_settings(codex_protocol::protocol::ThreadSettingsOverrides {
            permission_profile: Some(codex_protocol::models::PermissionProfile::Disabled),
            environments: Some(codex_protocol::protocol::TurnEnvironmentRequests::new(
                cwd.clone(),
                requests,
            )),
            ..Default::default()
        })
        .await?;
    fixture
        .turn("materialize accepted invoking executor settings")
        .await?;
    let published = fixture.test.codex.environment_selections().await;
    assert!(!published.is_empty());
    for selection in published {
        let codex_protocol::protocol::EnvironmentConfigState::Ready(owner) = selection.config
        else {
            panic!("accepted executor configuration must be ready")
        };
        let filesystem = owner
            .permission_profile
            .permission_profile()
            .file_system_sandbox_policy();
        assert!(filesystem.can_write_local_path_with_cwd(&host_file, cwd.as_path()));
        assert!(filesystem.can_write_local_path_with_cwd(&catalog, cwd.as_path()));
    }
    Ok(())
}

async fn native_post_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("actual loopback recording enabled")
        .iter()
        .filter(|request| {
            request.method.as_str() == "POST" && request.url.path() == "/v1/responses"
        })
        .count()
}

fn native_advertises_exec_command(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            (object.get("type").and_then(Value::as_str) == Some("function")
                && object.get("name").and_then(Value::as_str) == Some("exec_command"))
                || object.values().any(native_advertises_exec_command)
        }
        Value::Array(values) => values.iter().any(native_advertises_exec_command),
        _ => false,
    }
}

async fn script_native_followup(server: &MockServer, target: codex_protocol::ThreadId) {
    Mock::given(method("POST")).and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
            .set_body_string(sse(vec![
                ev_response_created("loaded-followup"),
                ev_function_call_with_namespace("loaded-followup-call", "collaboration", "followup_task",
                    &json!({"target":target.to_string(),"message":"continue the existing loaded allocation"}).to_string()),
                ev_completed("loaded-followup"),
            ])))
        .with_priority(1).up_to_n_times(1).mount(server).await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(completion("root-after-loaded-followup"))
        .with_priority(2)
        .mount(server)
        .await;
}

#[test_case::test_case(0; "loaded selected provider remains authorized")]
#[test_case::test_case(1; "loaded selected provider host authorization revoked")]
#[test_case::test_case(2; "loaded selected provider caller executor scope widened")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_loaded_followup_rechecks_policy_before_target_network(change: u8) -> Result<()> {
    let fixture = Fixture::new(MultiAgentVersion::V2, None).await?;
    scripted_spawn(
        &fixture.root,
        MultiAgentVersion::V2,
        "loaded_network_worker",
        json!({"message":"initial selected-provider turn","model_provider":"a"}),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(completion("loaded-a-complete"))
        .mount(&fixture.a)
        .await;
    fixture
        .turn("create an ordinary selected-provider child")
        .await?;
    let children = fixture.children(1).await?;
    assert_eq!(children.len(), 1);
    let child = Arc::clone(&children[0]);
    let id = child
        .read_thread(
            /*include_archived*/ true, /*include_history*/ false,
        )
        .await?
        .thread_id;
    assert!(matches!(
        native_terminal_status(&child).await,
        codex_protocol::protocol::AgentStatus::Completed(_)
    ));
    assert_eq!(native_post_count(&fixture.a).await, 1);
    assert_eq!(child.config_snapshot().await.model_provider_id, "a");

    if change == 1 {
        revoke_native_fixture_a(fixture.home.path())?;
        publish_native_fixture_host_config(&fixture, None).await?;
    } else if change == 2 {
        widen_native_fixture_executor(&fixture).await?;
    }
    fixture.root.reset().await;
    script_native_followup(&fixture.root, id).await;
    fixture
        .turn("follow up without evicting or replacing the existing child")
        .await?;
    if change == 0 {
        tokio::time::timeout(Duration::from_secs(30), async {
            while native_post_count(&fixture.a).await < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("current loaded child makes another real inference");
        assert!(matches!(
            native_terminal_status(&child).await,
            codex_protocol::protocol::AgentStatus::Completed(_)
        ));
    }
    assert!(Arc::ptr_eq(
        &child,
        &fixture.test.thread_manager.get_thread(id).await?
    ));
    assert_eq!(
        native_post_count(&fixture.a).await,
        if change == 0 { 2 } else { 1 },
        "revocation must stop new target HTTP, not merely hide the result"
    );
    assert_eq!(
        native_post_count(&fixture.b).await,
        0,
        "no provider fallback"
    );
    if change != 0 {
        assert!(
            fixture
                .root
                .received_requests()
                .await
                .expect("recording enabled")
                .iter()
                .filter(|request| request.method.as_str() == "POST")
                .any(
                    |request| body(request)["input"].as_array().is_some_and(|items| items
                        .iter()
                        .any(|item| item["type"].as_str() == Some("function_call_output")
                            && item["call_id"].as_str() == Some("loaded-followup-call")
                            && native_fixture_policy_denial(&item["output"].to_string())))
                ),
            "actual followup tool must report denial before accepting work"
        );
    }
    assert!(!fixture.home.path().join("helper-marker").exists());
    let root_before = native_post_count(&fixture.root).await;
    fixture
        .turn("ordinary OpenAI parent remains usable after child decision")
        .await?;
    assert!(native_post_count(&fixture.root).await > root_before);
    assert_eq!(
        fixture.test.codex.config_snapshot().await.model_provider_id,
        "openai"
    );
    assert!(Arc::ptr_eq(
        &child,
        &fixture.test.thread_manager.get_thread(id).await?
    ));
    Ok(())
}

fn native_continuation_exec_setup(config: &mut codex_core::config::Config) {
    // TestCodex changes cwd after loading Config; bind both root projections to
    // that owned workspace rather than retaining the loader's earlier cwd.
    config.workspace_roots = vec![config.cwd.clone()];
    config
        .permissions
        .set_workspace_roots(config.workspace_roots.clone());
    config
        .permissions
        .approval_policy
        .set(codex_protocol::protocol::AskForApproval::Never)
        .unwrap();
    config
        .permissions
        .set_permission_profile(
            codex_protocol::models::PermissionProfile::workspace_write_with(
                &[],
                codex_protocol::permissions::NetworkSandboxPolicy::Restricted,
                /*exclude_tmpdir_env_var*/ true,
                /*exclude_slash_tmp*/ true,
            ),
        )
        .unwrap();
    config.features.enable(Feature::ShellTool).unwrap();
    for feature in [
        Feature::CodeMode,
        Feature::CodeModeOnly,
        Feature::ToolSearch,
        Feature::CodeModeToolSearch,
    ] {
        config.features.disable(feature).unwrap();
    }
}

fn assert_native_continuation_workspace(
    fixture: &Fixture,
    config: &codex_core::config::Config,
    selections: &[codex_protocol::protocol::TurnEnvironmentSelection],
) {
    assert_eq!(config.cwd.as_path(), fixture.test.cwd.path());
    assert_eq!(config.workspace_roots, vec![config.cwd.clone()]);
    let expected = fixture.test.executor_environment().selection();
    assert_eq!(selections.len(), 1, "one actual owned executor is required");
    let selection = &selections[0];
    assert_eq!(selection.environment_id, expected.environment_id);
    assert_eq!(selection.cwd, expected.cwd);
    assert_eq!(selection.workspace_roots, vec![expected.cwd.clone()]);
    let owner = match &selection.config {
        codex_protocol::protocol::EnvironmentConfigState::Ready(owner) => owner.clone(),
        // Active selection readback preserves ownership as FromThread. This is an
        // effective filesystem permission projection from the actual child defaults
        // and selected roots, not readback of its full internal resolved config.
        codex_protocol::protocol::EnvironmentConfigState::FromThread => {
            core_test_support::test_codex::environment_config_for_selection(config, selection)
        }
        _ => panic!("selected executor configuration must be available"),
    };
    assert_eq!(owner.workspace_roots, selection.workspace_roots);
    let filesystem = owner
        .permission_profile
        .permission_profile()
        .clone()
        .materialize_project_roots_with_path_uris(&owner.workspace_roots)
        .file_system_sandbox_policy();
    for name in [
        "native-continuation-started",
        "native-continuation-release",
        "native-continuation-done",
    ] {
        assert!(
            filesystem.can_write_local_path_with_cwd(
                &fixture.test.cwd.path().join(name),
                config.cwd.as_path()
            ),
            "actual selected executor roots must permit the owned continuation markers"
        );
    }
    for path in [
        fixture.home.path().join("config.toml"),
        fixture.home.path().join("catalog.json"),
    ] {
        assert!(
            !filesystem.can_write_local_path_with_cwd(&path, config.cwd.as_path()),
            "the selected executor must not write host provider inputs"
        );
    }
}

fn native_owned_barrier_command(
    started: &std::path::Path,
    release: &std::path::Path,
    done: &std::path::Path,
) -> (String, &'static str) {
    if core_test_support::test_target_os() == core_test_support::TestTargetOs::Windows {
        let quote =
            |path: &std::path::Path| format!("'{}'", path.to_string_lossy().replace('\'', "''"));
        (
            format!(
                "Set-Content -LiteralPath {} -Value started; $limit=[DateTime]::UtcNow.AddSeconds(5); while(-not(Test-Path -LiteralPath {})) {{ if([DateTime]::UtcNow -ge $limit) {{ throw 'owned continuation barrier expired' }}; Start-Sleep -Milliseconds 10 }}; Set-Content -LiteralPath {} -Value done; Write-Output 'owned continuation tool completed'",
                quote(started),
                quote(release),
                quote(done)
            ),
            "powershell.exe",
        )
    } else {
        let quote =
            |path: &std::path::Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
        (
            format!(
                "printf started > {}; i=0; while ! test -f {}; do i=$((i+1)); if test \"$i\" -ge 500; then exit 77; fi; sleep 0.01; done; printf done > {}; printf 'owned continuation tool completed\\n'",
                quote(started),
                quote(release),
                quote(done)
            ),
            "/bin/sh",
        )
    }
}

#[test_case::test_case(0; "existing native turn current before next inference")]
#[test_case::test_case(1; "existing native turn host publication revokes next inference")]
#[test_case::test_case(2; "existing native turn host and target publication revoke next inference")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_existing_turn_rechecks_policy_after_successful_owned_tool(
    change: u8,
) -> Result<()> {
    let fixture =
        Fixture::with_host_setup(MultiAgentVersion::V2, None, native_continuation_exec_setup)
            .await?;
    let started = fixture.test.cwd.path().join("native-continuation-started");
    let release = fixture.test.cwd.path().join("native-continuation-release");
    let done = fixture.test.cwd.path().join("native-continuation-done");
    for path in [&started, &release, &done] {
        assert!(!path.exists());
    }
    let config = fixture.test.codex.config().await;
    assert_native_continuation_workspace(
        &fixture,
        &config,
        &fixture.test.codex.environment_selections().await,
    );
    let (cmd, shell) = native_owned_barrier_command(&started, &release, &done);
    let arguments = json!({"cmd":cmd,"shell":shell,"login":false,"tty":false,"yield_time_ms":10000,"timeout_ms":5000,"max_output_tokens":1000});
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(vec![
                    ev_response_created("native-owned-tool"),
                    core_test_support::responses::ev_exec_command_call_with_args(
                        "native-owned-tool-call",
                        &arguments,
                    ),
                    ev_completed("native-owned-tool"),
                ])),
        )
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&fixture.a)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(completion("native-after-owned-tool"))
        .with_priority(2)
        .mount(&fixture.a)
        .await;
    scripted_spawn(&fixture.root, MultiAgentVersion::V2, "continuation_worker",
        json!({"message":"execute the supplied bounded owned tool then continue","model_provider":"a"})).await;
    // The real host changes its own source only after the actual tool is running.
    // No child tool receives authority to modify the host provider definition.
    let change_policy = async {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !started.exists() {
                let root_requests = fixture.root.received_requests().await.expect("root recording enabled");
                for request in root_requests.iter().filter(|request| request.method.as_str() == "POST") {
                    if let Some(output) = body(request)["input"].as_array().and_then(|items| items.iter().find(|item| {
                        item["type"].as_str() == Some("function_call_output")
                            && item["call_id"].as_str() == Some("continuation_worker")
                    })) {
                        let ids = fixture.test.thread_manager.list_thread_ids().await;
                        if ids.iter().all(|id| *id == fixture.test.session_configured.thread_id) {
                            anyhow::bail!("actual root spawn returned without a registered child; setup failure, not policy RED: {output}");
                        }
                    }
                }
                for id in fixture.test.thread_manager.list_thread_ids().await {
                    if id == fixture.test.session_configured.thread_id {
                        continue;
                    }
                    if let Ok(child) = fixture.test.thread_manager.get_thread(id).await {
                        let status = child.agent_status().await;
                        if matches!(
                            status,
                            codex_protocol::protocol::AgentStatus::Completed(_)
                                | codex_protocol::protocol::AgentStatus::Errored(_)
                        ) && !started.exists() {
                            anyhow::bail!("registered child became terminal before the owned tool barrier; setup failure, not policy RED: {status:?}");
                        }
                    }
                }
                let requests = fixture.a.received_requests().await.expect("recording enabled");
                if let Some(first) = requests.iter().find(|request| request.method.as_str() == "POST") {
                    let tools = body(first)["tools"].clone();
                    assert!(
                        native_advertises_exec_command(&tools),
                        "actual selected-provider request must advertise exec_command before the owned tool barrier: {tools}"
                    );
                }
                for request in requests.iter().filter(|request| request.method.as_str() == "POST") {
                    if let Some(output) = body(request)["input"].as_array().and_then(|items| items.iter().find(|item| {
                        item["type"].as_str() == Some("function_call_output")
                            && item["call_id"].as_str() == Some("native-owned-tool-call")
                    })) {
                        anyhow::bail!("owned exec_command returned before creating the barrier; setup failure, not policy RED: {output}");
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("actual exec_command reaches owned barrier")?;
        let children = fixture.children(1).await?;
        assert_eq!(children.len(), 1);
        let child = &children[0];
        let child_config = child.config().await;
        assert_native_continuation_workspace(
            &fixture,
            &child_config,
            &child.environment_selections().await,
        );
        assert_native_continuation_workspace(
            &fixture,
            &child_config,
            &child
                .active_turn_environment_selections()
                .await
                .expect("actual owned tool keeps the child turn active"),
        );
        if change != 0 {
            revoke_native_fixture_a(fixture.home.path())?;
            let children = fixture.children(1).await?;
            assert_eq!(children.len(), 1);
            publish_native_fixture_host_config(
                &fixture,
                if change == 2 {
                    Some(children[0].as_ref())
                } else {
                    None
                },
            )
            .await?;
        }
        std::fs::write(&release, "release")?;
        Ok::<_, anyhow::Error>(())
    };
    let (root_result, policy_result) = tokio::join!(
        fixture.turn("delegate actual running child tool work"),
        change_policy
    );
    root_result?;
    policy_result?;
    let children = fixture.children(1).await?;
    assert_eq!(children.len(), 1);
    let child = Arc::clone(&children[0]);
    let status = native_terminal_status(&child).await;
    assert_eq!(
        std::fs::read_to_string(&done)?.trim(),
        "done",
        "actual owned tool must succeed, never convert a missing executable or barrier timeout into policy RED"
    );
    child.ensure_rollout_materialized().await;
    child.flush_rollout().await?;
    let stored = child
        .read_thread(
            /*include_archived*/ true, /*include_history*/ false,
        )
        .await?;
    let id = stored.thread_id;
    let history = fixture
        .test
        .thread_store
        .load_latest_model_context(codex_thread_store::LoadThreadHistoryParams {
            thread_id: id,
            include_archived: true,
        })
        .await?;
    assert_eq!(history.thread_id, id);
    assert!(
        matches!(history.items.first(), Some(codex_history::RolloutItem::SessionMeta(meta))
            if meta.meta.id == id
                && meta.meta.parent_thread_id == Some(fixture.test.session_configured.thread_id)),
        "actual durable model context must identify this child and its owned parent"
    );
    let successful_tool = history.items.iter().any(|item| {
        let codex_history::RolloutItem::ResponseItem(envelope) = item else { return false; };
        matches!(&envelope.item, codex_protocol::models::ResponseItem::FunctionCallOutput {
            call_id: Some(call_id), output, ..
        } if call_id == "native-owned-tool-call" && output.text_content().is_some_and(|text| text.contains("Process exited with code 0") && text.contains("owned continuation tool completed")))
    });
    assert!(
        successful_tool,
        "actual successful function result must precede the guarded next inference"
    );
    assert_eq!(
        native_post_count(&fixture.a).await,
        if change != 0 { 1 } else { 2 },
        "no next target network request after revocation inside an existing turn"
    );
    if change != 0 {
        assert!(
            matches!(status, codex_protocol::protocol::AgentStatus::Errored(ref error) if native_fixture_policy_denial(error)),
            "{status:?}"
        );
    } else {
        assert!(
            matches!(status, codex_protocol::protocol::AgentStatus::Completed(_)),
            "{status:?}"
        );
    }
    assert_eq!(native_post_count(&fixture.b).await, 0);
    assert!(!fixture.home.path().join("helper-marker").exists());
    let root_before = native_post_count(&fixture.root).await;
    fixture
        .turn("OpenAI parent subsequent turn after actual child tool and decision")
        .await?;
    assert!(native_post_count(&fixture.root).await > root_before);
    assert_eq!(
        fixture.test.codex.config_snapshot().await.model_provider_id,
        "openai"
    );
    assert!(Arc::ptr_eq(
        &child,
        &fixture.test.thread_manager.get_thread(id).await?
    ));
    Ok(())
}

// A real host command hook pauses only the selected custom-provider request.
// Unix is explicit because this fixture uses the existing Python command-hook
// runtime; no Windows execution or setup-failure policy credit is claimed.
#[cfg(unix)]
fn native_request_barrier_setup(config: &mut codex_core::config::Config, attempt: u8) {
    let home = config.codex_home.as_path();
    let script = home.join("native-request-barrier.py");
    let source = format!(
        r#"import json, pathlib, sys, time
home = pathlib.Path({home})
payload = json.load(sys.stdin)
if payload.get("model") == "shared":
    counter = home / "native-request-attempts"
    attempt = int(counter.read_text()) + 1 if counter.exists() else 1
    counter.write_text(str(attempt))
    if attempt == {attempt}:
        (home / "native-request-hook-entered").write_text(json.dumps(payload))
        deadline = time.monotonic() + 20
        while not (home / "native-request-hook-release").exists():
            if time.monotonic() >= deadline:
                raise RuntimeError("native request barrier timed out: setup failure, not policy evidence")
            time.sleep(0.01)
print("{{}}")
"#,
        home = serde_json::to_string(&home.to_string_lossy()).unwrap(),
    );
    std::fs::write(&script, source).unwrap();
    std::fs::write(
        home.join("hooks.json"),
        json!({"hooks":{"PreModelRequest":[{"hooks":[{
            "type":"command", "command":format!("python3 -I -S '{}'", script.display())
        }]}]}})
        .to_string(),
    )
    .unwrap();
    core_test_support::hooks::trust_discovered_hooks(config);
}

#[cfg(unix)]
fn native_first_request_barrier_setup(config: &mut codex_core::config::Config) {
    native_request_barrier_setup(config, 1);
}

#[cfg(unix)]
fn native_retry_request_barrier_setup(config: &mut codex_core::config::Config) {
    native_request_barrier_setup(config, 2);
}

#[cfg(unix)]
#[test_case::test_case(false, false; "actual pre-model hook unchanged positive")]
#[test_case::test_case(false, true; "actual pre-model hook host publication denies dispatch")]
#[test_case::test_case(true, false; "actual HTTP stream retry unchanged positive")]
#[test_case::test_case(true, true; "actual HTTP stream retry hook publication denies next dispatch")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_request_hook_and_http_retry_recheck_after_publication(
    retry: bool,
    revoke: bool,
) -> Result<()> {
    let setup: fn(&mut codex_core::config::Config) = if retry {
        native_retry_request_barrier_setup
    } else {
        native_first_request_barrier_setup
    };
    let fixture = Fixture::with_host_setup(MultiAgentVersion::V2, None, setup).await?;
    if retry {
        // A real accepted HTTP/SSE attempt lacks response.completed. Core must
        // observe this as a stream error and enter its actual normal retry loop.
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(vec![ev_response_created(
                        "incomplete-native-http-attempt",
                    )])),
            )
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&fixture.a)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(completion("custom-after-hook"))
        .mount(&fixture.a)
        .await;
    scripted_spawn(
        &fixture.root,
        MultiAgentVersion::V2,
        "hook_guard_worker",
        json!({"prompt":"Complete the bounded hook request","model_provider":"a"}),
    )
    .await;
    fixture.turn("spawn the hook barrier child").await?;
    let entered = fixture.home.path().join("native-request-hook-entered");
    let release = fixture.home.path().join("native-request-hook-release");
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if entered.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual PreModelRequest command must enter the owned barrier");
    let children = fixture.children(1).await?;
    let child = &children[0];
    let stored = child.read_thread(true, false).await?;
    let child_id = stored.thread_id;
    let hook_input: Value = serde_json::from_slice(&std::fs::read(&entered)?)?;
    assert_eq!(hook_input["hook_event_name"], "PreModelRequest");
    assert_eq!(hook_input["model"], "shared");
    assert_eq!(
        hook_input["agent_id"].as_str(),
        Some(child_id.to_string().as_str()),
        "the actual hook must belong to this registered custom-provider child"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.home.path().join("native-request-attempts"))?,
        if retry { "2" } else { "1" }
    );
    let before = usize::from(retry);
    assert_eq!(
        native_post_count(&fixture.a).await,
        before,
        "the barrier is before the selected-provider dispatch, after any required real HTTP retry attempt"
    );
    assert_eq!(native_post_count(&fixture.b).await, 0);
    assert!(!fixture.home.path().join("helper-marker").exists());
    if revoke {
        revoke_native_fixture_a(fixture.home.path())?;
        publish_native_fixture_host_config(&fixture, None).await?;
        assert_native_fixture_published_revocation(&fixture.test.codex.config().await);
    }
    std::fs::write(&release, b"release actual request hook")?;
    let status = native_terminal_status(child).await;
    if revoke {
        assert!(
            matches!(&status, codex_protocol::protocol::AgentStatus::Errored(message)
            if native_fixture_policy_denial(message)),
            "{status:?}"
        );
    } else {
        assert!(
            matches!(status, codex_protocol::protocol::AgentStatus::Completed(_)),
            "{status:?}"
        );
    }
    assert_eq!(
        native_post_count(&fixture.a).await,
        before + usize::from(!revoke)
    );
    assert_eq!(native_post_count(&fixture.b).await, 0);
    for request in fixture
        .a
        .received_requests()
        .await
        .expect("A request recording enabled")
        .iter()
        .filter(|request| request.method.as_str() == "POST")
    {
        assert_eq!(
            request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer synthetic-a")
        );
        assert!(!String::from_utf8_lossy(&request.body).contains("synthetic-root"));
    }
    assert!(!fixture.home.path().join("helper-marker").exists());
    assert_eq!(child.config_snapshot().await.model_provider_id, "a");
    assert!(Arc::ptr_eq(
        child,
        &fixture.test.thread_manager.get_thread(child_id).await?
    ));
    let root_before = native_post_count(&fixture.root).await;
    fixture
        .turn("ordinary OpenAI parent remains usable after the request boundary")
        .await?;
    assert!(native_post_count(&fixture.root).await > root_before);
    assert_eq!(
        fixture.test.codex.config_snapshot().await.model_provider_id,
        "openai"
    );
    // A different authorized provider and its fresh child remain independent.
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(completion("independent-b-child"))
        .mount(&fixture.b)
        .await;
    scripted_spawn(
        &fixture.root,
        MultiAgentVersion::V2,
        "hook_guard_sibling",
        json!({"prompt":"Complete the independent sibling","model_provider":"b"}),
    )
    .await;
    fixture.turn("spawn the unaffected B sibling").await?;
    let children = fixture.children(2).await?;
    let mut found_b = false;
    for sibling in children {
        if sibling.config_snapshot().await.model_provider_id == "b" {
            assert!(matches!(
                native_terminal_status(&sibling).await,
                codex_protocol::protocol::AgentStatus::Completed(_)
            ));
            found_b = true;
        }
    }
    assert!(found_b);
    assert_eq!(native_post_count(&fixture.b).await, 1);
    assert_eq!(
        native_post_count(&fixture.a).await,
        before + usize::from(!revoke)
    );
    assert!(!fixture.home.path().join("helper-marker").exists());
    Ok(())
}
