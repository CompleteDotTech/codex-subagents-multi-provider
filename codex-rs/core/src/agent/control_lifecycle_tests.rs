struct PendingPostAdmissionCreate {
    child: ThreadId,
    parent: ThreadId,
    release: tokio::sync::oneshot::Sender<()>,
}

struct PostAdmissionCreateGate {
    inner: InMemoryThreadStore,
    armed_parent: std::sync::Mutex<Option<ThreadId>>,
    armed_resume: std::sync::Mutex<Option<(ThreadId, ThreadId)>>,
    entered: tokio::sync::mpsc::UnboundedSender<PendingPostAdmissionCreate>,
}

impl ThreadStore for PostAdmissionCreateGate {
    fn begin_native_authority_epoch_owned(
        &self,
        epoch: codex_thread_store::NativeAuthorityEpoch,
        owner: codex_thread_store::ThreadWriterOwner,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        self.inner.begin_native_authority_epoch_owned(epoch, owner)
    }

    fn create_thread_owned(
        &self,
        params: codex_thread_store::CreateThreadParams,
    ) -> codex_thread_store::ThreadStoreFuture<'_, codex_thread_store::ThreadWriterOwner> {
        Box::pin(async move {
            let armed = *self.armed_parent.lock().unwrap();
            if params.parent_thread_id.is_some() && params.parent_thread_id == armed {
                let parent = armed.expect("armed actual parent");
                assert_eq!(params.source.parent_thread_id(), Some(parent));
                assert!(matches!(
                    &params.source,
                    SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
                ));
                *self.armed_parent.lock().unwrap() = None;
                let (release, released) = tokio::sync::oneshot::channel();
                self.entered
                    .send(PendingPostAdmissionCreate {
                        child: params.thread_id,
                        parent,
                        release,
                    })
                    .expect("actual store gate receiver");
                released
                    .await
                    .expect("release actual post-admission create");
            }
            self.inner.create_thread_owned(params).await
        })
    }
    fn resume_thread_owned(
        &self,
        params: codex_thread_store::ResumeThreadParams,
        preparation: Option<codex_thread_store::ThreadStoreResumePreparation>,
    ) -> codex_thread_store::ThreadStoreFuture<
        '_,
        (Arc<Vec<RolloutItem>>, codex_thread_store::ThreadWriterOwner),
    > {
        Box::pin(async move {
            let armed = *self.armed_resume.lock().unwrap();
            if let Some((target, parent)) = armed.filter(|(target, _)| *target == params.thread_id)
            {
                *self.armed_resume.lock().unwrap() = None;
                let history = params
                    .history
                    .as_ref()
                    .expect("actual host-read resume history");
                assert!(history.iter().any(|item| matches!(item, RolloutItem::SessionMeta(line) if line.meta.id == target && line.meta.parent_thread_id == Some(parent))));
                let (release, released) = tokio::sync::oneshot::channel();
                self.entered
                    .send(PendingPostAdmissionCreate {
                        child: target,
                        parent,
                        release,
                    })
                    .unwrap();
                released
                    .await
                    .expect("release actual post-admission resume writer");
            }
            self.inner.resume_thread_owned(params, preparation).await
        })
    }
    fn finish_thread_owned(
        &self,
        thread: ThreadId,
        owner: codex_thread_store::ThreadWriterOwner,
        discard: bool,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        self.inner.finish_thread_owned(thread, owner, discard)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    delegate_fresh_revocation_store! {
        fn resume_thread(params: codex_thread_store::ResumeThreadParams) -> Arc<Vec<RolloutItem>>;
        fn append_items(params: codex_thread_store::AppendThreadItemsParams) -> ();
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: codex_thread_store::LoadThreadHistoryParams) -> codex_thread_store::StoredThreadHistory;
        fn load_latest_model_context(params: codex_thread_store::LoadThreadHistoryParams) -> codex_thread_store::StoredModelContext;
        fn read_thread(params: codex_thread_store::ReadThreadParams) -> codex_thread_store::StoredThread;
        fn read_thread_by_rollout_path(params: codex_thread_store::ReadThreadByRolloutPathParams) -> codex_thread_store::StoredThread;
        fn list_threads(params: codex_thread_store::ListThreadsParams) -> codex_thread_store::ThreadPage;
        fn update_thread_metadata(params: codex_thread_store::UpdateThreadMetadataParams) -> Option<codex_thread_store::StoredThread>;
        fn archive_thread(params: codex_thread_store::ArchiveThreadParams) -> ();
        fn unarchive_thread(params: codex_thread_store::ArchiveThreadParams) -> codex_thread_store::StoredThread;
        fn delete_thread(params: codex_thread_store::DeleteThreadParams) -> ();
        fn flush_thread(thread_id: ThreadId) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
    }

    fn persist_thread(
        &self,
        thread_id: ThreadId,
        context: codex_thread_store::PersistContext,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        self.inner.persist_thread(thread_id, context)
    }

    fn create_thread(
        &self,
        params: codex_thread_store::CreateThreadParams,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let armed = *self.armed_parent.lock().unwrap();
            if params.parent_thread_id.is_some() && params.parent_thread_id == armed {
                let parent = armed.expect("armed actual parent");
                assert_eq!(params.source.parent_thread_id(), Some(parent));
                assert!(matches!(
                    &params.source,
                    SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
                ));
                *self.armed_parent.lock().unwrap() = None;
                let (release, released) = tokio::sync::oneshot::channel();
                self.entered
                    .send(PendingPostAdmissionCreate {
                        child: params.thread_id,
                        parent,
                        release,
                    })
                    .expect("actual store gate receiver");
                released
                    .await
                    .expect("release actual post-admission create");
            }
            self.inner.create_thread(params).await
        })
    }
}

#[test_case::test_case(0; "current post admission startup registers")]
#[test_case::test_case(1; "explicit close fences admitted unregistered child")]
#[test_case::test_case(2; "tree shutdown fences admitted unregistered child")]
#[tokio::test]
async fn post_admission_startup_cannot_escape_completed_subtree_shutdown(stage: u8) {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.model = Some("gpt-5.6-sol".into());
    let (entered, mut pending) = tokio::sync::mpsc::unbounded_channel();
    let store = Arc::new(PostAdmissionCreateGate {
        inner: InMemoryThreadStore::default(),
        armed_parent: std::sync::Mutex::new(None),
        armed_resume: std::sync::Mutex::new(None),
        entered,
    });
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth.clone(),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        store.clone(),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        manager,
        control,
    };
    let (caller_id, caller) = harness.start_thread().await;
    caller.ensure_rollout_materialized().await;
    caller.session.flush_rollout().await.unwrap();
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &caller, "post_admission_child").await;
    *store.armed_parent.lock().unwrap() = Some(caller_id);
    let start = fresh_lifetime_lower_start(&harness, &caller, prepared, source, permit);
    tokio::pin!(start);
    let blocked = tokio::select! {
        result = &mut start => panic!("must reach real post-admission store create: {}", result.is_ok()),
        reached = timeout(Duration::from_secs(/*secs*/ 10), pending.recv()) => reached.unwrap().expect("actual child store create"),
    };
    assert_eq!(blocked.parent, caller_id);
    assert_ne!(blocked.child, caller_id);
    assert!(caller.is_running());
    assert_eq!(harness.manager.list_thread_ids().await, vec![caller_id]);
    if stage == 0 {
        blocked.release.send(()).unwrap();
        let child = timeout(Duration::from_secs(/*secs*/ 10), start)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.thread_id, blocked.child);
        assert!(child.thread.is_running());
        assert!(Arc::ptr_eq(
            &child.thread,
            &harness.manager.get_thread(blocked.child).await.unwrap()
        ));
        timeout(
            Duration::from_secs(/*secs*/ 10),
            child.thread.shutdown_and_wait(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            harness
                .manager
                .remove_thread(&blocked.child)
                .await
                .is_some()
        );
        timeout(Duration::from_secs(/*secs*/ 10), caller.shutdown_and_wait())
            .await
            .unwrap()
            .unwrap();
        assert!(harness.manager.remove_thread(&caller_id).await.is_some());
        return;
    }
    let shutdown = async {
        if stage == 1 {
            harness.control.close_agent(caller_id).await.map(|_| ())
        } else {
            harness
                .control
                .shutdown_agent_tree(caller_id)
                .await
                .map(|_| ())
        }
    };
    tokio::pin!(shutdown);
    // Keep polling the independent startup as teardown runs: a repaired owner
    // may cancel and drain it instead of only rejecting its final registration.
    // The store gate itself remains closed until teardown has completed.
    let early = timeout(Duration::from_secs(/*secs*/ 10), async {
        tokio::select! {
            result = &mut shutdown => {
                result.unwrap();
                None
            },
            outcome = &mut start => {
                assert!(outcome.is_err(), "held real store creation cannot publish a successful child");
                shutdown.await.unwrap();
                Some(outcome)
            },
        }
    }).await.expect("actual subtree teardown completes while the admitted store gate is held");
    assert!(!caller.is_running());
    assert!(harness.manager.get_thread(caller_id).await.is_err());
    assert!(harness.manager.get_thread(blocked.child).await.is_err());
    let released = blocked.release.send(());
    let outcome = if let Some(outcome) = early {
        outcome
    } else {
        released.expect("independent unfinished startup still owns its store gate");
        timeout(Duration::from_secs(/*secs*/ 10), start)
            .await
            .unwrap()
    };
    let registered = harness.manager.get_thread(blocked.child).await.ok();
    let survivor_present = registered.is_some();
    let survivor =
        registered.or_else(|| outcome.as_ref().ok().map(|child| Arc::clone(&child.thread)));
    let survivor_running = survivor.as_ref().is_some_and(|child| child.is_running());
    tracing::info!(
        "POST_ADMISSION_SUBTREE_OBSERVED stage={stage} returned_success={} survivor_present={survivor_present} survivor_running={survivor_running}",
        outcome.is_ok()
    );
    if let Some(child) = survivor {
        timeout(Duration::from_secs(/*secs*/ 10), child.shutdown_and_wait())
            .await
            .unwrap()
            .unwrap();
        let _ = harness.manager.remove_thread(&blocked.child).await;
    }
    assert!(
        outcome.is_err(),
        "completed subtree teardown must reject independently awaited in-flight native startup: returned_success={} survivor_present={survivor_present} survivor_running={survivor_running}",
        outcome.is_ok()
    );
    assert!(
        !survivor_present && !survivor_running,
        "no admitted child may register or run after completed subtree teardown: survivor_present={survivor_present} survivor_running={survivor_running}"
    );
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

fn loaded_fixture_policy_denial(error: &str) -> bool {
    [
        "child_provider_not_authorized",
        "child_provider_untrusted_source",
        "child_provider_policy_conflict",
    ]
    .iter()
    .any(|code| error.contains(code))
}

async fn loaded_custom_provider_resident(
    harness: &AgentControlHarness,
    parent: &Arc<CodexThread>,
) -> Arc<CodexThread> {
    let runtime = &harness.control.runtime;
    let state = runtime.upgrade().unwrap();
    let membership = runtime.admit_start().unwrap();
    let slot = harness
        .control
        .reserve_v2_residency_slot(
            &state,
            &harness.config,
            &membership,
            /*protected_thread_id*/ None,
        )
        .await
        .unwrap();
    let turn = parent.session.new_default_turn().await;
    let step = StepContext::for_test(turn);
    let mut prepared = crate::agent::child_config::prepare_agent_spawn_config(
        &parent.session,
        &step,
        crate::agent::child_config::SpawnConfigOptions {
            resolved_provider: None,
            model_provider: Some("a"),
            version: crate::agent::child_config::SpawnConfigVersion::V2,
            full_history_fork: false,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .expect("real trusted custom-provider preparation")
    .config;
    assert_eq!(prepared.model_provider_id, "a");
    let mut reservation = runtime
        .registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .unwrap();
    let (source, mut metadata) = harness
        .control
        .prepare_thread_spawn(
            &mut reservation,
            &prepared,
            parent.session.thread_id,
            next_thread_spawn_depth(&parent.session_source),
            Some(AgentPath::from_string("/root/loaded_custom_authority_target".into()).unwrap()),
            /*agent_role*/ None,
            /*preferred_agent_nickname*/ None,
        )
        .unwrap();
    let permit = prepared
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            runtime,
            parent.session.thread_id,
            &prepared,
            /*full_history*/ false,
            &source,
        )
        .await
        .expect("consume actual prepared custom-provider proof");
    let child = fresh_lifetime_lower_start(harness, parent, prepared, source, permit)
        .await
        .unwrap();
    metadata.agent_id = Some(child.thread_id);
    reservation.commit(metadata);
    slot.commit(child.thread_id);
    // An idle lower-start has not activated its speculative rollout yet.
    // Materialize through the actual Session before the owner route reads it.
    child.thread.ensure_rollout_materialized().await;
    child.thread.flush_rollout().await.unwrap();
    assert_eq!(
        child.thread.multi_agent_version(),
        Some(MultiAgentVersion::V2)
    );
    let stored = child
        .thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .unwrap();
    let history = stored
        .history
        .expect("actual lower-start persisted startup history");
    assert!(history.items.iter().any(|item| matches!(item,
        RolloutItem::SessionMeta(line) if line.meta.id == child.thread_id && line.meta.multi_agent_version == Some(MultiAgentVersion::V2)
    )), "actual persisted V2 metadata required by owner-loaded path");
    assert!(
        runtime
            .registry
            .agent_metadata_for_thread(child.thread_id)
            .is_some()
    );
    assert!(child.thread.session.active_turn.lock().await.is_none());
    assert_eq!(child.thread.config_snapshot().await.model_provider_id, "a");
    child.thread
}

// These regressions deliberately retain already loaded allocations. Reloading a
// target would exercise a different, already guarded authorization path.
#[test_case::test_case(0, 0; "loaded user input current")]
#[test_case::test_case(1, 0; "loaded trigger current")]
#[test_case::test_case(2, 0; "loaded owner ensure current")]
#[test_case::test_case(0, 1; "loaded user input host revoked")]
#[test_case::test_case(1, 1; "loaded trigger host revoked")]
#[test_case::test_case(2, 1; "loaded owner ensure host revoked")]
#[test_case::test_case(0, 2; "loaded user input managed revoked")]
#[test_case::test_case(1, 2; "loaded trigger managed revoked")]
#[test_case::test_case(2, 2; "loaded owner ensure managed revoked")]
#[test_case::test_case(0, 3; "loaded user input executor scope widened")]
#[test_case::test_case(1, 3; "loaded trigger executor scope widened")]
#[test_case::test_case(2, 3; "loaded owner ensure executor scope widened")]
#[tokio::test]
async fn loaded_native_authority_rechecked_before_delivery(route: u8, change: u8) {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let host_file = home.path().join("config.toml");
    let catalog_file = home.path().join("loaded-a.json");
    std::fs::write(
        &host_file,
        r#"
model = "gpt-5.5"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "a"]
[agents.provider_defaults.a]
model = "shared-loaded"
model_catalog_json = "loaded-a.json"
[model_providers.a]
name = "Loaded custom fixture"
base_url = "http://127.0.0.1:9/v1"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 1000
"#,
    )
    .unwrap();
    let mut info = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|info| info.slug == "gpt-5.5")
        .expect("real bundled model metadata");
    info.slug = "shared-loaded".into();
    info.multi_agent_version = Some(MultiAgentVersion::V2);
    info.supports_search_tool = false;
    info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    info.used_fallback_model_metadata = false;
    std::fs::write(
        &catalog_file,
        serde_json::to_vec(&serde_json::json!({"models":[info]})).unwrap(),
    )
    .unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::config::ConfigOverrides {
            cwd: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    config.config_layer_stack = config
        .config_layer_stack
        .with_cloud_config_binding(snapshot.binding);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_thread().await;
    assert_eq!(parent.config_snapshot().await.model_provider_id, "openai");
    let target = loaded_custom_provider_resident(&harness, &parent).await;
    let target_id = target.session.thread_id;
    assert!(target.is_running());
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));
    let before_ops = harness.manager.captured_ops().len();

    match change {
        0 => {}
        1 => {
            let mut value: TomlValue =
                toml::from_str(&std::fs::read_to_string(&host_file).unwrap()).unwrap();
            let before_definition = value["model_providers"]["a"].clone();
            let before_defaults = value["agents"]["provider_defaults"]["a"].clone();
            value["agents"]["allowed_model_providers"] =
                TomlValue::Array(vec![TomlValue::String("openai".into())]);
            std::fs::write(&host_file, toml::to_string(&value).unwrap()).unwrap();
            assert_eq!(value["model_providers"]["a"], before_definition);
            assert_eq!(value["agents"]["provider_defaults"]["a"], before_defaults);
            let next = ConfigBuilder::without_managed_config_for_tests()
                .codex_home(harness.config.codex_home.to_path_buf())
                .harness_overrides(crate::config::ConfigOverrides {
                    cwd: Some(project.path().to_path_buf()),
                    ..Default::default()
                })
                .build()
                .await
                .unwrap();
            let current = parent.config().await;
            assert_eq!(
                parent.refresh_runtime_config(current, next).await,
                crate::ConfigRefreshOutcome::Published
            );
            let published = parent.config().await;
            let effective = published.config_layer_stack.effective_config();
            assert!(
                !effective["agents"]["allowed_model_providers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|id| id.as_str() == Some("a"))
            );
            assert_eq!(published.model_provider_id, "openai");
        }
        2 => {
            let mut bundle = codex_config::CloudConfigBundle::default();
            bundle
                .config_toml
                .enterprise_managed
                .push(codex_config::CloudConfigFragment {
                    id: "loaded-native-revocation".into(),
                    name: "revoke loaded provider".into(),
                    contents: "[agents]\nallowed_model_providers=['openai']\n".into(),
                });
            policy.observe_remote_bundle(&bundle).unwrap();
            assert!(
                parent
                    .config()
                    .await
                    .config_layer_stack
                    .cloud_config_binding()
                    .is_some_and(|binding| binding.read().status
                        != codex_config::CloudConfigBundleBindingStatus::Current)
            );
        }
        3 => {
            let environments = parent.session.inherited_environment_snapshot().await;
            let ready = environments.primary().expect("actual invoking executor");
            let mut owner = ready.config().clone();
            owner.permission_profile = codex_protocol::models::PermissionProfileSnapshot::legacy(
                PermissionProfile::Disabled,
            );
            let mut selection = ready.selection();
            selection.config = EnvironmentConfigState::Ready(owner);
            parent
                .update_thread_settings(ThreadSettingsOverrides {
                    permission_profile: Some(PermissionProfile::Disabled),
                    environments: Some(TurnEnvironmentRequests::new(
                        parent.thread_settings_snapshot().await.cwd,
                        vec![selection.into_request()],
                    )),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        _ => unreachable!(),
    }
    let caller_turn = parent
        .session
        .new_turn_with_default_settings("loaded-authority-followup".into(), Default::default())
        .await;
    let resume_config =
        crate::agent::child_config::build_agent_resume_config(&caller_turn).unwrap();
    let captured = crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
        &parent.session,
        &parent.session_source,
    )
    .await;
    assert!(
        captured.scope
            == crate::agent::child_provider_authorization::ChildProviderScope::from_step(
                &resume_config,
                &caller_turn.initial_environments,
            ),
        "caller settings and actual executor scope must agree before the loaded operation"
    );
    if change == 3 {
        let ready = caller_turn
            .initial_environments
            .primary()
            .expect("actual widened caller executor");
        let filesystem = ready
            .permission_profile_with_workspace_roots()
            .file_system_sandbox_policy();
        assert!(
            filesystem.can_write_local_path_with_cwd(&host_file, caller_turn.config.cwd.as_path())
        );
        assert!(
            filesystem
                .can_write_local_path_with_cwd(&catalog_file, caller_turn.config.cwd.as_path())
        );
    }
    let outcome = if route == 2 {
        harness
            .control
            .ensure_child_loaded(parent_id, target_id)
            .await
    } else {
        harness
            .control
            .send(SendRequest {
                caller: parent_id,
                target: AgentTarget::Id(target_id),
                resume_config,
                input: if route == 0 {
                    AgentInput::UserInput(text_input("loaded authority followup"))
                } else {
                    AgentInput::Message {
                        message: AgentMessage::Plaintext("loaded authority followup".into()),
                        mode: MessageDeliveryMode::TriggerTurn,
                    }
                },
                start_options: Default::default(),
            })
            .await
            .map(|_| ())
    };
    if change == 0 && route == 0 {
        wait_for_recorded_user_message(target.as_ref(), "loaded authority followup").await;
    }
    let after_ops = harness.manager.captured_ops().len();
    let same_allocation = Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap(),
    );
    // Always finish the owned actual Sessions before an expected RED assertion.
    tokio::time::timeout(Duration::from_secs(10), target.shutdown_and_wait())
        .await
        .expect("bounded target cleanup")
        .unwrap();
    harness.manager.remove_thread(&target_id).await;
    tokio::time::timeout(Duration::from_secs(10), parent.shutdown_and_wait())
        .await
        .expect("bounded parent cleanup")
        .unwrap();
    harness.manager.remove_thread(&parent_id).await;
    assert!(
        same_allocation,
        "the test must never reconstruct or replace the target"
    );
    if change == 0 {
        outcome.expect("current policy must allow loaded native work");
        // The existing ops observer captures inter-agent delivery, not UserTurn.
        // User input is proved by the actual recorded message above.
        assert_eq!(after_ops - before_ops, usize::from(route == 1));
    } else {
        let error = outcome
            .err()
            .expect("revoked loaded authority must reject before accepting new work");
        assert!(loaded_fixture_policy_denial(&error.to_string()), "{error}");
        assert_eq!(
            after_ops, before_ops,
            "revocation must precede actual target submission"
        );
    }
}

async fn post_admission_lifecycle_harness(
    version: MultiAgentVersion,
) -> (
    AgentControlHarness,
    Arc<PostAdmissionCreateGate>,
    tokio::sync::mpsc::UnboundedReceiver<PendingPostAdmissionCreate>,
) {
    let (home, mut config) = if version == MultiAgentVersion::V1 {
        let home = TempDir::new().unwrap();
        let mut info = codex_models_manager::bundled_models_response()
            .unwrap()
            .models
            .into_iter()
            .find(|info| info.slug == "gpt-5.5")
            .unwrap();
        info.multi_agent_version = Some(MultiAgentVersion::V1);
        info.supports_search_tool = false;
        info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
        info.used_fallback_model_metadata = false;
        std::fs::write(
            home.path().join("lifecycle-v1.json"),
            serde_json::to_vec(&serde_json::json!({"models": [info]})).unwrap(),
        )
        .unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "model = \"gpt-5.5\"\nmodel_catalog_json = \"lifecycle-v1.json\"\n",
        )
        .unwrap();
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .unwrap();
        (home, config)
    } else {
        test_config().await
    };
    config.features.enable(Feature::MultiAgentV2).unwrap();
    if version == MultiAgentVersion::V1 {
        config
            .features
            .disable(Feature::MultiAgentV2)
            .expect("disable MultiAgentV2 for the V1 lifecycle fixture");
    }
    if version == MultiAgentVersion::V2 {
        config.model = Some("gpt-5.6-sol".into());
    }
    config.multi_agent_v2.max_concurrent_threads_per_session = 8;
    config.agent_max_depth = 8;
    let (entered, pending) = tokio::sync::mpsc::unbounded_channel();
    let store = Arc::new(PostAdmissionCreateGate {
        inner: InMemoryThreadStore::default(),
        armed_parent: std::sync::Mutex::new(None),
        armed_resume: std::sync::Mutex::new(None),
        entered,
    });
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth.clone(),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        store.clone(),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        manager,
        control,
    };
    (harness, store, pending)
}

#[test_case::test_case(false, false; "loaded owner teardown wins late grandchild")]
#[test_case::test_case(false, true; "loaded owner registration wins and joins teardown")]
#[test_case::test_case(true, false; "unloaded owner teardown fences loaded grandchild caller")]
#[test_case::test_case(true, true; "unloaded owner includes registered great grandchild")]
#[tokio::test]
async fn post_admission_descendant_registration_orders_with_ancestor_shutdown(
    unloaded: bool,
    registration_wins: bool,
) {
    let (harness, store, mut pending) =
        post_admission_lifecycle_harness(MultiAgentVersion::V2).await;
    let (root_id, root) = harness.start_thread().await;
    let sibling =
        post_admission_registered_idle_child(&harness, &root, "independent_sibling").await;
    let owner = post_admission_registered_idle_child(&harness, &root, "subtree_owner").await;
    let caller =
        post_admission_registered_idle_child(&harness, &owner, "loaded_grandchild_caller").await;
    residency_checkpoint_mark_idle(&sibling).await;
    residency_checkpoint_mark_idle(&owner).await;
    // A terminal caller would enqueue its completion in the owner's SessionIo,
    // retaining the owner's residency read guard until that submission dispatches.
    // This race needs a loaded caller, not a completed caller with pending mail.
    let caller_turn = caller.session.new_default_turn().await;
    assert_eq!(caller_turn.multi_agent_version, MultiAgentVersion::V2);
    assert_eq!(caller.multi_agent_version(), Some(MultiAgentVersion::V2));
    assert_eq!(caller.agent_status().await, AgentStatus::PendingInit);
    assert!(caller.session.active_turn.lock().await.is_none());
    let owner_id = owner.session.thread_id;
    let caller_id = caller.session.thread_id;
    if unloaded {
        let status = owner.agent_status().await;
        let active = owner.session.active_turn.lock().await.is_some();
        let sleeping = owner.session.has_outstanding_durable_sleep();
        let trigger_mail = owner
            .session
            .input_queue
            .has_trigger_turn_mailbox_items()
            .await;
        let same = Arc::ptr_eq(&owner, &harness.manager.get_thread(owner_id).await.unwrap());
        let registry_current = harness
            .control
            .runtime
            .registry
            .graph_open_is_current(&owner.session);
        let retained_matches = harness
            .control
            .runtime
            .retained_authority(owner_id)
            .is_some_and(|authority| authority.matches_session(&owner.session));
        let gate_available = owner.residency_gate.try_write().is_ok();
        assert!(
            matches!(status, AgentStatus::Completed(_))
                && !active
                && !sleeping
                && !trigger_mail
                && same
                && registry_current
                && retained_matches
                && gate_available,
            "actual owner eviction prerequisites: status={status:?} active={active} sleeping={sleeping} trigger_mail={trigger_mail} same={same} registry_current={registry_current} retained_matches={retained_matches} gate_available={gate_available}"
        );
        assert_eq!(
            harness
                .manager
                .try_evict_v2_thread(Arc::clone(&owner))
                .await
                .unwrap(),
            ThreadEvictionOutcome::Evicted
        );
        assert!(!owner.is_running());
        assert!(harness.manager.get_thread(owner_id).await.is_err());
        assert!(caller.is_running());
    }
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &caller, "late_descendant").await;
    *store.armed_parent.lock().unwrap() = Some(caller_id);
    let start = fresh_lifetime_lower_start(&harness, &caller, prepared, source, permit);
    tokio::pin!(start);
    let blocked = tokio::select! {
        result = &mut start => panic!("must enter actual post-admission descendant writer create: {}", result.is_ok()),
        entered = timeout(Duration::from_secs(10), pending.recv()) => entered.unwrap().unwrap(),
    };
    assert_eq!(blocked.parent, caller_id);
    let mut release = Some(blocked.release);
    let early_child = if registration_wins {
        release.take().unwrap().send(()).unwrap();
        Some(
            timeout(Duration::from_secs(10), &mut start)
                .await
                .unwrap()
                .unwrap()
                .thread,
        )
    } else {
        None
    };
    let teardown = timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(owner_id),
    )
    .await
    .unwrap();
    if unloaded {
        assert!(matches!(
            teardown.unwrap_err().details(),
            CodexErrorDetails::ThreadNotFound(_)
        ));
    } else {
        teardown.unwrap();
    }
    assert!(!caller.is_running());
    assert!(harness.manager.get_thread(caller_id).await.is_err());
    if let Some(child) = early_child {
        assert!(
            !child.is_running(),
            "registration winning the shared map ordering must be included in teardown"
        );
    } else {
        release.take().unwrap().send(()).unwrap();
        assert!(
            timeout(Duration::from_secs(10), &mut start)
                .await
                .unwrap()
                .is_err(),
            "teardown winning the shared map ordering must reject late registration"
        );
    }
    assert!(harness.manager.get_thread(blocked.child).await.is_err());
    if !registration_wins {
        assert!(
            matches!(store.inner.load_history(codex_thread_store::LoadThreadHistoryParams { thread_id: blocked.child, include_archived: true }).await,
            Err(codex_thread_store::ThreadStoreError::ThreadNotFound { thread_id }) if thread_id == blocked.child),
            "denied fresh acquisition must leave no provisional history"
        );
    }
    assert!(
        root.is_running(),
        "subtree closure must not stop the outside root"
    );
    assert!(
        sibling.is_running(),
        "subtree closure must not stop a sibling"
    );
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

async fn post_admission_registered_idle_child(
    harness: &AgentControlHarness,
    parent: &Arc<CodexThread>,
    name: &str,
) -> Arc<CodexThread> {
    let parent_id = parent.session.thread_id;
    let source = thread_spawn_source(
        parent_id,
        &parent.session_source,
        next_thread_spawn_depth(&parent.session_source),
        /*agent_role*/ None,
        Some(name.into()),
    )
    .unwrap();
    let mut prepared = harness
        .control
        .prepare_spawn_fixture(&harness.config, &source, /*full_history*/ false)
        .await
        .unwrap();
    let mut reservation = harness
        .control
        .runtime
        .registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .unwrap();
    let role = prepared
        .prepared_child_authorization
        .as_ref()
        .unwrap()
        .role_name();
    let (source, mut metadata) = harness
        .control
        .prepare_thread_spawn(
            &mut reservation,
            &prepared,
            parent_id,
            next_thread_spawn_depth(&parent.session_source),
            source.get_agent_path(),
            role,
            /*preferred_agent_nickname*/ None,
        )
        .unwrap();
    let permit = prepared
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            &harness.control.runtime,
            parent_id,
            &prepared,
            /*full_history*/ false,
            &source,
        )
        .await
        .unwrap();
    let state = harness.control.runtime.upgrade().unwrap();
    let membership = harness.control.runtime.admit_start().unwrap();
    let slot = harness
        .control
        .reserve_v2_residency_slot(&state, &prepared, &membership, Some(parent_id))
        .await
        .unwrap();
    let child = fresh_lifetime_lower_start(harness, parent, prepared, source, permit)
        .await
        .unwrap();
    metadata.agent_id = Some(child.thread_id);
    reservation.commit(metadata);
    slot.commit(child.thread_id);
    child.thread.ensure_rollout_materialized().await;
    child.thread.session.flush_rollout().await.unwrap();
    assert!(child.thread.session.active_turn.lock().await.is_none());
    child.thread
}

#[test_case::test_case(0; "outside caller current owned resume positive")]
#[test_case::test_case(1; "outside caller cannot resume target closed after admission")]
#[test_case::test_case(2; "second explicit close invalidates new resume of already closed target")]
#[tokio::test]
async fn post_admission_resume_target_revision_fences_outside_live_caller(stage: u8) {
    let (harness, store, mut pending) =
        post_admission_lifecycle_harness(MultiAgentVersion::V2).await;
    let (root_id, root) = harness.start_thread().await;
    let turn = root.session.new_default_turn().await;
    assert_eq!(turn.multi_agent_version, MultiAgentVersion::V2);
    assert_eq!(root.multi_agent_version(), Some(MultiAgentVersion::V2));
    let target = post_admission_registered_idle_child(&harness, &root, "resume_target").await;
    // An idle lower-start has no real execution TurnContext yet. Persist its
    // actual policy observation instead of inheriting the memory store's defaults.
    let target_turn = target.session.new_default_turn().await;
    let target_context = target_turn.to_turn_context_item();
    let caller_context = turn.to_turn_context_item();
    assert_eq!(
        target_context.approval_policy,
        caller_context.approval_policy
    );
    assert_eq!(
        target_context.permission_profile(),
        caller_context.permission_profile()
    );
    assert!(
        target
            .session
            .persist_rollout_items(&[RolloutItem::TurnContext(target_context.clone())])
            .await
    );
    target.session.flush_rollout().await.unwrap();
    let stored = target
        .read_thread(
            /*include_archived*/ false, /*include_history*/ false,
        )
        .await
        .unwrap();
    assert_eq!(stored.approval_mode, caller_context.approval_policy);
    assert_eq!(
        stored.permission_profile,
        caller_context.permission_profile()
    );
    let target_id = target.session.thread_id;
    let source = target.session_source.clone();
    if stage == 2 {
        harness.control.close_agent(target_id).await.unwrap();
    } else {
        assert_eq!(
            harness
                .manager
                .try_evict_v2_thread(Arc::clone(&target))
                .await
                .unwrap(),
            ThreadEvictionOutcome::Evicted
        );
    }
    assert!(!target.is_running());
    assert!(harness.manager.get_thread(target_id).await.is_err());
    *store.armed_resume.lock().unwrap() = Some((target_id, root_id));
    let resume =
        harness
            .control
            .resume_agent_from_rollout(harness.config.clone(), target_id, source);
    tokio::pin!(resume);
    let blocked = tokio::select! {
        result = &mut resume => panic!("must enter actual resume acquisition after admission: {result:?}"),
        entered = timeout(Duration::from_secs(10), pending.recv()) => entered.unwrap().unwrap(),
    };
    assert_eq!(blocked.child, target_id);
    assert_eq!(blocked.parent, root_id);
    if stage != 0 {
        let closed = harness.control.close_agent(target_id).await;
        if stage == 1 {
            closed.unwrap();
        } else {
            assert!(matches!(
                closed.unwrap_err().details(),
                CodexErrorDetails::ThreadNotFound(_)
            ));
        }
    }
    assert!(
        root.is_running(),
        "actual caller outside the target subtree stays current"
    );
    blocked.release.send(()).unwrap();
    let result = timeout(Duration::from_secs(10), resume).await.unwrap();
    if stage == 0 {
        assert_eq!(result.unwrap(), target_id);
        assert!(
            harness
                .manager
                .get_thread(target_id)
                .await
                .unwrap()
                .is_running()
        );
    } else {
        assert!(
            result
                .as_ref()
                .err()
                .is_some_and(|error| error.to_string().contains("child_provider_policy_conflict")),
            "target teardown revision must invalidate an admitted resume even when its actual outside caller remains live"
        );
        assert!(harness.manager.get_thread(target_id).await.is_err());
        assert!(
            !store
                .inner
                .load_history(codex_thread_store::LoadThreadHistoryParams {
                    thread_id: target_id,
                    include_archived: true
                })
                .await
                .unwrap()
                .items
                .is_empty(),
            "denied resume must retain existing history"
        );
        // A deliberate new operation after the completed close captures the new
        // revision and remains supported; the previous failed operation grants nothing.
        let reopened = timeout(
            Duration::from_secs(10),
            harness.control.resume_agent_from_rollout(
                harness.config.clone(),
                target_id,
                target.session_source.clone(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(reopened, target_id);
    }
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

#[test_case::test_case(0; "v1 current post admission startup registers")]
#[test_case::test_case(1; "v1 explicit close fences admitted unregistered child")]
#[test_case::test_case(2; "v1 tree shutdown fences admitted unregistered child")]
#[tokio::test]
async fn post_admission_v1_startup_cannot_escape_completed_subtree_shutdown(stage: u8) {
    let (harness, store, mut pending) =
        post_admission_lifecycle_harness(MultiAgentVersion::V1).await;
    let (caller_id, caller) = harness.start_thread().await;
    let turn = caller.session.new_default_turn().await;
    assert_eq!(
        turn.model_info().multi_agent_version,
        Some(MultiAgentVersion::V1)
    );
    assert_eq!(turn.multi_agent_version, MultiAgentVersion::V1);
    caller.ensure_rollout_materialized().await;
    caller.session.flush_rollout().await.unwrap();
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &caller, "post_admission_child").await;
    assert_eq!(caller.multi_agent_version(), Some(MultiAgentVersion::V1));
    *store.armed_parent.lock().unwrap() = Some(caller_id);
    let start = fresh_lifetime_lower_start(&harness, &caller, prepared, source, permit);
    tokio::pin!(start);
    let blocked = tokio::select! {
        result = &mut start => panic!("must reach real post-admission store create: {}", result.is_ok()),
        reached = timeout(Duration::from_secs(/*secs*/ 10), pending.recv()) => reached.unwrap().expect("actual child store create"),
    };
    assert_eq!(blocked.parent, caller_id);
    assert_ne!(blocked.child, caller_id);
    assert!(caller.is_running());
    assert_eq!(harness.manager.list_thread_ids().await, vec![caller_id]);
    if stage == 0 {
        blocked.release.send(()).unwrap();
        let child = timeout(Duration::from_secs(/*secs*/ 10), start)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.thread_id, blocked.child);
        assert!(child.thread.is_running());
        assert!(Arc::ptr_eq(
            &child.thread,
            &harness.manager.get_thread(blocked.child).await.unwrap()
        ));
        timeout(
            Duration::from_secs(/*secs*/ 10),
            child.thread.shutdown_and_wait(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            harness
                .manager
                .remove_thread(&blocked.child)
                .await
                .is_some()
        );
        timeout(Duration::from_secs(/*secs*/ 10), caller.shutdown_and_wait())
            .await
            .unwrap()
            .unwrap();
        assert!(harness.manager.remove_thread(&caller_id).await.is_some());
        return;
    }
    let shutdown = async {
        if stage == 1 {
            harness.control.close_agent(caller_id).await.map(|_| ())
        } else {
            harness
                .control
                .shutdown_agent_tree(caller_id)
                .await
                .map(|_| ())
        }
    };
    tokio::pin!(shutdown);
    // Keep polling the independent startup as teardown runs: a repaired owner
    // may cancel and drain it instead of only rejecting its final registration.
    // The store gate itself remains closed until teardown has completed.
    let early = timeout(Duration::from_secs(/*secs*/ 10), async {
        tokio::select! {
            result = &mut shutdown => {
                result.unwrap();
                None
            },
            outcome = &mut start => {
                assert!(outcome.is_err(), "held real store creation cannot publish a successful child");
                shutdown.await.unwrap();
                Some(outcome)
            },
        }
    }).await.expect("actual subtree teardown completes while the admitted store gate is held");
    assert!(!caller.is_running());
    assert!(harness.manager.get_thread(caller_id).await.is_err());
    assert!(harness.manager.get_thread(blocked.child).await.is_err());
    let released = blocked.release.send(());
    let outcome = if let Some(outcome) = early {
        outcome
    } else {
        released.expect("independent unfinished startup still owns its store gate");
        timeout(Duration::from_secs(/*secs*/ 10), start)
            .await
            .unwrap()
    };
    let registered = harness.manager.get_thread(blocked.child).await.ok();
    let survivor_present = registered.is_some();
    let survivor =
        registered.or_else(|| outcome.as_ref().ok().map(|child| Arc::clone(&child.thread)));
    let survivor_running = survivor.as_ref().is_some_and(|child| child.is_running());
    tracing::info!(
        "POST_ADMISSION_SUBTREE_OBSERVED stage={stage} returned_success={} survivor_present={survivor_present} survivor_running={survivor_running}",
        outcome.is_ok()
    );
    if let Some(child) = survivor {
        timeout(Duration::from_secs(/*secs*/ 10), child.shutdown_and_wait())
            .await
            .unwrap()
            .unwrap();
        let _ = harness.manager.remove_thread(&blocked.child).await;
    }
    assert!(
        outcome.is_err(),
        "completed subtree teardown must reject independently awaited in-flight native startup: returned_success={} survivor_present={survivor_present} survivor_running={survivor_running}",
        outcome.is_ok()
    );
    assert!(
        !survivor_present && !survivor_running,
        "no admitted child may register or run after completed subtree teardown: survivor_present={survivor_present} survivor_running={survivor_running}"
    );
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

#[tokio::test]
async fn post_admission_stale_caller_cannot_register_or_retire_same_id_replacement() {
    let (harness, store, mut pending) =
        post_admission_lifecycle_harness(MultiAgentVersion::V2).await;
    let (root_id, root) = harness.start_thread().await;
    root.ensure_rollout_materialized().await;
    root.session.flush_rollout().await.unwrap();
    let state = harness.control.runtime.upgrade().unwrap();
    let stored = state
        .read_stored_thread(codex_thread_store::ReadThreadParams {
            thread_id: root_id,
            include_archived: true,
            include_history: true,
        })
        .await
        .unwrap();
    let initial = InitialHistory::Resumed(ResumedHistory {
        history_revision: None,
        conversation_id: root_id,
        history: Arc::new(stored.history.unwrap().items),
        rollout_path: stored.rollout_path,
    });
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &root, "replacement_race_child").await;
    *store.armed_parent.lock().unwrap() = Some(root_id);
    let start = fresh_lifetime_lower_start(&harness, &root, prepared, source, permit);
    tokio::pin!(start);
    let blocked = tokio::select! {
        result = &mut start => panic!("must reach admitted real writer acquisition: {}", result.is_ok()),
        entered = timeout(Duration::from_secs(10), pending.recv()) => entered.unwrap().unwrap(),
    };
    timeout(Duration::from_secs(10), root.shutdown_and_wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        harness
            .manager
            .remove_thread_if_matches(&root_id, &root)
            .await
            .is_some()
    );
    let replacement = timeout(
        Duration::from_secs(10),
        harness.manager.start_thread_with_control_for_tests(
            StartThreadOptions {
                initial_history: initial,
                history_mode: Some(ThreadHistoryMode::Legacy),
                session_source: Some(root.session_source.clone()),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        ),
    )
    .await
    .unwrap()
    .unwrap()
    .thread;
    assert_eq!(replacement.session.thread_id, root_id);
    assert!(!Arc::ptr_eq(&root, &replacement));
    let epoch = harness.control.runtime.retained_authority(root_id).unwrap();
    assert!(replacement.is_running());
    blocked.release.send(()).unwrap();
    assert!(
        timeout(Duration::from_secs(10), start)
            .await
            .unwrap()
            .is_err()
    );
    assert!(harness.manager.get_thread(blocked.child).await.is_err());
    assert!(
        matches!(store.inner.load_history(codex_thread_store::LoadThreadHistoryParams { thread_id: blocked.child, include_archived: true }).await,
        Err(codex_thread_store::ThreadStoreError::ThreadNotFound { thread_id }) if thread_id == blocked.child),
        "stale native startup must clean only its own fresh history"
    );
    assert!(Arc::ptr_eq(
        &harness.manager.get_thread(root_id).await.unwrap(),
        &replacement
    ));
    assert!(replacement.is_running());
    assert!(
        harness
            .control
            .runtime
            .retained_authority(root_id)
            .unwrap()
            .same_registration(&epoch)
    );
    assert!(
        harness
            .manager
            .remove_thread_if_matches(&root_id, &root)
            .await
            .is_none()
    );
    assert!(replacement.is_running());
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

struct LegacyRootOnlyThreadStore {
    inner: InMemoryThreadStore,
}
impl ThreadStore for LegacyRootOnlyThreadStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    delegate_fresh_revocation_store! {
        fn create_thread(params: codex_thread_store::CreateThreadParams) -> ();
        fn resume_thread(params: codex_thread_store::ResumeThreadParams) -> Arc<Vec<RolloutItem>>;
        fn append_items(params: codex_thread_store::AppendThreadItemsParams) -> ();
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: codex_thread_store::LoadThreadHistoryParams) -> codex_thread_store::StoredThreadHistory;
        fn read_thread(params: codex_thread_store::ReadThreadParams) -> codex_thread_store::StoredThread;
        fn read_thread_by_rollout_path(params: codex_thread_store::ReadThreadByRolloutPathParams) -> codex_thread_store::StoredThread;
        fn list_threads(params: codex_thread_store::ListThreadsParams) -> codex_thread_store::ThreadPage;
        fn update_thread_metadata(params: codex_thread_store::UpdateThreadMetadataParams) -> Option<codex_thread_store::StoredThread>;
        fn archive_thread(params: codex_thread_store::ArchiveThreadParams) -> ();
        fn unarchive_thread(params: codex_thread_store::ArchiveThreadParams) -> codex_thread_store::StoredThread;
        fn delete_thread(params: codex_thread_store::DeleteThreadParams) -> ();
        fn flush_thread(thread_id: ThreadId) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
    }
    fn persist_thread(
        &self,
        id: ThreadId,
        context: codex_thread_store::PersistContext,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        self.inner.persist_thread(id, context)
    }
}

#[tokio::test]
async fn legacy_custom_store_root_remains_supported_without_native_owned_writer_fallback() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.model = Some("gpt-5.6-sol".into());
    let store = Arc::new(LegacyRootOnlyThreadStore {
        inner: InMemoryThreadStore::default(),
    });
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth.clone(),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        store.clone(),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        manager,
        control,
    };
    let (root_id, root) = harness.start_thread().await;
    root.ensure_rollout_materialized().await;
    root.session.flush_rollout().await.unwrap();
    assert!(root.is_running());
    assert_eq!(store.inner.calls().await.create_thread, 1);
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &root, "unsupported_native_owner").await;
    let denied = timeout(
        Duration::from_secs(10),
        fresh_lifetime_lower_start(&harness, &root, prepared, source, permit),
    )
    .await
    .unwrap();
    assert!(
        denied
            .as_ref()
            .err()
            .is_some_and(|error| error.to_string().contains("owned_writer")),
        "unknown native store must fail before acquisition, with no ID-only fallback"
    );
    assert_eq!(
        store.inner.calls().await.create_thread,
        1,
        "native rejection must never invoke legacy create"
    );
    assert!(root.is_running(), "ordinary existing root remains usable");
    timeout(Duration::from_secs(10), root.shutdown_and_wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        harness
            .manager
            .remove_thread_if_matches(&root_id, &root)
            .await
            .is_some()
    );
    assert!(store.inner.calls().await.shutdown_thread >= 1);
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

// Exercise the actual guarded-queue implementation with actual registered
// sessions. A cloned SessionIo replaces only the queue with a bounded observed
// queue, so no Core task can consume capacity before the publication barrier.
#[test_case::test_case(0; "prepared operation queue unchanged positive")]
#[test_case::test_case(1; "publication while queue capacity pending")]
#[test_case::test_case(2; "permission and environment publication while queue capacity pending")]
#[test_case::test_case(3; "no-op settings preserves pending operation")]
#[test_case::test_case(4; "receiver drop wakes every guarded producer")]
#[test_case::test_case(5; "cancelled guarded producer never queues after capacity release")]
#[tokio::test]
async fn loaded_operation_publication_between_check_and_enqueue(change: u8) {
    let revoke = matches!(change, 1 | 2);
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let host_file = home.path().join("config.toml");
    let catalog_file = home.path().join("loaded-a.json");
    std::fs::write(
        &host_file,
        r#"
model = "gpt-5.5"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "a"]
[agents.provider_defaults.a]
model = "shared-loaded"
model_catalog_json = "loaded-a.json"
[model_providers.a]
name = "Loaded custom fixture"
base_url = "http://127.0.0.1:9/v1"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 1000
"#,
    )
    .unwrap();
    let mut info = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|info| info.slug == "gpt-5.5")
        .expect("real bundled model metadata");
    info.slug = "shared-loaded".into();
    info.multi_agent_version = Some(MultiAgentVersion::V2);
    info.supports_search_tool = false;
    info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    info.used_fallback_model_metadata = false;
    std::fs::write(
        &catalog_file,
        serde_json::to_vec(&serde_json::json!({"models":[info]})).unwrap(),
    )
    .unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::config::ConfigOverrides {
            cwd: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    config.config_layer_stack = config
        .config_layer_stack
        .with_cloud_config_binding(snapshot.binding);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_thread().await;
    assert_eq!(parent.config_snapshot().await.model_provider_id, "openai");
    let target = loaded_custom_provider_resident(&harness, &parent).await;
    let target_id = target.session.thread_id;
    assert!(target.is_running());
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));

    let guard = crate::agent::child_provider_authorization::current::prepare_loaded(
        &harness.control.runtime,
        target_id,
        parent_id,
    )
    .await
    .unwrap()
    .expect("ordinary delegated target is guarded");
    guard
        .check()
        .expect("prepared current helper stage is admitted");
    let (tx, rx) = crate::session::bounded_submission_channel(/*capacity*/ 1);
    let mut io = target.io.clone();
    io.tx_sub = tx;
    io.submit(Op::Interrupt).await.unwrap();
    let submit = io.submit_with_trace_guarded(
        Op::Interrupt,
        /*trace*/ None,
        /*parent_turn_id*/ None,
        /*root_turn_id*/ None,
        /*residency_guard*/ None,
        /*message_delivery*/ None,
        Some(&guard),
    );
    let mut submit = Box::pin(submit);
    assert!(
        matches!(futures::poll!(submit.as_mut()), std::task::Poll::Pending),
        "the actual guarded queue must wait for capacity after the initial policy check"
    );
    if change == 4 {
        let mut other = Box::pin(io.submit_with_trace_guarded(
            Op::Interrupt,
            /*trace*/ None,
            /*parent_turn_id*/ None,
            /*root_turn_id*/ None,
            /*residency_guard*/ None,
            /*message_delivery*/ None,
            Some(&guard),
        ));
        assert!(futures::poll!(other.as_mut()).is_pending());
        drop(rx);
        let (first, second) = timeout(Duration::from_secs(5), async {
            (submit.await, other.await)
        })
        .await
        .expect("receiver drop wakes both capacity waiters");
        assert!(matches!(
            first
                .expect_err("first capacity waiter must fail on receiver drop")
                .details(),
            CodexErrorDetails::InternalAgentDied
        ));
        assert!(matches!(
            second
                .expect_err("second capacity waiter must fail on receiver drop")
                .details(),
            CodexErrorDetails::InternalAgentDied
        ));
        guard
            .check()
            .expect("receiver closure does not revoke policy");
        return;
    }
    if change == 5 {
        drop(submit);
        assert!(matches!(rx.recv().await.unwrap().op, Op::Interrupt));
        assert!(matches!(
            rx.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        let next_id = timeout(
            Duration::from_secs(5),
            io.submit_with_trace_guarded(
                Op::Interrupt,
                /*trace*/ None,
                /*parent_turn_id*/ None,
                /*root_turn_id*/ None,
                /*residency_guard*/ None,
                /*message_delivery*/ None,
                Some(&guard),
            ),
        )
        .await
        .expect("later guarded operation does not hang behind a cancelled waiter")
        .expect("cancelled waiter leaves capacity available to a later operation");
        assert_eq!(rx.recv().await.unwrap().id, next_id);
        assert!(matches!(
            rx.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        return;
    }
    if change == 2 {
        let environments = parent.session.inherited_environment_snapshot().await;
        let ready = environments.primary().expect("actual invoking executor");
        let mut owner = ready.config().clone();
        owner.permission_profile =
            codex_protocol::models::PermissionProfileSnapshot::legacy(PermissionProfile::Disabled);
        let mut selection = ready.selection();
        selection.config = EnvironmentConfigState::Ready(owner);
        parent
            .update_thread_settings(ThreadSettingsOverrides {
                permission_profile: Some(PermissionProfile::Disabled),
                environments: Some(TurnEnvironmentRequests::new(
                    parent.thread_settings_snapshot().await.cwd,
                    vec![selection.into_request()],
                )),
                ..Default::default()
            })
            .await
            .unwrap();
    } else if change == 3 {
        parent
            .update_thread_settings(ThreadSettingsOverrides::default())
            .await
            .unwrap();
    } else if revoke {
        let mut value: TomlValue =
            toml::from_str(&std::fs::read_to_string(&host_file).unwrap()).unwrap();
        value["agents"]["allowed_model_providers"] =
            TomlValue::Array(vec![TomlValue::String("openai".into())]);
        std::fs::write(&host_file, toml::to_string(&value).unwrap()).unwrap();
        let next = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(harness.config.codex_home.to_path_buf())
            .harness_overrides(crate::config::ConfigOverrides {
                cwd: Some(project.path().to_path_buf()),
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        assert_eq!(
            parent
                .refresh_runtime_config(parent.config().await, next)
                .await,
            crate::ConfigRefreshOutcome::Published
        );
    }
    assert!(matches!(rx.recv().await.unwrap().op, Op::Interrupt));
    let outcome = submit.await;
    if revoke {
        let error =
            outcome.expect_err("a prepared operation cannot cross a completed policy publication");
        assert!(loaded_fixture_policy_denial(&error.to_string()), "{error}");
        assert!(
            matches!(rx.try_recv(), Err(async_channel::TryRecvError::Empty)),
            "the final admission must not enqueue the denied operation"
        );
        let mut later_stage_ran = false;
        assert!(
            guard
                .with_current(|| {
                    later_stage_ran = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(
            !later_stage_ran,
            "a later helper/retry/fallback stage cannot reuse the admitted earlier stage"
        );
    } else {
        outcome.expect("an unchanged prepared operation can enqueue after the capacity wait");
        assert!(matches!(rx.recv().await.unwrap().op, Op::Interrupt));
        guard
            .check()
            .expect("later stages remain usable under unchanged policy");
    }
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));
    target.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&target_id).await;
    parent.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&parent_id).await;
}

async fn loaded_custom_provider_resident_nested(
    harness: &AgentControlHarness,
    parent: &Arc<CodexThread>,
) -> Arc<CodexThread> {
    let runtime = &harness.control.runtime;
    let state = runtime.upgrade().unwrap();
    let membership = runtime.admit_start().unwrap();
    let slot = harness
        .control
        .reserve_v2_residency_slot(
            &state,
            &harness.config,
            &membership,
            /*protected_thread_id*/ None,
        )
        .await
        .unwrap();
    let turn = parent.session.new_default_turn().await;
    let step = StepContext::for_test(turn);
    let mut prepared = crate::agent::child_config::prepare_agent_spawn_config(
        &parent.session,
        &step,
        crate::agent::child_config::SpawnConfigOptions {
            resolved_provider: None,
            model_provider: Some("a"),
            version: crate::agent::child_config::SpawnConfigVersion::V2,
            full_history_fork: false,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .expect("real trusted custom-provider preparation")
    .config;
    assert_eq!(prepared.model_provider_id, "a");
    let mut reservation = runtime
        .registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .unwrap();
    let (source, mut metadata) = harness
        .control
        .prepare_thread_spawn(
            &mut reservation,
            &prepared,
            parent.session.thread_id,
            next_thread_spawn_depth(&parent.session_source),
            Some(
                AgentPath::from_string(format!(
                    "{}/nested_authority_target",
                    parent.session_source.get_agent_path().unwrap().as_str()
                ))
                .unwrap(),
            ),
            /*agent_role*/ None,
            /*preferred_agent_nickname*/ None,
        )
        .unwrap();
    let permit = prepared
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            runtime,
            parent.session.thread_id,
            &prepared,
            /*full_history*/ false,
            &source,
        )
        .await
        .expect("consume actual prepared custom-provider proof");
    let child = fresh_lifetime_lower_start(harness, parent, prepared, source, permit)
        .await
        .unwrap();
    metadata.agent_id = Some(child.thread_id);
    reservation.commit(metadata);
    slot.commit(child.thread_id);
    // An idle lower-start has not activated its speculative rollout yet.
    // Materialize through the actual Session before the owner route reads it.
    child.thread.ensure_rollout_materialized().await;
    child.thread.flush_rollout().await.unwrap();
    assert_eq!(
        child.thread.multi_agent_version(),
        Some(MultiAgentVersion::V2)
    );
    let stored = child
        .thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .unwrap();
    let history = stored
        .history
        .expect("actual lower-start persisted startup history");
    assert!(history.items.iter().any(|item| matches!(item,
        RolloutItem::SessionMeta(line) if line.meta.id == child.thread_id && line.meta.multi_agent_version == Some(MultiAgentVersion::V2)
    )), "actual persisted V2 metadata required by owner-loaded path");
    assert!(
        runtime
            .registry
            .agent_metadata_for_thread(child.thread_id)
            .is_some()
    );
    assert!(child.thread.session.active_turn.lock().await.is_none());
    assert_eq!(child.thread.config_snapshot().await.model_provider_id, "a");
    child.thread
}

#[tokio::test]
async fn inference_lineage_keeps_eviction_but_rejects_replaced_intermediate_registration() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let host_file = home.path().join("config.toml");
    let catalog_file = home.path().join("loaded-a.json");
    std::fs::write(
        &host_file,
        r#"
model = "gpt-5.5"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "a"]
[agents.provider_defaults.a]
model = "shared-loaded"
model_catalog_json = "loaded-a.json"
[model_providers.a]
name = "Loaded custom fixture"
base_url = "http://127.0.0.1:9/v1"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 1000
"#,
    )
    .unwrap();
    let mut info = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|info| info.slug == "gpt-5.5")
        .expect("real bundled model metadata");
    info.slug = "shared-loaded".into();
    info.multi_agent_version = Some(MultiAgentVersion::V2);
    info.supports_search_tool = false;
    info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    info.used_fallback_model_metadata = false;
    std::fs::write(
        &catalog_file,
        serde_json::to_vec(&serde_json::json!({"models":[info]})).unwrap(),
    )
    .unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::config::ConfigOverrides {
            cwd: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    config.config_layer_stack = config
        .config_layer_stack
        .with_cloud_config_binding(snapshot.binding);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_thread().await;
    assert_eq!(parent.config_snapshot().await.model_provider_id, "openai");
    let target = loaded_custom_provider_resident(&harness, &parent).await;
    let target_id = target.session.thread_id;
    assert!(target.is_running());
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));

    let owner = target;
    let owner_id = target_id;
    let descendant = loaded_custom_provider_resident_nested(&harness, &owner).await;
    let descendant_id = descendant.session.thread_id;
    let runtime = &harness.control.runtime;
    let check = || {
        runtime.with_current_native_operation(&parent.session, &descendant.session, &[], || Ok(()))
    };
    check().expect("current canonical lineage is admitted");
    let original = runtime.retained_authority(owner_id).unwrap();
    runtime.register_authority(
        crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
            &owner.session,
            &owner.session_source,
        )
        .await,
    );
    assert!(original.same_registration(&runtime.retained_authority(owner_id).unwrap()));
    check().expect("same-registration snapshot refresh preserves the ownership binding");
    let parent_settings = parent.thread_settings_snapshot().await;
    let descendant_settings = descendant.thread_settings_snapshot().await;
    assert_eq!(original.settings.model, "shared-loaded");
    assert_ne!(parent_settings.model, original.settings.model);
    assert!(
        owner
            .read_thread(
                /*include_archived*/ true, /*include_history*/ false
            )
            .await
            .unwrap()
            .model
            .is_none(),
        "this idle legacy child has no persisted TurnContext model"
    );
    let turn = owner.session.new_default_turn().await;
    owner
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                root_turn_id: None,
                turn_id: turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("owned intermediate completion".into()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;
    assert_eq!(
        harness
            .manager
            .try_evict_v2_thread(Arc::clone(&owner))
            .await
            .unwrap(),
        ThreadEvictionOutcome::Evicted
    );
    assert!(harness.manager.get_thread(owner_id).await.is_err());
    assert!(descendant.is_running());
    check().expect("completed own residency eviction preserves an active descendant");
    harness
        .control
        .ensure_child_loaded(parent_id, owner_id)
        .await
        .expect("private owned resume creates the actual replacement registration");
    let replacement = harness.manager.get_thread(owner_id).await.unwrap();
    assert_eq!(
        replacement.thread_settings_snapshot().await.model,
        original.settings.model,
        "an absent saved model must use the retained target selection, never the parent model"
    );
    assert!(!Arc::ptr_eq(&owner, &replacement));
    assert!(!original.same_registration(&runtime.retained_authority(owner_id).unwrap()));
    assert!(Arc::ptr_eq(
        &descendant,
        &harness.manager.get_thread(descendant_id).await.unwrap()
    ));
    assert!(
        descendant.is_running(),
        "the old descendant remains the exact Current allocation for this denial"
    );
    let before = harness.manager.captured_ops().len();
    let error = check()
        .expect_err("a Current descendant cannot acquire a replacement owner by stable thread ID");
    assert!(loaded_fixture_policy_denial(&error.to_string()), "{error}");
    assert_eq!(
        harness.manager.captured_ops().len(),
        before,
        "pure lineage denial performs no target submission/helper/network call"
    );
    // A real persisted conflicting model remains an error. The new None path
    // must never turn an explicit saved selection into retained authority.
    let replacement_turn = replacement.session.new_default_turn().await;
    let mut conflicting = replacement_turn.to_turn_context_item();
    conflicting.model = parent_settings.model.clone();
    assert_ne!(conflicting.model, original.settings.model);
    assert!(
        replacement
            .session
            .persist_rollout_items(&[RolloutItem::TurnContext(conflicting)])
            .await
    );
    replacement.session.flush_rollout().await.unwrap();
    assert_eq!(
        replacement
            .read_thread(
                /*include_archived*/ true, /*include_history*/ false
            )
            .await
            .unwrap()
            .model
            .as_deref(),
        Some(parent_settings.model.as_str())
    );
    replacement
        .session
        .send_event(
            replacement_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                root_turn_id: None,
                turn_id: replacement_turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("conflicting saved model fixture".into()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;
    assert_eq!(
        harness
            .manager
            .try_evict_v2_thread(Arc::clone(&replacement))
            .await
            .unwrap(),
        ThreadEvictionOutcome::Evicted
    );
    let retained_replacement = runtime.retained_authority(owner_id).unwrap();
    let before = harness.manager.captured_ops().len();
    let error = harness
        .control
        .ensure_child_loaded(parent_id, owner_id)
        .await
        .expect_err("an explicit saved model mismatch cannot be filled from retained selection");
    assert!(
        matches!(error.details(), CodexErrorDetails::InvalidRequest(message)
        if message.contains("saved model differs from its retained actual selection")),
        "{error}"
    );
    assert!(harness.manager.get_thread(owner_id).await.is_err());
    assert!(retained_replacement.same_registration(&runtime.retained_authority(owner_id).unwrap()));
    assert_eq!(harness.manager.captured_ops().len(), before);
    assert!(parent.thread_settings_snapshot().await == parent_settings);
    assert!(descendant.thread_settings_snapshot().await == descendant_settings);
    assert!(Arc::ptr_eq(
        &parent,
        &harness.manager.get_thread(parent_id).await.unwrap()
    ));
    assert!(Arc::ptr_eq(
        &descendant,
        &harness.manager.get_thread(descendant_id).await.unwrap()
    ));
    descendant.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&descendant_id).await;
    replacement.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&owner_id).await;
    parent.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&parent_id).await;
}

#[test_case::test_case(false; "actual sender shares managed channel")]
#[test_case::test_case(true; "independent actual sender binding invalidates after precheck")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loaded_operation_pins_every_actual_policy_owner_binding(independent: bool) {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let host_file = home.path().join("config.toml");
    let catalog_file = home.path().join("loaded-a.json");
    std::fs::write(
        &host_file,
        r#"
model = "gpt-5.5"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "a"]
[agents.provider_defaults.a]
model = "shared-loaded"
model_catalog_json = "loaded-a.json"
[model_providers.a]
name = "Loaded custom fixture"
base_url = "http://127.0.0.1:9/v1"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 1000
"#,
    )
    .unwrap();
    let mut info = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|info| info.slug == "gpt-5.5")
        .expect("real bundled model metadata");
    info.slug = "shared-loaded".into();
    info.multi_agent_version = Some(MultiAgentVersion::V2);
    info.supports_search_tool = false;
    info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    info.used_fallback_model_metadata = false;
    std::fs::write(
        &catalog_file,
        serde_json::to_vec(&serde_json::json!({"models":[info]})).unwrap(),
    )
    .unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::config::ConfigOverrides {
            cwd: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    config.config_layer_stack = config
        .config_layer_stack
        .with_cloud_config_binding(snapshot.binding);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_thread().await;
    assert_eq!(parent.config_snapshot().await.model_provider_id, "openai");
    let target = loaded_custom_provider_resident(&harness, &parent).await;
    let target_id = target.session.thread_id;
    assert!(target.is_running());
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));

    let caller_policy = codex_config::CloudConfigBundlePolicy::default();
    let mut caller_config = harness.config.clone();
    if independent {
        let mut snapshot = codex_config::CloudConfigBundleSnapshot {
            bundle: Ok(None),
            binding: None,
            config_server: None,
        };
        caller_policy.publish_snapshot(&mut snapshot);
        caller_config.config_layer_stack = caller_config
            .config_layer_stack
            .with_cloud_config_binding(snapshot.binding);
    }
    let caller = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions::new(caller_config),
            harness.control.clone(),
        )
        .await
        .unwrap()
        .thread;
    let caller_id = caller.session.thread_id;
    assert_ne!(caller_id, parent_id);
    let mut operation = crate::agent::child_provider_authorization::current::prepare_loaded(
        &harness.control.runtime,
        target_id,
        caller_id,
    )
    .await
    .unwrap()
    .expect("actual delegated operation with actual cross-origin sender");
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    operation.set_after_policy_validation(Arc::new(move || {
        entered_tx.send(()).unwrap();
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .expect("bounded post-validation admission barrier");
    }));
    let callbacks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let admitted = Arc::clone(&callbacks);
    let task = tokio::task::spawn_blocking(move || {
        operation.with_current(|| {
            admitted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("actual operation passes ALL policy prechecks before invalidation");
    let revision = *caller.session.child_policy_revision.lock().unwrap();
    if independent {
        let mut bundle = codex_config::CloudConfigBundle::default();
        bundle
            .config_toml
            .enterprise_managed
            .push(codex_config::CloudConfigFragment {
                id: "independent-sender-policy".into(),
                name: "sender revocation".into(),
                contents: "[agents]\nallowed_model_providers=['openai']\n".into(),
            });
        caller_policy.observe_remote_bundle(&bundle).unwrap();
        assert_ne!(
            caller
                .config()
                .await
                .config_layer_stack
                .cloud_config_binding()
                .unwrap()
                .read()
                .status,
            codex_config::CloudConfigBundleBindingStatus::Current
        );
    }
    assert_eq!(
        parent
            .config()
            .await
            .config_layer_stack
            .cloud_config_binding()
            .unwrap()
            .read()
            .status,
        codex_config::CloudConfigBundleBindingStatus::Current,
        "the target root binding stays current"
    );
    assert_eq!(
        *caller.session.child_policy_revision.lock().unwrap(),
        revision,
        "independent managed invalidation does not change the Session config publication revision"
    );
    let before = harness.manager.captured_ops().len();
    release_tx.send(()).unwrap();
    let outcome = task.await.unwrap();
    if independent {
        let error = outcome
            .expect_err("final managed admission must see the independent sender revocation");
        assert!(loaded_fixture_policy_denial(&error.to_string()), "{error}");
        assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    } else {
        outcome.expect("one shared policy channel may appear repeatedly without recursive locking");
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    }
    assert_eq!(harness.manager.captured_ops().len(), before);
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));
    caller.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&caller_id).await;
    target.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&target_id).await;
    parent.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&parent_id).await;
}
#[test_case::test_case(false, false; "actual lower HTTP configured retry unchanged positive")]
#[test_case::test_case(false, true; "actual lower HTTP configured retry publication denies second POST")]
#[test_case::test_case(true, false; "actual held auth unchanged dispatch positive")]
#[test_case::test_case(true, true; "actual held auth publication denies first POST")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn current_operation_reaches_actual_lower_auth_and_http_attempt(
    hold_auth: bool,
    revoke: bool,
) {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let host_file = home.path().join("config.toml");
    let catalog_file = home.path().join("loaded-a.json");
    std::fs::write(
        &host_file,
        r#"
model = "gpt-5.5"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "a"]
[agents.provider_defaults.a]
model = "shared-loaded"
model_catalog_json = "loaded-a.json"
[model_providers.a]
name = "Loaded custom fixture"
base_url = "http://127.0.0.1:9/v1"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 1000
"#,
    )
    .unwrap();
    let mut info = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|info| info.slug == "gpt-5.5")
        .expect("real bundled model metadata");
    info.slug = "shared-loaded".into();
    info.multi_agent_version = Some(MultiAgentVersion::V2);
    info.supports_search_tool = false;
    info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    info.used_fallback_model_metadata = false;
    std::fs::write(
        &catalog_file,
        serde_json::to_vec(&serde_json::json!({"models":[info]})).unwrap(),
    )
    .unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::config::ConfigOverrides {
            cwd: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    config.config_layer_stack = config
        .config_layer_stack
        .with_cloud_config_binding(snapshot.binding);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_thread().await;
    assert_eq!(parent.config_snapshot().await.model_provider_id, "openai");
    let target = loaded_custom_provider_resident(&harness, &parent).await;
    let target_id = target.session.thread_id;
    assert!(target.is_running());
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));

    let guard = crate::agent::child_provider_authorization::current::prepare_loaded(
        &harness.control.runtime,
        target_id,
        parent_id,
    )
    .await
    .unwrap()
    .expect("ordinary delegated target is guarded");
    guard
        .check()
        .expect("prepared current helper stage is admitted");

    crate::client::exercise_current_http_boundary(
        guard,
        hold_auth,
        async {
            if revoke {
                let mut value: TomlValue =
                    toml::from_str(&std::fs::read_to_string(&host_file).unwrap()).unwrap();
                value["agents"]["allowed_model_providers"] =
                    TomlValue::Array(vec![TomlValue::String("openai".into())]);
                std::fs::write(&host_file, toml::to_string(&value).unwrap()).unwrap();
                let next = ConfigBuilder::without_managed_config_for_tests()
                    .codex_home(harness.config.codex_home.to_path_buf())
                    .harness_overrides(crate::config::ConfigOverrides {
                        cwd: Some(project.path().to_path_buf()),
                        ..Default::default()
                    })
                    .build()
                    .await
                    .unwrap();
                assert_eq!(
                    parent
                        .refresh_runtime_config(parent.config().await, next)
                        .await,
                    crate::ConfigRefreshOutcome::Published
                );
            }
        },
        revoke,
    )
    .await;
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));
    assert_eq!(parent.config_snapshot().await.model_provider_id, "openai");
    target.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&target_id).await;
    parent.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&parent_id).await;
}

#[test_case::test_case(false, 0; "public HTTP native idle prewarm explicitly skipped while current")]
#[test_case::test_case(false, 1; "public HTTP native idle prewarm skipped after publication")]
#[test_case::test_case(true, 0; "public supported WS native idle prewarm explicitly skipped while current")]
#[test_case::test_case(true, 1; "public supported WS native idle prewarm skipped after publication")]
#[test_case::test_case(false, 2; "public HTTP native idle prewarm skipped after managed invalidation")]
#[test_case::test_case(true, 2; "public supported WS native idle prewarm skipped after managed invalidation")]
#[tokio::test]
async fn public_native_idle_prewarm_has_no_optional_helper_worker_or_transport(
    websockets: bool,
    change: u8,
) {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let host_file = home.path().join("config.toml");
    let catalog_file = home.path().join("loaded-a.json");
    std::fs::write(
        &host_file,
        r#"
model = "gpt-5.5"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "a"]
[agents.provider_defaults.a]
model = "shared-loaded"
model_catalog_json = "loaded-a.json"
[model_providers.a]
name = "Loaded custom fixture"
base_url = "http://127.0.0.1:9/v1"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 1000
"#,
    )
    .unwrap();
    let mut info = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|info| info.slug == "gpt-5.5")
        .expect("real bundled model metadata");
    info.slug = "shared-loaded".into();
    info.multi_agent_version = Some(MultiAgentVersion::V2);
    info.supports_search_tool = false;
    info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    info.used_fallback_model_metadata = false;
    std::fs::write(
        &catalog_file,
        serde_json::to_vec(&serde_json::json!({"models":[info]})).unwrap(),
    )
    .unwrap();
    let server = wiremock::MockServer::start().await;
    let mut host: TomlValue =
        toml::from_str(&std::fs::read_to_string(&host_file).unwrap()).unwrap();
    let provider = host["model_providers"]["a"].as_table_mut().unwrap();
    provider.insert(
        "base_url".into(),
        TomlValue::String(format!("{}/v1", server.uri())),
    );
    provider.insert("supports_websockets".into(), TomlValue::Boolean(websockets));
    let marker = home.path().join("idle-native-helper-marker");
    let command = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
    let args = if cfg!(windows) {
        vec![
            "/D".to_owned(),
            "/C".to_owned(),
            format!("echo invoked>\"{}\"&echo synthetic-idle", marker.display()),
        ]
    } else {
        vec![
            "-c".to_owned(),
            format!(
                "printf invoked > '{}'; printf synthetic-idle",
                marker.display()
            ),
        ]
    };
    provider.insert(
        "auth".into(),
        TomlValue::Table(toml::Table::from_iter([
            ("command".into(), TomlValue::String(command.into())),
            (
                "args".into(),
                TomlValue::Array(args.into_iter().map(TomlValue::String).collect()),
            ),
            ("timeout_ms".into(), TomlValue::Integer(5000)),
            ("refresh_interval_ms".into(), TomlValue::Integer(0)),
        ])),
    );
    std::fs::write(&host_file, toml::to_string(&host).unwrap()).unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::config::ConfigOverrides {
            cwd: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    config.config_layer_stack = config
        .config_layer_stack
        .with_cloud_config_binding(snapshot.binding);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_thread().await;
    assert_eq!(parent.config_snapshot().await.model_provider_id, "openai");
    let target = loaded_custom_provider_resident(&harness, &parent).await;
    let target_id = target.session.thread_id;
    assert!(target.is_running());
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));

    assert_eq!(
        target
            .session
            .services
            .model_client
            .responses_websocket_enabled(),
        websockets
    );
    assert!(!target.session.has_startup_prewarm_for_test().await);
    assert!(
        !marker.exists(),
        "preregistration startup must also defer native helper prewarm"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    if change == 1 {
        let mut value: TomlValue =
            toml::from_str(&std::fs::read_to_string(&host_file).unwrap()).unwrap();
        value["agents"]["allowed_model_providers"] =
            TomlValue::Array(vec![TomlValue::String("openai".into())]);
        std::fs::write(&host_file, toml::to_string(&value).unwrap()).unwrap();
        let next = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(harness.config.codex_home.to_path_buf())
            .harness_overrides(crate::config::ConfigOverrides {
                cwd: Some(project.path().to_path_buf()),
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        assert_eq!(
            parent
                .refresh_runtime_config(parent.config().await, next)
                .await,
            crate::ConfigRefreshOutcome::Published
        );
    }
    if change == 2 {
        let mut bundle = codex_config::CloudConfigBundle::default();
        bundle
            .config_toml
            .enterprise_managed
            .push(codex_config::CloudConfigFragment {
                id: "idle-native-revocation".into(),
                name: "idle native revocation".into(),
                contents: "[agents]\nallowed_model_providers=['openai']\n".into(),
            });
        policy.observe_remote_bundle(&bundle).unwrap();
        assert_ne!(
            parent
                .config()
                .await
                .config_layer_stack
                .cloud_config_binding()
                .unwrap()
                .read()
                .status,
            codex_config::CloudConfigBundleBindingStatus::Current
        );
    }
    target.prewarm().await;
    target.prewarm_with_history().await;
    assert!(
        !target.session.has_startup_prewarm_for_test().await,
        "no optional WS task may be scheduled"
    );
    assert!(
        !marker.exists(),
        "neither public call may start the concrete configured helper"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));
    assert_eq!(parent.config_snapshot().await.model_provider_id, "openai");
    target.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&target_id).await;
    parent.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&parent_id).await;
}

#[tokio::test]
async fn actual_host_owned_root_retains_optional_auth_prewarm() {
    let websockets = false;
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let host_file = home.path().join("config.toml");
    let catalog_file = home.path().join("loaded-a.json");
    std::fs::write(
        &host_file,
        r#"
model = "gpt-5.5"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "a"]
[agents.provider_defaults.a]
model = "shared-loaded"
model_catalog_json = "loaded-a.json"
[model_providers.a]
name = "Loaded custom fixture"
base_url = "http://127.0.0.1:9/v1"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 1000
"#,
    )
    .unwrap();
    let mut info = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|info| info.slug == "gpt-5.5")
        .expect("real bundled model metadata");
    info.slug = "shared-loaded".into();
    info.multi_agent_version = Some(MultiAgentVersion::V2);
    info.supports_search_tool = false;
    info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    info.used_fallback_model_metadata = false;
    std::fs::write(
        &catalog_file,
        serde_json::to_vec(&serde_json::json!({"models":[info]})).unwrap(),
    )
    .unwrap();
    let server = wiremock::MockServer::start().await;
    let mut host: TomlValue =
        toml::from_str(&std::fs::read_to_string(&host_file).unwrap()).unwrap();
    let provider = host["model_providers"]["a"].as_table_mut().unwrap();
    provider.insert(
        "base_url".into(),
        TomlValue::String(format!("{}/v1", server.uri())),
    );
    provider.insert("supports_websockets".into(), TomlValue::Boolean(websockets));
    let marker = home.path().join("idle-native-helper-marker");
    let command = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
    let args = if cfg!(windows) {
        vec![
            "/D".to_owned(),
            "/C".to_owned(),
            format!("echo invoked>\"{}\"&echo synthetic-idle", marker.display()),
        ]
    } else {
        vec![
            "-c".to_owned(),
            format!(
                "printf invoked > '{}'; printf synthetic-idle",
                marker.display()
            ),
        ]
    };
    provider.insert(
        "auth".into(),
        TomlValue::Table(toml::Table::from_iter([
            ("command".into(), TomlValue::String(command.into())),
            (
                "args".into(),
                TomlValue::Array(args.into_iter().map(TomlValue::String).collect()),
            ),
            ("timeout_ms".into(), TomlValue::Integer(5000)),
            ("refresh_interval_ms".into(), TomlValue::Integer(0)),
        ])),
    );
    std::fs::write(&host_file, toml::to_string(&host).unwrap()).unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::config::ConfigOverrides {
            cwd: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    config.config_layer_stack = config
        .config_layer_stack
        .with_cloud_config_binding(snapshot.binding);
    config.model_provider_id = "a".into();
    config.model_provider = config.model_providers["a"].clone();
    config.model = Some("shared-loaded".into());
    config.model_catalog =
        Some(serde_json::from_slice(&std::fs::read(&catalog_file).unwrap()).unwrap());
    assert_eq!(config.model.as_deref(), Some("shared-loaded"));
    let catalog = config
        .model_catalog
        .as_ref()
        .expect("actual fixture catalog");
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0].slug, "shared-loaded");
    assert!(!catalog.models[0].used_fallback_model_metadata);

    // The generic test manager intentionally has no Config/catalog input. This
    // root control must retain its real static catalog rather than discover it
    // over HTTP before auth-only prewarm. Keep the other test-manager services.
    crate::thread_manager::set_thread_manager_test_mode_for_tests(/*enabled*/ true);
    let state_db = init_state_db(&config).await;
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let models = crate::thread_manager::build_models_manager(&config, auth.clone());
    let retained_models = models.get_remote_models().await;
    assert_eq!(retained_models.len(), 1);
    assert_eq!(retained_models[0].slug, "shared-loaded");
    let store = Arc::new(LocalThreadStore::new(
        LocalThreadStoreConfig {
            codex_home: config.codex_home.to_path_buf(),
            sqlite: codex_state::SqliteConfig::new_for_testing(config.codex_home.clone()),
            default_model_provider_id: codex_model_provider_info::OPENAI_PROVIDER_ID.to_string(),
        },
        state_db.clone(),
    ));
    let manager = ThreadManager::new(
        &config,
        auth,
        models,
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        store,
        crate::thread_manager::local_agent_graph_store_from_state_db(state_db.as_ref()),
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    )
    .with_code_mode_session_provider(Arc::new(codex_code_mode::DisabledCodeModeSessionProvider));
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db,
        manager,
        control,
    };
    let (parent_id, parent) = harness.start_thread().await;
    assert_eq!(parent.config_snapshot().await.model_provider_id, "a");
    assert_eq!(parent.config_snapshot().await.model, "shared-loaded");

    assert!(parent.session_source.parent_thread_id().is_none());
    let requests_before_prewarm = server.received_requests().await.unwrap().len();
    parent.prewarm().await;
    parent.prewarm_with_history().await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual host-owned auth prewarm must still run the configured helper");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "invoked");
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests.is_empty(),
        "HTTP auth-only warmup sends no model request; before_prewarm_count={}, final_count={}, first_method_paths={:?}",
        requests_before_prewarm,
        requests.len(),
        requests
            .iter()
            .take(8)
            .map(|request| (
                request
                    .method
                    .to_string()
                    .chars()
                    .take(16)
                    .collect::<String>(),
                request.url.path().chars().take(128).collect::<String>(),
            ))
            .collect::<Vec<_>>()
    );
    parent.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&parent_id).await;
}

#[derive(Clone, Copy)]
enum GraphClosedGatePosition {
    BeforeCommit,
    AfterCommit,
}

#[derive(Debug)]
struct GraphClosedBarrier {
    child: ThreadId,
    committed: bool,
    release: tokio::sync::oneshot::Sender<()>,
}

struct LifecycleGraphGate {
    inner: Arc<dyn codex_agent_graph_store::AgentGraphStore>,
    armed: std::sync::Mutex<Option<(ThreadId, GraphClosedGatePosition)>>,
    entered: tokio::sync::mpsc::UnboundedSender<GraphClosedBarrier>,
}

impl codex_agent_graph_store::AgentGraphStore for LifecycleGraphGate {
    fn upsert_thread_spawn_edge(
        &self,
        parent: ThreadId,
        child: ThreadId,
        status: codex_agent_graph_store::ThreadSpawnEdgeStatus,
    ) -> codex_agent_graph_store::AgentGraphStoreFuture<'_, ()> {
        self.inner.upsert_thread_spawn_edge(parent, child, status)
    }

    fn set_thread_spawn_edge_status(
        &self,
        child: ThreadId,
        status: codex_agent_graph_store::ThreadSpawnEdgeStatus,
    ) -> codex_agent_graph_store::AgentGraphStoreFuture<'_, ()> {
        Box::pin(async move {
            let gate = {
                let mut armed = self.armed.lock().unwrap();
                if status == codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed
                    && armed.as_ref().is_some_and(|(id, _)| *id == child)
                {
                    armed.take().map(|(_, position)| position)
                } else {
                    None
                }
            };
            let Some(position) = gate else {
                return self.inner.set_thread_spawn_edge_status(child, status).await;
            };
            let committed = matches!(position, GraphClosedGatePosition::AfterCommit);
            if committed {
                self.inner
                    .set_thread_spawn_edge_status(child, status)
                    .await?;
            }
            let (release, released) = tokio::sync::oneshot::channel();
            self.entered
                .send(GraphClosedBarrier {
                    child,
                    committed,
                    release,
                })
                .expect("actual graph close barrier receiver");
            released.await.expect("release actual graph status write");
            if !committed {
                self.inner
                    .set_thread_spawn_edge_status(child, status)
                    .await?;
            }
            Ok(())
        })
    }

    fn list_thread_spawn_children(
        &self,
        parent: ThreadId,
        status: Option<codex_agent_graph_store::ThreadSpawnEdgeStatus>,
    ) -> codex_agent_graph_store::AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        self.inner.list_thread_spawn_children(parent, status)
    }

    fn list_thread_spawn_descendants(
        &self,
        root: ThreadId,
        status: Option<codex_agent_graph_store::ThreadSpawnEdgeStatus>,
    ) -> codex_agent_graph_store::AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        self.inner.list_thread_spawn_descendants(root, status)
    }
}

async fn graph_lifecycle_harness() -> (
    AgentControlHarness,
    Arc<LifecycleGraphGate>,
    tokio::sync::mpsc::UnboundedReceiver<GraphClosedBarrier>,
) {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.model = Some("gpt-5.6-sol".into());
    config.multi_agent_v2.max_concurrent_threads_per_session = 8;
    config.agent_max_depth = 8;
    assert!(!config.ephemeral);
    let state_db = init_state_db(&config)
        .await
        .expect("actual graph SQLite state");
    let inner = crate::thread_manager::local_agent_graph_store_from_state_db(Some(&state_db))
        .expect("actual LocalAgentGraphStore");
    let (entered, pending) = tokio::sync::mpsc::unbounded_channel();
    let graph = Arc::new(LifecycleGraphGate {
        inner,
        armed: std::sync::Mutex::new(None),
        entered,
    });
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth.clone(),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        crate::thread_manager::thread_store_from_config(&config, Some(state_db.clone())),
        Some(graph.clone()),
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let control = manager.agent_control();
    (
        AgentControlHarness {
            _home: home,
            config,
            state_db: Some(state_db),
            manager,
            control,
        },
        graph,
        pending,
    )
}

async fn graph_edge_has_status(
    graph: &LifecycleGraphGate,
    parent: ThreadId,
    child: ThreadId,
    status: codex_agent_graph_store::ThreadSpawnEdgeStatus,
) -> bool {
    graph
        .inner
        .list_thread_spawn_children(parent, Some(status))
        .await
        .unwrap()
        .contains(&child)
}

async fn graph_publish_actual_open(harness: &AgentControlHarness, child: &Arc<CodexThread>) {
    harness
        .control
        .persist_thread_spawn_edge_for_source(
            child.as_ref(),
            child.session.thread_id,
            Some(&child.session_source),
        )
        .await;
}

#[tokio::test]
async fn graph_current_registered_open_publication_remains_supported() {
    use codex_agent_graph_store::ThreadSpawnEdgeStatus;
    let (harness, graph, _pending) = graph_lifecycle_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let child = post_admission_registered_idle_child(&harness, &root, "graph_current").await;
    let child_id = child.session.thread_id;
    let epoch = harness
        .control
        .runtime
        .retained_authority(child_id)
        .unwrap();
    let revision = harness.control.runtime.teardown_revision(child_id);
    graph_publish_actual_open(&harness, &child).await;
    let is_open =
        graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Open).await;
    assert!(Arc::ptr_eq(
        &harness.manager.get_thread(child_id).await.unwrap(),
        &child
    ));
    assert!(
        harness
            .control
            .runtime
            .retained_authority(child_id)
            .unwrap()
            .same_registration(&epoch)
    );
    assert_eq!(
        harness.control.runtime.teardown_revision(child_id),
        revision
    );
    assert!(child.is_running() && root.is_running());
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
    assert!(
        is_open,
        "current exact registration must publish actual SQLite Open"
    );
}

#[tokio::test]
async fn graph_delayed_open_cannot_reopen_closed_same_registered_arc() {
    use codex_agent_graph_store::ThreadSpawnEdgeStatus;
    let (harness, graph, mut pending) = graph_lifecycle_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let child =
        post_admission_registered_idle_child(&harness, &root, "graph_closed_same_arc").await;
    let sibling =
        post_admission_registered_idle_child(&harness, &root, "graph_same_arc_sibling").await;
    let child_id = child.session.thread_id;
    graph_publish_actual_open(&harness, &sibling).await;
    graph_publish_actual_open(&harness, &child).await;
    assert!(graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Open).await);
    let epoch = harness
        .control
        .runtime
        .retained_authority(child_id)
        .unwrap();
    let revision = harness.control.runtime.teardown_revision(child_id);
    let state = harness.control.runtime.upgrade().unwrap();
    // Construct but do not poll this actual production publication until Closed committed.
    let delayed_open = graph_publish_actual_open(&harness, &child);
    *graph.armed.lock().unwrap() = Some((child_id, GraphClosedGatePosition::AfterCommit));
    let close = harness.control.close_agent(child_id);
    tokio::pin!(close);
    let barrier = tokio::select! {
        _ = &mut close => panic!("close returned before actual ClosedAfterCommit barrier"),
        entered = timeout(Duration::from_secs(10), pending.recv()) => entered.unwrap().unwrap(),
    };
    assert_eq!(barrier.child, child_id);
    assert!(barrier.committed);
    assert!(graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Closed).await);
    assert!(harness.control.runtime.teardown_revision(child_id) > revision);
    assert!(
        harness
            .control
            .runtime
            .retained_authority(child_id)
            .unwrap()
            .same_registration(&epoch)
    );
    assert!(
        child.is_running(),
        "Closed barrier precedes actual captured Session shutdown"
    );
    // Scheduling probe only: a repaired Closed writer owns map.write across storage.
    let serialized_close = state.threads.try_write().is_err();
    if serialized_close {
        barrier.release.send(()).unwrap();
        timeout(Duration::from_secs(10), &mut close)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(10), delayed_open)
            .await
            .unwrap();
    } else {
        assert!(Arc::ptr_eq(
            &harness.manager.get_thread(child_id).await.unwrap(),
            &child
        ));
        timeout(Duration::from_secs(10), delayed_open)
            .await
            .unwrap();
        barrier.release.send(()).unwrap();
        timeout(Duration::from_secs(10), &mut close)
            .await
            .unwrap()
            .unwrap();
    }
    let final_closed =
        graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Closed).await;
    let stale_open =
        graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Open).await;
    assert!(!child.is_running());
    assert!(harness.manager.get_thread(child_id).await.is_err());
    assert!(
        root.is_running() && sibling.is_running(),
        "explicit child close preserves outside root and sibling"
    );
    assert!(
        graph_edge_has_status(
            &graph,
            root_id,
            sibling.session.thread_id,
            ThreadSpawnEdgeStatus::Open
        )
        .await
    );
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
    assert!(
        final_closed && !stale_open,
        "delayed Open must not reopen actual Closed same Arc; serialized_close={serialized_close}, final_closed={final_closed}, stale_open={stale_open}"
    );
}

#[tokio::test]
async fn graph_delayed_unloaded_close_cannot_relabel_new_resume_open() {
    use codex_agent_graph_store::ThreadSpawnEdgeStatus;
    let (harness, graph, mut pending) = graph_lifecycle_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let old = post_admission_registered_idle_child(&harness, &root, "graph_unloaded_target").await;
    let sibling =
        post_admission_registered_idle_child(&harness, &root, "graph_unloaded_sibling").await;
    let child_id = old.session.thread_id;
    let source = old.session_source.clone();
    graph_publish_actual_open(&harness, &sibling).await;
    graph_publish_actual_open(&harness, &old).await;
    assert!(graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Open).await);
    let old_epoch = harness
        .control
        .runtime
        .retained_authority(child_id)
        .unwrap();
    assert_eq!(
        harness
            .manager
            .try_evict_v2_thread(old.clone())
            .await
            .unwrap(),
        ThreadEvictionOutcome::Evicted
    );
    assert!(!old.is_running());
    assert!(harness.manager.get_thread(child_id).await.is_err());
    assert!(harness.control.get_agent_metadata(child_id).is_some());
    let revision = harness.control.runtime.teardown_revision(child_id);
    let state = harness.control.runtime.upgrade().unwrap();
    *graph.armed.lock().unwrap() = Some((child_id, GraphClosedGatePosition::BeforeCommit));
    let close = harness.control.close_agent(child_id);
    tokio::pin!(close);
    let barrier = tokio::select! {
        _ = &mut close => panic!("unloaded close returned before actual ClosedBeforeCommit barrier"),
        entered = timeout(Duration::from_secs(10), pending.recv()) => entered.unwrap().unwrap(),
    };
    assert_eq!(barrier.child, child_id);
    assert!(!barrier.committed);
    assert!(harness.control.runtime.teardown_revision(child_id) > revision);
    let close_revision = harness.control.runtime.teardown_revision(child_id);
    let serialized_close = state.threads.try_write().is_err();
    // NEW operation prepared after canonical close, preserving deliberate resume-after-close.
    let resume =
        harness
            .control
            .resume_agent_from_rollout(harness.config.clone(), child_id, source);
    if serialized_close {
        barrier.release.send(()).unwrap();
        timeout(Duration::from_secs(10), &mut close)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(10), resume)
                .await
                .unwrap()
                .unwrap(),
            child_id
        );
    } else {
        assert_eq!(
            timeout(Duration::from_secs(10), resume)
                .await
                .unwrap()
                .unwrap(),
            child_id
        );
        assert!(
            graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Open).await
        );
        barrier.release.send(()).unwrap();
        timeout(Duration::from_secs(10), &mut close)
            .await
            .unwrap()
            .unwrap();
    }
    let replacement = harness.manager.get_thread(child_id).await.unwrap();
    let new_epoch = harness
        .control
        .runtime
        .retained_authority(child_id)
        .unwrap();
    assert!(!Arc::ptr_eq(&old, &replacement));
    assert!(!old_epoch.same_registration(&new_epoch));
    assert!(new_epoch.matches_session(&replacement.session));
    assert!(harness.control.runtime.teardown_revision(child_id) > close_revision);
    assert!(replacement.is_running() && root.is_running() && sibling.is_running());
    assert!(
        graph_edge_has_status(
            &graph,
            root_id,
            sibling.session.thread_id,
            ThreadSpawnEdgeStatus::Open
        )
        .await
    );
    let final_open =
        graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Open).await;
    let stale_closed =
        graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Closed).await;
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
    assert!(
        final_open && !stale_closed,
        "old unloaded Closed must preserve actual new resume Open; serialized_close={serialized_close}, final_open={final_open}, stale_closed={stale_closed}"
    );
}

// This deliberately has NO manager close lock: exact residency alone must not
// authorize a delayed Open for a canonically revoked, still loaded allocation.
#[tokio::test]
async fn graph_revoked_same_resident_cannot_publish_open_without_close_serialization() {
    use codex_agent_graph_store::ThreadSpawnEdgeStatus;
    let (harness, graph, _pending) = graph_lifecycle_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let child =
        post_admission_registered_idle_child(&harness, &root, "graph_revoked_resident").await;
    let sibling =
        post_admission_registered_idle_child(&harness, &root, "graph_revoked_sibling").await;
    let child_id = child.session.thread_id;
    graph_publish_actual_open(&harness, &child).await;
    graph_publish_actual_open(&harness, &sibling).await;
    assert!(
        harness
            .control
            .runtime
            .registry
            .graph_open_is_current(&child.session)
    );
    harness.control.runtime.registry.revoke_caller(child_id);
    assert!(
        !harness
            .control
            .runtime
            .registry
            .graph_open_is_current(&child.session)
    );
    let state = harness.control.runtime.upgrade().unwrap();
    assert!(
        state.threads.try_write().is_ok(),
        "no close map lock may hide this canonical phase check"
    );
    assert!(Arc::ptr_eq(
        &child,
        &harness.manager.get_thread(child_id).await.unwrap()
    ));
    assert!(child.is_running());
    graph
        .inner
        .set_thread_spawn_edge_status(child_id, ThreadSpawnEdgeStatus::Closed)
        .await
        .unwrap();
    assert!(graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Closed).await);
    graph_publish_actual_open(&harness, &child).await;
    let closed =
        graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Closed).await;
    let reopened =
        graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Open).await;
    assert!(root.is_running() && sibling.is_running());
    assert!(
        harness
            .control
            .runtime
            .registry
            .graph_open_is_current(&sibling.session)
    );
    assert!(
        graph_edge_has_status(
            &graph,
            root_id,
            sibling.session.thread_id,
            ThreadSpawnEdgeStatus::Open
        )
        .await
    );
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
    assert!(
        closed && !reopened,
        "canonical Revoked must independently reject Open for the SAME registered Arc"
    );
}

#[tokio::test]
async fn graph_same_registration_explicit_close_intent_survives_newer_explicit_stamp() {
    use codex_agent_graph_store::ThreadSpawnEdgeStatus;
    let (harness, graph, _pending) = graph_lifecycle_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let child = post_admission_registered_idle_child(&harness, &root, "graph_close_stamp").await;
    let child_id = child.session.thread_id;
    graph_publish_actual_open(&harness, &child).await;
    let state = harness.control.runtime.upgrade().unwrap();
    let old = state
        .fence_native_subtree_with_graph_stamp(
            &harness.control.runtime,
            child_id,
            /*descendants*/ true,
            |_| {},
        )
        .await;
    let old_revision = harness.control.runtime.teardown_revision(child_id);
    let current = state
        .fence_native_subtree_with_graph_stamp(
            &harness.control.runtime,
            child_id,
            /*descendants*/ true,
            |_| {},
        )
        .await;
    assert!(harness.control.runtime.teardown_revision(child_id) > old_revision);
    assert!(Arc::ptr_eq(
        &child,
        &harness.manager.get_thread(child_id).await.unwrap()
    ));
    assert!(
        child.is_running(),
        "same Arc/registration remains loaded across the two close generations"
    );
    state
        .persist_native_graph_closed(&harness.control.runtime, &old)
        .await
        .unwrap();
    assert!(
        graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Closed).await,
        "a newer teardown in the SAME registration cannot erase explicit Closed intent"
    );
    state
        .persist_native_graph_closed(&harness.control.runtime, &current)
        .await
        .unwrap();
    assert!(
        graph_edge_has_status(&graph, root_id, child_id, ThreadSpawnEdgeStatus::Closed).await,
        "the actual current close generation can persist Closed"
    );
    assert!(root.is_running());
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

async fn graph_explicit_close_survives_ordinary_shutdown(tree: bool) {
    use crate::agent::registry::GraphLifecycleTestPoint;
    use codex_agent_graph_store::ThreadSpawnEdgeStatus;
    let (harness, graph, _pending) = graph_lifecycle_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let child = post_admission_registered_idle_child(&harness, &root, "graph_intent_child").await;
    let sibling =
        post_admission_registered_idle_child(&harness, &root, "graph_intent_sibling").await;
    let id = child.session.thread_id;
    graph_publish_actual_open(&harness, &child).await;
    graph_publish_actual_open(&harness, &sibling).await;
    let original = harness.control.runtime.retained_authority(id).unwrap();
    let explicit_pause = harness
        .control
        .runtime
        .registry
        .pause_graph_at(GraphLifecycleTestPoint::AfterExplicitCapture);
    let control = harness.control.clone();
    let close = tokio::spawn(async move { control.close_agent(id).await });
    timeout(Duration::from_secs(10), explicit_pause.entered.notified())
        .await
        .unwrap();
    let explicit_revision = harness.control.runtime.teardown_revision(id);
    let ordinary_pause = harness
        .control
        .runtime
        .registry
        .pause_graph_at(GraphLifecycleTestPoint::AfterOrdinaryCapture);
    let control = harness.control.clone();
    let ordinary = tokio::spawn(async move {
        if tree {
            control.shutdown_agent_tree(id).await
        } else {
            control.shutdown_live_agent(id).await
        }
    });
    timeout(Duration::from_secs(10), ordinary_pause.entered.notified())
        .await
        .unwrap();
    assert!(harness.control.runtime.teardown_revision(id) > explicit_revision);
    assert!(original.same_registration(&harness.control.runtime.retained_authority(id).unwrap()));
    assert!(Arc::ptr_eq(
        &child,
        &harness.manager.get_thread(id).await.unwrap()
    ));
    assert!(child.is_running());
    // Both actual callers have fenced the SAME allocation; neither holds map.
    // The ordinary caller will never publish graph Closed on this path.
    explicit_pause.release.notify_one();
    let closed = timeout(Duration::from_secs(10), close)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(closed, AgentInfo::Loaded { .. }));
    assert!(graph_edge_has_status(&graph, root_id, id, ThreadSpawnEdgeStatus::Closed).await);
    assert!(!graph_edge_has_status(&graph, root_id, id, ThreadSpawnEdgeStatus::Open).await);
    ordinary_pause.release.notify_one();
    // The exact writer may already be stopped by explicit close. Preserve its
    // actual result; graph intent must not depend on this redundant cleanup.
    let _ordinary_result = timeout(Duration::from_secs(10), ordinary)
        .await
        .unwrap()
        .unwrap();
    assert!(root.is_running() && sibling.is_running());
    assert!(
        graph_edge_has_status(
            &graph,
            root_id,
            sibling.session.thread_id,
            ThreadSpawnEdgeStatus::Open
        )
        .await
    );
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
}
#[tokio::test]
async fn graph_explicit_closed_intent_survives_shutdown_live_agent() {
    graph_explicit_close_survives_ordinary_shutdown(/*tree*/ false).await;
}
#[tokio::test]
async fn graph_explicit_closed_intent_survives_shutdown_agent_tree() {
    graph_explicit_close_survives_ordinary_shutdown(/*tree*/ true).await;
}

#[tokio::test]
async fn graph_explicit_closed_intent_survives_newer_cancelled_explicit_close() {
    use crate::agent::registry::GraphLifecycleTestPoint;
    use codex_agent_graph_store::ThreadSpawnEdgeStatus;
    let (harness, graph, _pending) = graph_lifecycle_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let child =
        post_admission_registered_idle_child(&harness, &root, "graph_cancelled_reclose").await;
    let sibling =
        post_admission_registered_idle_child(&harness, &root, "graph_cancelled_sibling").await;
    let id = child.session.thread_id;
    graph_publish_actual_open(&harness, &child).await;
    graph_publish_actual_open(&harness, &sibling).await;
    let old_pause = harness
        .control
        .runtime
        .registry
        .pause_graph_at(GraphLifecycleTestPoint::AfterExplicitCapture);
    let control = harness.control.clone();
    let old_close = tokio::spawn(async move { control.close_agent(id).await });
    timeout(Duration::from_secs(10), old_pause.entered.notified())
        .await
        .unwrap();
    let revision = harness.control.runtime.teardown_revision(id);
    let cancelled_pause = harness
        .control
        .runtime
        .registry
        .pause_graph_at(GraphLifecycleTestPoint::BeforeExplicitCapture);
    let control = harness.control.clone();
    let cancelled_close = tokio::spawn(async move { control.close_agent(id).await });
    timeout(Duration::from_secs(10), cancelled_pause.entered.notified())
        .await
        .unwrap();
    assert!(harness.control.runtime.teardown_revision(id) > revision);
    assert!(Arc::ptr_eq(
        &child,
        &harness.manager.get_thread(id).await.unwrap()
    ));
    cancelled_close.abort();
    assert!(cancelled_close.await.unwrap_err().is_cancelled());
    old_pause.release.notify_one();
    assert!(matches!(
        timeout(Duration::from_secs(10), old_close)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        AgentInfo::Loaded { .. }
    ));
    assert!(graph_edge_has_status(&graph, root_id, id, ThreadSpawnEdgeStatus::Closed).await);
    assert!(!graph_edge_has_status(&graph, root_id, id, ThreadSpawnEdgeStatus::Open).await);
    assert!(root.is_running() && sibling.is_running());
    assert!(
        graph_edge_has_status(
            &graph,
            root_id,
            sibling.session.thread_id,
            ThreadSpawnEdgeStatus::Open
        )
        .await
    );
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

#[tokio::test]
async fn graph_explicit_stamp_transfers_to_same_registration_absence_after_shutdown() {
    use codex_agent_graph_store::ThreadSpawnEdgeStatus;
    for tree in [false, true] {
        let (harness, graph, _pending) = graph_lifecycle_harness().await;
        let (root_id, root) = harness.start_thread().await;
        let child =
            post_admission_registered_idle_child(&harness, &root, "graph_transfer_absence").await;
        let id = child.session.thread_id;
        graph_publish_actual_open(&harness, &child).await;
        let old_registration = harness.control.runtime.retained_authority(id).unwrap();
        let state = harness.control.runtime.upgrade().unwrap();
        let captured = state
            .fence_native_subtree_with_graph_stamp(
                &harness.control.runtime,
                id,
                /*descendants*/ true,
                |_| {},
            )
            .await;
        if tree {
            harness.control.shutdown_agent_tree(id).await.unwrap();
        } else {
            harness.control.shutdown_live_agent(id).await.unwrap();
        }
        assert!(harness.manager.get_thread(id).await.is_err());
        assert!(
            old_registration
                .same_registration(&harness.control.runtime.retained_authority(id).unwrap())
        );
        assert!(graph_edge_has_status(&graph, root_id, id, ThreadSpawnEdgeStatus::Open).await);
        state
            .persist_native_graph_closed(&harness.control.runtime, &captured)
            .await
            .unwrap();
        assert!(graph_edge_has_status(&graph, root_id, id, ThreadSpawnEdgeStatus::Closed).await);
        assert!(!graph_edge_has_status(&graph, root_id, id, ThreadSpawnEdgeStatus::Open).await);
        timeout(
            Duration::from_secs(10),
            harness.control.shutdown_agent_tree(root_id),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(harness.manager.list_thread_ids().await.is_empty());
    }
}

async fn graph_old_absent_close_rejects_new_registration_evicted_back_to_absence(
    missing_retained: bool,
) {
    use crate::agent::registry::GraphLifecycleTestPoint;
    use codex_agent_graph_store::ThreadSpawnEdgeStatus;
    let (mut harness, graph, _pending) = graph_lifecycle_harness().await;
    if missing_retained {
        // A real missing-ceiling resume must use the production sealed cold
        // authority path. Protect the host store before any Session starts.
        harness
            .config
            .permissions
            .set_permission_profile(PermissionProfile::read_only())
            .unwrap();
    }
    let (root_id, root) = harness.start_thread().await;
    let old = post_admission_registered_idle_child(&harness, &root, "graph_absence_aba").await;
    let sibling = post_admission_registered_idle_child(&harness, &root, "graph_aba_sibling").await;
    let id = old.session.thread_id;
    let source = old.session_source.clone();
    graph_publish_actual_open(&harness, &old).await;
    graph_publish_actual_open(&harness, &sibling).await;
    let metadata = harness.control.get_agent_metadata(id).unwrap();
    let old_registration = harness.control.runtime.retained_authority(id).unwrap();
    let root_authority = harness.control.runtime.retained_authority(root_id).unwrap();
    let sibling_authority = harness
        .control
        .runtime
        .retained_authority(sibling.session.thread_id)
        .unwrap();
    if missing_retained {
        // Persist the actual selected runtime context, as a normal completed
        // turn would, so cold admission need not infer a model from a ceiling.
        let turn = old.session.new_default_turn().await;
        assert!(
            old.session
                .persist_rollout_items(&[RolloutItem::TurnContext(turn.to_turn_context_item())])
                .await
        );
        old.session.flush_rollout().await.unwrap();
        assert_eq!(
            old.read_thread(
                /*include_archived*/ false, /*include_history*/ false
            )
            .await
            .unwrap()
            .model
            .as_deref(),
            Some("gpt-5.6-sol")
        );
    }
    assert_eq!(
        harness
            .manager
            .try_evict_v2_thread(old.clone())
            .await
            .unwrap(),
        ThreadEvictionOutcome::Evicted
    );
    let state = harness.control.runtime.upgrade().unwrap();
    let control = harness.control.clone();
    if missing_retained {
        let storage = state.native_authority_storage().unwrap();
        assert!(
            crate::agent::child_provider_authorization::durable::protected_storage(
                &storage,
                &root_authority.scope,
            )
        );
        assert!(
            crate::agent::child_provider_authorization::durable::protected_storage(
                &storage,
                &old_registration.scope,
            )
        );
        let receipt = state
            .read_native_authority_receipt(id)
            .await
            .unwrap()
            .expect("actual completed eviction must seal cold authority before fault injection");
        assert_eq!(receipt.epoch.thread_id, id);
        assert_eq!(receipt.epoch.parent_thread_id, Some(root_id));
        assert_eq!(receipt.epoch.root_thread_id, root_id);
        // Controlled missing-retained fixture state in the ACTUAL host runtime.
        // Remove only this target ceiling; preserve its generation tombstone,
        // metadata, root/sibling authority, and private provider-origin map.
        let revision = control.runtime.teardown_revision(id);
        control
            .runtime
            .registry
            .remove_retained_authority_for_graph_fixture(id);
        assert_eq!(control.runtime.teardown_revision(id), revision);
        let current_metadata = control.get_agent_metadata(id).unwrap();
        assert_eq!(current_metadata.agent_id, metadata.agent_id);
        assert_eq!(current_metadata.agent_path, metadata.agent_path);
        assert_eq!(current_metadata.agent_nickname, metadata.agent_nickname);
        assert_eq!(current_metadata.agent_role, metadata.agent_role);
        assert!(control.runtime.retained_authority(id).is_none());
        assert!(Arc::ptr_eq(
            &root_authority,
            &control.runtime.retained_authority(root_id).unwrap()
        ));
        assert!(Arc::ptr_eq(
            &sibling_authority,
            &control
                .runtime
                .retained_authority(sibling.session.thread_id)
                .unwrap()
        ));
    }
    assert!(Arc::ptr_eq(
        &control.runtime.child_provider_origins,
        &root
            .session
            .services
            .local_agent_runtime
            .child_provider_origins
    ));
    let pause = control
        .runtime
        .registry
        .pause_graph_at(GraphLifecycleTestPoint::AfterExplicitCapture);
    let close_control = control.clone();
    let old_close = tokio::spawn(async move { close_control.close_agent(id).await });
    timeout(Duration::from_secs(10), pause.entered.notified())
        .await
        .unwrap();
    assert!(harness.manager.get_thread(id).await.is_err());
    if missing_retained {
        assert!(control.runtime.retained_authority(id).is_none());
    }
    assert_eq!(
        control
            .resume_agent_from_rollout(harness.config.clone(), id, source)
            .await
            .unwrap(),
        id
    );
    let replacement = harness.manager.get_thread(id).await.unwrap();
    let new_registration = control.runtime.retained_authority(id).unwrap();
    assert!(new_registration.matches_session(&replacement.session));
    assert!(!old_registration.same_registration(&new_registration));
    assert!(!Arc::ptr_eq(&old, &replacement));
    assert!(graph_edge_has_status(&graph, root_id, id, ThreadSpawnEdgeStatus::Open).await);
    assert_eq!(
        harness
            .manager
            .try_evict_v2_thread(replacement)
            .await
            .unwrap(),
        ThreadEvictionOutcome::Evicted
    );
    assert!(harness.manager.get_thread(id).await.is_err());
    assert!(new_registration.same_registration(&control.runtime.retained_authority(id).unwrap()));
    pause.release.notify_one();
    assert!(matches!(
        timeout(Duration::from_secs(10), old_close)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        AgentInfo::Unloaded(_)
    ));
    assert!(graph_edge_has_status(&graph, root_id, id, ThreadSpawnEdgeStatus::Open).await);
    assert!(!graph_edge_has_status(&graph, root_id, id, ThreadSpawnEdgeStatus::Closed).await);
    assert!(root.is_running() && sibling.is_running());
    assert!(
        graph_edge_has_status(
            &graph,
            root_id,
            sibling.session.thread_id,
            ThreadSpawnEdgeStatus::Open
        )
        .await
    );
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
}
#[tokio::test]
async fn graph_old_retained_absent_close_rejects_new_registration_aba() {
    graph_old_absent_close_rejects_new_registration_evicted_back_to_absence(
        /*missing_retained*/ false,
    )
    .await;
}
#[tokio::test]
async fn graph_old_missing_retained_close_rejects_new_registration_aba() {
    graph_old_absent_close_rejects_new_registration_evicted_back_to_absence(
        /*missing_retained*/ true,
    )
    .await;
}

#[tokio::test]
async fn graph_loaded_missing_registration_cannot_transfer_close_to_absence() {
    use codex_agent_graph_store::ThreadSpawnEdgeStatus;
    let (harness, graph, _pending) = graph_lifecycle_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let child =
        post_admission_registered_idle_child(&harness, &root, "graph_unproven_loaded").await;
    let id = child.session.thread_id;
    graph_publish_actual_open(&harness, &child).await;
    let state = harness.control.runtime.upgrade().unwrap();
    let cold_control = LocalAgentControl::new(
        Arc::downgrade(&state),
        crate::thread_manager::default_thread_id_generator(),
        /*rollout_budget*/ None,
    );
    assert!(cold_control.runtime.retained_authority(id).is_none());
    let unproven = state
        .fence_native_subtree_with_graph_stamp(
            &cold_control.runtime,
            id,
            /*descendants*/ true,
            |_| {},
        )
        .await;
    assert!(Arc::ptr_eq(
        &child,
        &harness.manager.get_thread(id).await.unwrap()
    ));
    harness.control.shutdown_live_agent(id).await.unwrap();
    assert!(harness.manager.get_thread(id).await.is_err());
    state
        .persist_native_graph_closed(&cold_control.runtime, &unproven)
        .await
        .unwrap();
    assert!(graph_edge_has_status(&graph, root_id, id, ThreadSpawnEdgeStatus::Open).await);
    assert!(!graph_edge_has_status(&graph, root_id, id, ThreadSpawnEdgeStatus::Closed).await);
    assert!(root.is_running());
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

// A public V1 handler must retain its current-operation fence through the real
// submission channel's capacity wait, not only its earlier loaded-target check.
#[test_case::test_case(false, 0; "ordinary V1 current queue positive")]
#[test_case::test_case(false, 1; "ordinary V1 host publication while queued")]
#[test_case::test_case(false, 2; "ordinary V1 managed invalidation while queued")]
#[test_case::test_case(false, 3; "ordinary V1 executor widening while queued")]
#[test_case::test_case(false, 4; "ordinary V1 registration revoked while queued")]
#[test_case::test_case(true, 0; "selected V1 current queue positive")]
#[test_case::test_case(true, 1; "selected V1 host publication while queued")]
#[test_case::test_case(true, 2; "selected V1 managed invalidation while queued")]
#[test_case::test_case(true, 3; "selected V1 executor widening while queued")]
#[test_case::test_case(true, 4; "selected V1 registration revoked while queued")]
#[tokio::test]
async fn public_v1_send_input_rechecks_publication_after_real_queue_wait(
    selected: bool,
    change: u8,
) {
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::handlers::multi_agents::SendInputHandler;
    use crate::tools::registry::ToolExecutor;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let host_file = home.path().join("config.toml");
    std::fs::write(
        &host_file,
        r#"
model = "gpt-5.5"
model_catalog_json = "root-v1.json"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "a"]
[agents.provider_defaults.a]
model = "shared-loaded"
model_catalog_json = "selected-v1.json"
[model_providers.a]
name = "V1 queue fixture"
base_url = "http://127.0.0.1:9/v1"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 1000
"#,
    )
    .unwrap();
    let mut root_info = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|info| info.slug == "gpt-5.5")
        .unwrap();
    root_info.multi_agent_version = Some(MultiAgentVersion::V1);
    root_info.supports_search_tool = false;
    root_info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    root_info.used_fallback_model_metadata = false;
    let mut selected_info = root_info.clone();
    selected_info.slug = "shared-loaded".into();
    for (file, info) in [
        ("root-v1.json", root_info),
        ("selected-v1.json", selected_info),
    ] {
        std::fs::write(
            home.path().join(file),
            serde_json::to_vec(&serde_json::json!({"models":[info]})).unwrap(),
        )
        .unwrap();
    }
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::config::ConfigOverrides {
            cwd: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut binding = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut binding);
    config.config_layer_stack = config
        .config_layer_stack
        .with_cloud_config_binding(binding.binding);
    // Use the config-aware production manager so the root's actual model is V1.
    crate::thread_manager::set_thread_manager_test_mode_for_tests(/*enabled*/ true);
    let state_db = init_state_db(&config).await;
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth.clone(),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        Arc::new(LocalThreadStore::new(
            LocalThreadStoreConfig {
                codex_home: config.codex_home.to_path_buf(),
                sqlite: codex_state::SqliteConfig::new_for_testing(config.codex_home.clone()),
                default_model_provider_id: "openai".into(),
            },
            state_db.clone(),
        )),
        crate::thread_manager::local_agent_graph_store_from_state_db(state_db.as_ref()),
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    )
    .with_code_mode_session_provider(Arc::new(codex_code_mode::DisabledCodeModeSessionProvider));
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db,
        manager,
        control,
    };
    let (parent_id, parent) = harness.start_thread().await;
    // Idle V1 sessions resolve their actual backend on the first full turn context.
    let turn = parent.session.new_default_turn().await;
    assert_eq!(
        turn.model_info().multi_agent_version,
        Some(MultiAgentVersion::V1)
    );
    assert_eq!(turn.multi_agent_version, MultiAgentVersion::V1);
    assert_eq!(parent.multi_agent_version(), Some(MultiAgentVersion::V1));
    let step = StepContext::for_test(Arc::clone(&turn));
    let mut prepared = crate::agent::child_config::prepare_agent_spawn_config(
        &parent.session,
        &step,
        crate::agent::child_config::SpawnConfigOptions {
            resolved_provider: None,
            model_provider: selected.then_some("a"),
            version: crate::agent::child_config::SpawnConfigVersion::V1,
            full_history_fork: false,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .unwrap()
    .config;
    let mut reservation = harness
        .control
        .runtime
        .registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .unwrap();
    let (source, mut metadata) = harness
        .control
        .prepare_thread_spawn(
            &mut reservation,
            &prepared,
            parent_id,
            next_thread_spawn_depth(&parent.session_source),
            Some(AgentPath::from_string("/root/public_v1_queue_target".into()).unwrap()),
            /*agent_role*/ None,
            /*preferred_agent_nickname*/ None,
        )
        .unwrap();
    let permit = prepared
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            &harness.control.runtime,
            parent_id,
            &prepared,
            /*full_history*/ false,
            &source,
        )
        .await
        .unwrap();
    let started = fresh_lifetime_lower_start(&harness, &parent, prepared, source, permit)
        .await
        .unwrap();
    metadata.agent_id = Some(started.thread_id);
    reservation.commit(metadata);
    let target_id = started.thread_id;
    let original = started.thread;
    // Resolve the idle child through the same production full-context path.
    let target_turn = original.session.new_default_turn().await;
    assert_eq!(
        target_turn.model_info().multi_agent_version,
        Some(MultiAgentVersion::V1)
    );
    assert_eq!(target_turn.multi_agent_version, MultiAgentVersion::V1);
    assert_eq!(original.multi_agent_version(), Some(MultiAgentVersion::V1));
    drop(target_turn);
    assert_eq!(
        original.config_snapshot().await.model_provider_id,
        if selected { "a" } else { "openai" }
    );
    original.ensure_rollout_materialized().await;
    original.flush_rollout().await.unwrap();

    let (tx, rx) = crate::session::bounded_submission_channel(/*capacity*/ 1);
    let capacity_wait = tx.capacity_wait_observer_for_test();
    let mut io = original.io.clone();
    io.tx_sub = tx;
    let mut observed = CodexThread::new(
        Arc::clone(&original.session),
        io,
        crate::thread_startup_metadata::ThreadStartupMetadata::from(
            &original.startup_metadata().to_session_configured_event(),
        ),
        original.rollout_path(),
        original.session_source.clone(),
    );
    observed.residency_gate = Arc::clone(&original.residency_gate);
    let observed = Arc::new(observed);
    let state = harness.control.runtime.upgrade().unwrap();
    let replaced = state
        .threads
        .write()
        .await
        .insert(target_id, Arc::clone(&observed))
        .unwrap();
    assert!(Arc::ptr_eq(&replaced, &original));
    assert!(Arc::ptr_eq(&observed.session, &original.session));
    assert!(
        harness
            .control
            .runtime
            .retained_authority(target_id)
            .unwrap()
            .matches_session(&observed.session)
    );
    let comparison_guard = crate::agent::child_provider_authorization::current::prepare_loaded(
        &harness.control.runtime,
        target_id,
        parent_id,
    )
    .await
    .unwrap()
    .expect("actual ordinary native V1 target has an operation fence");
    comparison_guard
        .check()
        .expect("actual original authority is current");
    observed.io.submit(Op::Interrupt).await.unwrap();
    let handler = SendInputHandler;
    let invocation = ToolInvocation {
        session: Arc::clone(&parent.session), turn: Arc::clone(&turn), step_context: step,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(crate::turn_diff_tracker::TurnDiffTracker::default())),
        call_id: "public-v1-capacity-call".into(),
        tool_name: codex_tools::ToolName::namespaced(
            crate::tools::handlers::multi_agents_spec::MULTI_AGENT_V1_NAMESPACE,
            "send_input",
        ),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::json!({"target": target_id.to_string(), "message": "public V1 queued followup"}).to_string(),
        },
    };
    let mut send = Box::pin(handler.handle(invocation));
    tokio::select! {
        result = &mut send => panic!("actual V1 handler must block at the full queue; success={}", result.is_ok()),
        waited = timeout(Duration::from_secs(10), capacity_wait.notified()) => {
            waited.expect("actual V1 handler reached submission capacity wait");
        }
    }
    assert_eq!(rx.len(), 1, "receiver stays full through publication");
    match change {
        0 => {}
        1 => {
            let mut host: TomlValue =
                toml::from_str(&std::fs::read_to_string(&host_file).unwrap()).unwrap();
            host["agents"]["allowed_model_providers"] = TomlValue::Array(vec![TomlValue::String(
                if selected { "openai" } else { "a" }.into(),
            )]);
            std::fs::write(&host_file, toml::to_string(&host).unwrap()).unwrap();
            let next = ConfigBuilder::without_managed_config_for_tests()
                .codex_home(harness.config.codex_home.to_path_buf())
                .harness_overrides(crate::config::ConfigOverrides {
                    cwd: Some(project.path().to_path_buf()),
                    ..Default::default()
                })
                .build()
                .await
                .unwrap();
            let current = parent.config().await;
            assert_eq!(
                parent.refresh_runtime_config(current, next).await,
                crate::ConfigRefreshOutcome::Published
            );
        }
        2 => {
            let mut bundle = codex_config::CloudConfigBundle::default();
            bundle
                .config_toml
                .enterprise_managed
                .push(codex_config::CloudConfigFragment {
                    id: "public-v1-queue-revocation".into(),
                    name: "publish managed revocation".into(),
                    contents: "[agents]\nallowed_model_providers=['openai']\n".into(),
                });
            policy.observe_remote_bundle(&bundle).unwrap();
        }
        3 => {
            let environments = parent.session.inherited_environment_snapshot().await;
            let ready = environments.primary().unwrap();
            let mut owner = ready.config().clone();
            owner.permission_profile = codex_protocol::models::PermissionProfileSnapshot::legacy(
                PermissionProfile::Disabled,
            );
            let mut selection = ready.selection();
            selection.config = EnvironmentConfigState::Ready(owner);
            parent
                .update_thread_settings(ThreadSettingsOverrides {
                    permission_profile: Some(PermissionProfile::Disabled),
                    environments: Some(TurnEnvironmentRequests::new(
                        parent.thread_settings_snapshot().await.cwd,
                        vec![selection.into_request()],
                    )),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        4 => harness.control.runtime.registry.revoke_caller(target_id),
        _ => unreachable!(),
    }
    let publication_fence_denied = comparison_guard.check().is_err();
    assert!(matches!(rx.recv().await.unwrap().op, Op::Interrupt));
    // The controlled receiver only acknowledges actual queue admission. No Core
    // task consumes this queue, starts inference, or contacts any provider.
    let (result, accepted) = timeout(Duration::from_secs(10), async {
        tokio::select! {
            result = &mut send => (result, false),
            submission = rx.recv() => {
                let submission = submission.unwrap();
                let Op::TurnInput { request, reply, .. } = submission.op else {
                    panic!("public V1 must submit the real TurnInput operation");
                };
                assert!(matches!(request.input, codex_protocol::turn_input::TurnInput::UserInput { ref content, .. } if content == &text_input("public V1 queued followup")));
                assert!(reply.send(Ok(codex_protocol::turn_input::TurnInputSubmission::Started {
                    turn_id: "observed-public-v1-turn".into(), root_turn_id: "observed-public-v1-turn".into(),
                })).is_ok());
                (send.as_mut().await, true)
            }
        }
    }).await.expect("public V1 queue decision is bounded after capacity release");
    assert!(rx.try_recv().is_err());
    let same_allocation = Arc::ptr_eq(
        &observed,
        &harness.manager.get_thread(target_id).await.unwrap(),
    );
    let restored = state
        .threads
        .write()
        .await
        .insert(target_id, Arc::clone(&original))
        .unwrap();
    assert!(Arc::ptr_eq(&restored, &observed));
    drop(send);
    // Complete actual owned Session cleanup before asserting the expected RED.
    original.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&target_id).await;
    parent.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&parent_id).await;
    assert!(
        same_allocation,
        "only the submission queue was instrumented"
    );
    assert_eq!(
        publication_fence_denied,
        change != 0,
        "the actual independent operation fence must observe this publication"
    );
    eprintln!(
        "V1_PUBLIC_QUEUE_OBSERVED selected={selected} change={change} accepted={accepted} fence_denied={publication_fence_denied} cleanup=complete"
    );
    if change == 0 {
        assert!(accepted, "unchanged V1 must reach the target queue");
        assert!(result.is_ok(), "unchanged V1 public handler should succeed");
    } else {
        assert!(
            !accepted,
            "revoked public V1 must reject BEFORE target enqueue; selected={selected}, change={change}"
        );
        let error = result.err().expect("revoked V1 handler must deny");
        assert!(loaded_fixture_policy_denial(&error.to_string()), "{error}");
    }
}

async fn public_v1_cold_compat_resident(
    harness: &AgentControlHarness,
    parent: &Arc<CodexThread>,
    selected: bool,
) -> Arc<CodexThread> {
    let runtime = &harness.control.runtime;
    let state = runtime.upgrade().unwrap();
    let membership = runtime.admit_start().unwrap();
    let slot = harness
        .control
        .reserve_v2_residency_slot(
            &state,
            &harness.config,
            &membership,
            /*protected_thread_id*/ None,
        )
        .await
        .unwrap();
    let turn = parent.session.new_default_turn().await;
    let step = StepContext::for_test(turn);
    let mut prepared = crate::agent::child_config::prepare_agent_spawn_config(
        &parent.session,
        &step,
        crate::agent::child_config::SpawnConfigOptions {
            resolved_provider: None,
            model_provider: selected.then_some("a"),
            version: crate::agent::child_config::SpawnConfigVersion::V2,
            full_history_fork: false,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .expect("real trusted custom-provider preparation")
    .config;
    assert_eq!(
        prepared.model_provider_id,
        if selected { "a" } else { "openai" }
    );
    let mut reservation = runtime
        .registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .unwrap();
    let (source, mut metadata) = harness
        .control
        .prepare_thread_spawn(
            &mut reservation,
            &prepared,
            parent.session.thread_id,
            next_thread_spawn_depth(&parent.session_source),
            Some(AgentPath::from_string("/root/public_v1_cold_compat_target".into()).unwrap()),
            /*agent_role*/ None,
            /*preferred_agent_nickname*/ None,
        )
        .unwrap();
    let permit = prepared
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            runtime,
            parent.session.thread_id,
            &prepared,
            /*full_history*/ false,
            &source,
        )
        .await
        .expect("consume actual prepared custom-provider proof");
    let child = fresh_lifetime_lower_start(harness, parent, prepared, source, permit)
        .await
        .unwrap();
    metadata.agent_id = Some(child.thread_id);
    reservation.commit(metadata);
    slot.commit(child.thread_id);
    // An idle lower-start has not activated its speculative rollout yet.
    // Materialize through the actual Session before the owner route reads it.
    child.thread.ensure_rollout_materialized().await;
    child.thread.flush_rollout().await.unwrap();
    assert_eq!(
        child.thread.multi_agent_version(),
        Some(MultiAgentVersion::V2)
    );
    let stored = child
        .thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .unwrap();
    let history = stored
        .history
        .expect("actual lower-start persisted startup history");
    assert!(history.items.iter().any(|item| matches!(item,
        RolloutItem::SessionMeta(line) if line.meta.id == child.thread_id && line.meta.multi_agent_version == Some(MultiAgentVersion::V2)
    )), "actual persisted V2 metadata required by owner-loaded path");
    assert!(
        runtime
            .registry
            .agent_metadata_for_thread(child.thread_id)
            .is_some()
    );
    assert!(child.thread.session.active_turn.lock().await.is_none());
    assert_eq!(
        child.thread.config_snapshot().await.model_provider_id,
        if selected { "a" } else { "openai" }
    );
    child.thread
}

#[test_case::test_case(false; "ordinary V1 cold interrupt positive")]
#[test_case::test_case(true; "selected V1 cold interrupt positive")]
#[tokio::test]
async fn public_v1_send_input_reloads_before_interrupt(selected: bool) {
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::handlers::multi_agents::SendInputHandler;
    use crate::tools::registry::ToolExecutor;

    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string("event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"cold-compat\",\"output\":[]}}\n\n"))
        .mount(&server).await;
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let host_file = home.path().join("config.toml");
    std::fs::write(
        &host_file,
        format!(
            r#"
openai_base_url = "{}/v1"
model = "gpt-5.5"
model_catalog_json = "root-v1.json"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "a"]
[agents.provider_defaults.a]
model = "shared-loaded"
model_catalog_json = "selected-v1.json"
[model_providers.a]
name = "V1 queue fixture"
base_url = "{}/v1"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 1000
"#,
            server.uri(),
            server.uri()
        ),
    )
    .unwrap();
    let mut root_info = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|info| info.slug == "gpt-5.5")
        .unwrap();
    root_info.multi_agent_version = Some(MultiAgentVersion::V2);
    root_info.supports_search_tool = false;
    root_info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    root_info.used_fallback_model_metadata = false;
    let mut selected_info = root_info.clone();
    selected_info.slug = "shared-loaded".into();
    for (file, info) in [
        ("root-v1.json", root_info),
        ("selected-v1.json", selected_info),
    ] {
        std::fs::write(
            home.path().join(file),
            serde_json::to_vec(&serde_json::json!({"models":[info]})).unwrap(),
        )
        .unwrap();
    }
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::config::ConfigOverrides {
            cwd: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::Sqlite).unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut binding = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut binding);
    config.config_layer_stack = config
        .config_layer_stack
        .with_cloud_config_binding(binding.binding);
    // Use actual V2 sessions for the protected eviction/reload path; invoke the V1 tool below.
    crate::thread_manager::set_thread_manager_test_mode_for_tests(/*enabled*/ true);
    let state_db = init_state_db(&config).await;
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth.clone(),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        Arc::new(LocalThreadStore::new(
            LocalThreadStoreConfig {
                codex_home: config.codex_home.to_path_buf(),
                sqlite: codex_state::SqliteConfig::new_for_testing(config.codex_home.clone()),
                default_model_provider_id: "openai".into(),
            },
            state_db.clone(),
        )),
        crate::thread_manager::local_agent_graph_store_from_state_db(state_db.as_ref()),
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    )
    .with_code_mode_session_provider(Arc::new(codex_code_mode::DisabledCodeModeSessionProvider));
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db,
        manager,
        control,
    };
    let (parent_id, parent) = harness.start_thread().await;
    assert_eq!(parent.multi_agent_version(), Some(MultiAgentVersion::V2));
    let original = public_v1_cold_compat_resident(&harness, &parent, selected).await;
    let target_id = original.session.thread_id;
    assert_eq!(
        harness
            .manager
            .try_evict_v2_thread(Arc::clone(&original))
            .await
            .unwrap(),
        ThreadEvictionOutcome::Evicted
    );
    assert!(harness.manager.get_thread(target_id).await.is_err());
    assert!(
        harness
            .control
            .runtime
            .registry
            .evicted_environments(target_id)
            .is_some()
    );
    let before_ops = harness.manager.captured_ops().len();
    let turn = parent.session.new_default_turn().await;
    let step = StepContext::for_test(Arc::clone(&turn));
    let invocation = ToolInvocation {
        session: Arc::clone(&parent.session), turn, step_context: step,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(crate::turn_diff_tracker::TurnDiffTracker::default())),
        call_id: "public-v1-cold-interrupt".into(),
        tool_name: codex_tools::ToolName::namespaced(crate::tools::handlers::multi_agents_spec::MULTI_AGENT_V1_NAMESPACE, "send_input"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function { arguments: serde_json::json!({"target": target_id.to_string(), "message": "cold V1 interrupt compatibility", "interrupt": true}).to_string() },
    };
    let result = timeout(Duration::from_secs(10), SendInputHandler.handle(invocation))
        .await
        .expect("bounded actual cold V1 handler");
    let loaded = harness.manager.get_thread(target_id).await.ok();
    let interrupted = harness
        .manager
        .captured_ops()
        .into_iter()
        .skip(before_ops)
        .filter(|(thread_id, op)| *thread_id == target_id && matches!(op, Op::Interrupt))
        .count();
    let mut reloaded = false;
    if let Some(thread) = &loaded {
        reloaded = !Arc::ptr_eq(thread, &original);
        assert_eq!(
            thread.config_snapshot().await.model_provider_id,
            if selected { "a" } else { "openai" }
        );
        if result.is_ok() {
            wait_for_recorded_user_message(thread.as_ref(), "cold V1 interrupt compatibility")
                .await;
        }
        thread.shutdown_and_wait().await.unwrap();
        harness.manager.remove_thread(&target_id).await;
    }
    parent.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&parent_id).await;
    eprintln!(
        "V1_PUBLIC_COLD_INTERRUPT_OBSERVED selected={selected} reloaded={reloaded} interrupted={interrupted} accepted={} cleanup=complete",
        result.is_ok()
    );
    assert!(
        result.is_ok(),
        "actual cold V1 interrupt must reload before interrupt"
    );
    assert!(reloaded, "actual evicted allocation must be replaced");
    assert_eq!(
        interrupted, 1,
        "actual V1 interrupt dispatched once after reload"
    );
}
// These cases keep an ACTUAL explicit close paused after canonical revocation.
// The manager allocation and residency count must remain real throughout admission.
#[test_case::test_case(0; "same revoked allocation is busy")]
#[test_case::test_case(1; "later idle candidate supplies capacity")]
#[test_case::test_case(2; "only revoked candidate returns capacity error")]
#[tokio::test]
async fn revoked_counted_resident_never_reports_capacity_progress(case: u8) {
    let (home, mut config, _server) = residency_checkpoint_config().await;
    config.multi_agent_v2.max_concurrent_threads_per_session = if case == 1 { 3 } else { 2 };
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let first =
        post_admission_registered_idle_child(&harness, &root, "revoked_counted_first").await;
    let first_id = first.session.thread_id;
    residency_checkpoint_mark_idle(&first).await;
    let second = if case == 1 {
        let child =
            post_admission_registered_idle_child(&harness, &root, "later_eligible_resident").await;
        residency_checkpoint_mark_idle(&child).await;
        Some(child)
    } else {
        None
    };
    let state = harness.control.runtime.upgrade().unwrap();
    let membership = harness.control.runtime.admit_start().unwrap();
    let expected_count = if second.is_some() { 2 } else { 1 };
    assert_eq!(
        harness
            .control
            .runtime
            .residency
            .snapshot_for_test()
            .0
            .len(),
        expected_count
    );
    let pause = harness
        .control
        .runtime
        .registry
        .pause_graph_at(crate::agent::registry::GraphLifecycleTestPoint::BeforeExplicitCapture);
    let close = harness.control.close_agent(first_id);
    tokio::pin!(close);
    tokio::select! {
        result = &mut close => panic!("real close must reach its held revocation fence: {}", result.is_ok()),
        entered = timeout(Duration::from_secs(10), pause.entered.notified()) => entered.expect("real close reached canonical revocation"),
    }
    let preconditions = first.is_running()
        && first.multi_agent_version() == Some(MultiAgentVersion::V2)
        && first.session.active_turn.lock().await.is_none()
        && matches!(first.agent_status().await, AgentStatus::Completed(_))
        && Arc::ptr_eq(&first, &harness.manager.get_thread(first_id).await.unwrap())
        && !harness
            .control
            .runtime
            .registry
            .graph_open_is_current(&first.session)
        && harness
            .control
            .runtime
            .residency
            .snapshot_for_test()
            .0
            .len()
            == expected_count;
    assert!(
        preconditions,
        "real revoked allocation must remain idle, registered and counted"
    );
    let mut direct_busy = false;
    let mut timed_out = false;
    let mut admitted = false;
    let mut capacity_denied = false;
    let mut reserved_slot = None;
    if case == 0 {
        direct_busy = timeout(
            Duration::from_secs(5),
            state.try_evict_v2_thread(Arc::clone(&first), &membership),
        )
        .await
        .unwrap()
        .unwrap()
            == ThreadEvictionOutcome::Busy;
    } else {
        match timeout(
            Duration::from_secs(5),
            harness.control.reserve_v2_residency_slot(
                &state,
                &harness.config,
                &membership,
                Some(root_id),
            ),
        )
        .await
        {
            Ok(Ok(slot)) => {
                admitted = true;
                reserved_slot = Some(slot);
            }
            Ok(Err(error)) => {
                capacity_denied =
                    matches!(error.details(), CodexErrorDetails::AgentLimitReached { .. })
                        && error.agent_context() == Some(AgentErrorContext::ResidencyCapacity)
            }
            Err(_) => timed_out = true,
        }
    }
    let (resident_ids, pending) = harness.control.runtime.residency.snapshot_for_test();
    let first_still_exact =
        Arc::ptr_eq(&first, &harness.manager.get_thread(first_id).await.unwrap())
            && first.is_running()
            && resident_ids.contains(&first_id);
    let later_removed = if let Some(child) = &second {
        harness
            .manager
            .get_thread(child.session.thread_id)
            .await
            .is_err()
            && !resident_ids.contains(&child.session.thread_id)
    } else {
        true
    };
    drop(reserved_slot);
    pause.release.notify_one();
    timeout(Duration::from_secs(10), &mut close)
        .await
        .unwrap()
        .unwrap();
    drop(close);
    drop(membership);
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    timeout(
        Duration::from_secs(10),
        harness.control.runtime.request_shutdown().wait(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        harness.manager.list_thread_ids().await.is_empty(),
        "all real allocations cleaned up"
    );
    assert_eq!(
        harness.control.runtime.residency.snapshot_for_test(),
        (Vec::new(), 0)
    );
    eprintln!(
        "RESIDENCY_CLOSE_FENCE_OBSERVED case={case} actual_preconditions={preconditions} first_still_exact={first_still_exact} direct_busy={direct_busy} timed_out={timed_out} admitted={admitted} capacity_denied={capacity_denied} later_removed={later_removed} pending={pending} cleanup=complete"
    );
    assert!(
        first_still_exact,
        "held real close must not release its still-live residency accounting"
    );
    match case {
        0 => assert!(
            direct_busy,
            "same revoked resident must be Busy, never reported capacity progress"
        ),
        1 => {
            assert!(
                admitted && !timed_out && later_removed,
                "later eligible resident must supply capacity while actual close stays held"
            );
            assert_eq!(pending, 1);
        }
        2 => {
            assert!(
                capacity_denied && !timed_out && !admitted,
                "only revoked resident must return capacity denial while actual close stays held"
            );
            assert_eq!(pending, 0);
        }
        _ => unreachable!(),
    }
}

async fn residency_checkpoint_mark_idle(thread: &Arc<CodexThread>) {
    let turn = thread.session.new_default_turn().await;
    assert_eq!(thread.multi_agent_version(), Some(MultiAgentVersion::V2));
    thread
        .session
        .send_event(
            &turn,
            EventMsg::TurnComplete(TurnCompleteEvent {
                root_turn_id: None,
                turn_id: turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("idle registered fixture".into()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;
    assert!(thread.session.active_turn.lock().await.is_none());
    assert!(matches!(
        thread.agent_status().await,
        AgentStatus::Completed(_)
    ));
}

async fn residency_checkpoint_config() -> (TempDir, Config, wiremock::MockServer) {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(core_test_support::responses::sse_response(
            core_test_support::responses::sse_completed("fixture-complete"),
        ))
        .mount(&server)
        .await;
    let host = format!(
        "openai_base_url = {}\n",
        serde_json::to_string(&format!("{}/v1", server.uri())).unwrap()
    );
    let (home, mut config) = test_config_with_host_configuration(Vec::new(), Some(&host)).await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.model = Some("gpt-5.6-sol".into());
    config.multi_agent_v2.max_concurrent_threads_per_session = 8;
    (home, config, server)
}

struct PendingForkStandardCheckpoint {
    child: ThreadId,
    context: PersistContext,
    copied_prefix_present: bool,
    release: tokio::sync::oneshot::Sender<bool>,
}

struct ForkStandardCheckpointStore {
    inner: InMemoryThreadStore,
    armed_parent: std::sync::Mutex<Option<ThreadId>>,
    armed_child: std::sync::Mutex<Option<ThreadId>>,
    child_owner: std::sync::Mutex<Option<(ThreadId, codex_thread_store::ThreadWriterOwner)>>,
    appends: std::sync::Mutex<Vec<(ThreadId, String)>>,
    finished: std::sync::Mutex<Vec<(ThreadId, bool, bool)>>,
    actual_io_errors: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::mpsc::UnboundedSender<PendingForkStandardCheckpoint>,
    io_fault_path: std::path::PathBuf,
}

impl ThreadStore for ForkStandardCheckpointStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn begin_native_authority_epoch_owned(
        &self,
        epoch: codex_thread_store::NativeAuthorityEpoch,
        owner: codex_thread_store::ThreadWriterOwner,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        self.inner.begin_native_authority_epoch_owned(epoch, owner)
    }
    fn create_thread_owned(
        &self,
        params: codex_thread_store::CreateThreadParams,
    ) -> codex_thread_store::ThreadStoreFuture<'_, codex_thread_store::ThreadWriterOwner> {
        Box::pin(async move {
            let parent = *self.armed_parent.lock().unwrap();
            let arm = parent.is_some()
                && params.parent_thread_id == parent
                && params.forked_from_id == parent;
            if arm {
                assert_eq!(params.history_mode, ThreadHistoryMode::Legacy);
                *self.armed_parent.lock().unwrap() = None;
            }
            let child = params.thread_id;
            let owner = self.inner.create_thread_owned(params).await?;
            if arm {
                *self.armed_child.lock().unwrap() = Some(child);
                *self.child_owner.lock().unwrap() = Some((child, owner.clone()));
            }
            Ok(owner)
        })
    }
    fn resume_thread_owned(
        &self,
        params: codex_thread_store::ResumeThreadParams,
        preparation: Option<codex_thread_store::ThreadStoreResumePreparation>,
    ) -> codex_thread_store::ThreadStoreFuture<
        '_,
        (Arc<Vec<RolloutItem>>, codex_thread_store::ThreadWriterOwner),
    > {
        self.inner.resume_thread_owned(params, preparation)
    }
    fn finish_thread_owned(
        &self,
        thread: ThreadId,
        owner: codex_thread_store::ThreadWriterOwner,
        discard: bool,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let matches = self
                .child_owner
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|(child, expected)| *child == thread && expected.matches(&owner));
            self.finished
                .lock()
                .unwrap()
                .push((thread, discard, matches));
            self.inner.finish_thread_owned(thread, owner, discard).await
        })
    }
    fn append_items(
        &self,
        params: codex_thread_store::AppendThreadItemsParams,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.appends.lock().unwrap().push((
                params.thread_id,
                serde_json::to_string(&params.items).unwrap(),
            ));
            self.inner.append_items(params).await
        })
    }
    fn persist_thread(
        &self,
        thread: ThreadId,
        context: PersistContext,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let block = {
                let mut armed = self.armed_child.lock().unwrap();
                if *armed == Some(thread) && context == PersistContext::Standard {
                    armed.take();
                    true
                } else {
                    false
                }
            };
            if block {
                let prefix = self.appends.lock().unwrap().iter().any(|(child, text)| {
                    *child == thread && text.contains("actual copied checkpoint parent prefix")
                });
                let (release, released) = tokio::sync::oneshot::channel();
                self.entered
                    .send(PendingForkStandardCheckpoint {
                        child: thread,
                        context,
                        copied_prefix_present: prefix,
                        release,
                    })
                    .unwrap();
                if released
                    .await
                    .expect("release actual fork Standard checkpoint")
                {
                    // This performs a real owned-temp filesystem operation. Its parent is
                    // a regular file, so the failed Standard checkpoint has a real I/O cause.
                    let error = std::fs::File::create(&self.io_fault_path)
                        .expect_err("checkpoint target parent is a real regular file");
                    self.actual_io_errors.fetch_add(1, Ordering::SeqCst);
                    return Err(codex_thread_store::ThreadStoreError::Internal {
                        message: format!("required fork Standard checkpoint I/O: {error}"),
                    });
                }
            }
            self.inner.persist_thread(thread, context).await
        })
    }
    delegate_fresh_revocation_store! {
        fn create_thread(params: codex_thread_store::CreateThreadParams) -> ();
        fn resume_thread(params: codex_thread_store::ResumeThreadParams) -> Arc<Vec<RolloutItem>>;
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: codex_thread_store::LoadThreadHistoryParams) -> codex_thread_store::StoredThreadHistory;
        fn load_latest_model_context(params: codex_thread_store::LoadThreadHistoryParams) -> codex_thread_store::StoredModelContext;
        fn read_thread(params: codex_thread_store::ReadThreadParams) -> codex_thread_store::StoredThread;
        fn read_thread_by_rollout_path(params: codex_thread_store::ReadThreadByRolloutPathParams) -> codex_thread_store::StoredThread;
        fn list_threads(params: codex_thread_store::ListThreadsParams) -> codex_thread_store::ThreadPage;
        fn update_thread_metadata(params: codex_thread_store::UpdateThreadMetadataParams) -> Option<codex_thread_store::StoredThread>;
        fn archive_thread(params: codex_thread_store::ArchiveThreadParams) -> ();
        fn unarchive_thread(params: codex_thread_store::ArchiveThreadParams) -> codex_thread_store::StoredThread;
        fn delete_thread(params: codex_thread_store::DeleteThreadParams) -> ();
        fn flush_thread(thread_id: ThreadId) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
    }
}

#[test_case::test_case(false; "copied fork Standard checkpoint succeeds before input")]
#[test_case::test_case(true; "copied fork Standard I/O failure denies input and acknowledgement")]
#[tokio::test]
async fn copied_fork_standard_checkpoint_precedes_input_and_ack(fail: bool) {
    let (home, config, _server) = residency_checkpoint_config().await;
    let blocked_parent = home.path().join("checkpoint-parent-is-file");
    std::fs::write(&blocked_parent, b"not a directory").unwrap();
    let (entered, mut pending) = tokio::sync::mpsc::unbounded_channel();
    let store = Arc::new(ForkStandardCheckpointStore {
        inner: InMemoryThreadStore::default(),
        armed_parent: std::sync::Mutex::new(None),
        armed_child: std::sync::Mutex::new(None),
        child_owner: std::sync::Mutex::new(None),
        appends: std::sync::Mutex::new(Vec::new()),
        finished: std::sync::Mutex::new(Vec::new()),
        actual_io_errors: std::sync::atomic::AtomicUsize::new(0),
        entered,
        io_fault_path: blocked_parent.join("checkpoint"),
    });
    crate::thread_manager::set_thread_manager_test_mode_for_tests(/*enabled*/ true);
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth.clone(),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        store.clone(),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    )
    .with_code_mode_session_provider(Arc::new(codex_code_mode::DisabledCodeModeSessionProvider));
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        manager,
        control,
    };
    let (root_id, root) = harness.start_thread().await;
    assert_eq!(
        root.config_snapshot().await.history_mode,
        ThreadHistoryMode::Legacy
    );
    root.inject_response_items(vec![user_message("actual copied checkpoint parent prefix")])
        .await
        .unwrap();
    let turn = root.session.new_default_turn().await;
    root.session
        .record_conversation_items(
            &turn,
            turn.model_info(),
            &[spawn_agent_call("required-checkpoint-fork")],
        )
        .await;
    root.session
        .try_ensure_rollout_materialized(PersistContext::Standard)
        .await
        .unwrap();
    *store.armed_parent.lock().unwrap() = Some(root_id);
    let spawn = harness.control.spawn_agent_with_metadata(
        harness.config.clone(),
        text_input("initial fork input after required checkpoint"),
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: root_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
        SpawnAgentOptions {
            fork_mode: Some(SpawnAgentForkMode::FullHistory),
            fork_parent_spawn_call_id: Some("required-checkpoint-fork".into()),
            ..Default::default()
        },
    );
    tokio::pin!(spawn);
    let gate = tokio::select! {
        outcome = &mut spawn => panic!("real copied fork must reach Standard checkpoint: {}", outcome.is_ok()),
        event = timeout(Duration::from_secs(10), pending.recv()) => event.unwrap().unwrap(),
    };
    let child_id = gate.child;
    let child = harness
        .manager
        .get_thread(child_id)
        .await
        .expect("actual child registered before acknowledgement checkpoint");
    let before = child.session.clone_history().await;
    let real_preconditions = gate.context == PersistContext::Standard
        && gate.copied_prefix_present
        && child.multi_agent_version() == Some(MultiAgentVersion::V2)
        && history_contains_text(before.raw_items(), "actual copied checkpoint parent prefix")
        && !history_contains_text(
            before.raw_items(),
            "initial fork input after required checkpoint",
        )
        && harness
            .control
            .runtime
            .retained_authority(child_id)
            .unwrap()
            .matches_session(&child.session);
    assert!(
        real_preconditions,
        "actual copied-history owned child must reach required Standard checkpoint before input"
    );
    gate.release.send(fail).unwrap();
    let outcome = timeout(Duration::from_secs(10), &mut spawn).await.unwrap();
    drop(spawn);
    if outcome.is_ok() {
        wait_for_recorded_user_message(&child, "initial fork input after required checkpoint")
            .await;
    }
    if outcome.is_err() {
        timeout(Duration::from_secs(10), async {
            loop {
                if !child.is_running() && harness.manager.get_thread(child_id).await.is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual PendingSpawn stops/removes exact failed child");
    }
    let acknowledged = outcome.is_ok();
    let error_is_checkpoint = outcome.as_ref().err().is_some_and(|error| {
        error
            .to_string()
            .contains("required fork Standard checkpoint I/O")
    });
    let final_child_history = child.session.clone_history().await;
    let input_delivered = history_contains_text(
        final_child_history.raw_items(),
        "initial fork input after required checkpoint",
    );
    let prefix_preserved = history_contains_text(
        final_child_history.raw_items(),
        "actual copied checkpoint parent prefix",
    );
    let removed_before_root_cleanup = harness.manager.get_thread(child_id).await.is_err();
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    timeout(
        Duration::from_secs(10),
        harness.control.runtime.request_shutdown().wait(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
    let exact_finishes = store
        .finished
        .lock()
        .unwrap()
        .iter()
        .filter(|(id, _, _)| *id == child_id)
        .map(|(_, discard, matches)| (*discard, *matches))
        .collect::<Vec<_>>();
    let io_errors = store.actual_io_errors.load(Ordering::SeqCst);
    eprintln!(
        "FORK_STANDARD_CHECKPOINT_OBSERVED fail={fail} actual_preconditions={real_preconditions} actual_io_errors={io_errors} acknowledged={acknowledged} input_delivered={input_delivered} prefix_preserved={prefix_preserved} removed_before_root_cleanup={removed_before_root_cleanup} exact_finishes={exact_finishes:?} cleanup=complete"
    );
    assert!(
        prefix_preserved,
        "actual copied prefix remains in the retained Session history"
    );
    assert!(
        !exact_finishes.is_empty() && exact_finishes.iter().all(|(_, matched)| *matched),
        "all actual child cleanup uses its exact acquired writer identity"
    );
    if fail {
        assert_eq!(io_errors, 1);
        assert!(
            !acknowledged && error_is_checkpoint,
            "required copied-history Standard I/O failure must deny child acknowledgement"
        );
        assert!(
            !input_delivered,
            "required copied-history Standard I/O failure must deny initial input"
        );
        assert!(
            removed_before_root_cleanup,
            "PendingSpawn itself must remove the exact failed child"
        );
        assert!(
            exact_finishes.iter().any(|(discard, _)| *discard),
            "pending failure must execute its exact discard path"
        );
    } else {
        assert_eq!(io_errors, 0);
        assert!(
            acknowledged && input_delivered && !removed_before_root_cleanup,
            "successful actual copied checkpoint admits input and acknowledgement"
        );
    }
}
