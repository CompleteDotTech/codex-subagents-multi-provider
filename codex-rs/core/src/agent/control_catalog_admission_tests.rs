// Test-only RED for original-owner admission before dynamic catalog/auth work.
// Included by ROOT in agent::control::tests; the author does not edit that shared include.
mod catalog_admission_tests {
    use super::*;
    use futures::FutureExt as _;
    use std::sync::atomic::AtomicUsize;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    const TARGET: &str = "catalog_probe";
    const MODEL: &str = "catalog-owned-model";

    #[derive(Default)]
    struct CatalogStore {
        inner: InMemoryThreadStore,
        creates: AtomicUsize,
        resumes: AtomicUsize,
        legacy_creates: AtomicUsize,
    }
    impl ThreadStore for CatalogStore {
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
        ) -> codex_thread_store::ThreadStoreFuture<'_, codex_thread_store::ThreadWriterOwner>
        {
            Box::pin(async move {
                self.creates.fetch_add(1, Ordering::SeqCst);
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
                self.resumes.fetch_add(1, Ordering::SeqCst);
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
                self.legacy_creates.fetch_add(1, Ordering::SeqCst);
                self.inner.create_thread(params).await
            })
        }
    }
    struct CatalogFixture {
        harness: AgentControlHarness,
        store: Arc<CatalogStore>,
        _project: TempDir,
        server: MockServer,
        calls: std::path::PathBuf,
        version: MultiAgentVersion,
        owned: std::sync::Mutex<Vec<Arc<CodexThread>>>,
    }
    impl CatalogFixture {
        async fn new(version: MultiAgentVersion) -> Self {
            let home = TempDir::new().unwrap();
            let project = TempDir::new().unwrap();
            let server = MockServer::start().await;
            let mut model = codex_models_manager::bundled_models_response()
                .unwrap()
                .models
                .into_iter()
                .find(|model| model.slug == "gpt-5.5")
                .unwrap();
            model.multi_agent_version = Some(version);
            model.supports_search_tool = false;
            model.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
            model.used_fallback_model_metadata = false;
            std::fs::write(
                home.path().join("root.json"),
                serde_json::to_vec(&serde_json::json!({"models":[model.clone()]})).unwrap(),
            )
            .unwrap();
            model.slug = MODEL.to_owned();
            model.display_name = MODEL.to_owned();
            Mock::given(method("GET"))
                .and(path("/catalog"))
                .and(header("authorization", "Bearer catalog-test-token"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"models":[model]})),
                )
                .mount(&server)
                .await;
            let calls = home.path().join("catalog-auth.calls");
            let script = home.path().join(if cfg!(windows) {
                "catalog-auth.cmd"
            } else {
                "catalog-auth.sh"
            });
            std::fs::write(
                &script,
                if cfg!(windows) {
                    "@echo off\r\necho call>>catalog-auth.calls\r\necho catalog-test-token\r\n"
                } else {
                    "printf 'call\\n' >> catalog-auth.calls\nprintf catalog-test-token\n"
                },
            )
            .unwrap();
            let auth = if cfg!(windows) {
                serde_json::json!({"command":"cmd.exe","args":["/D","/C",script],"cwd":home.path(),"timeout_ms":5000,"refresh_interval_ms":0})
            } else {
                serde_json::json!({"command":"sh","args":[script],"cwd":home.path(),"timeout_ms":5000,"refresh_interval_ms":0})
            };
            let mut contents = format!(
                r#"
model = "gpt-5.5"
model_catalog_json = "root.json"
approval_policy = "on-request"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "catalog_probe"]
[agents.provider_defaults.catalog_probe]
model = "catalog-owned-model"
[model_providers.catalog_probe]
name = "Owned localhost catalog fixture"
base_url = "{}/v1"
model_catalog_url = "{}/catalog"
request_max_retries = 0
stream_max_retries = 0
"#,
                server.uri(),
                server.uri()
            );
            contents.push_str("\n[model_providers.catalog_probe.auth]\n");
            contents.push_str(&toml::to_string(&auth).unwrap());
            std::fs::write(home.path().join("config.toml"), contents).unwrap();
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
                .set_permission_profile(PermissionProfile::workspace_write_with(
                    &[],
                    codex_protocol::protocol::NetworkSandboxPolicy::Restricted,
                    /*exclude_tmpdir_env_var*/ true,
                    /*exclude_slash_tmp*/ true,
                ))
                .unwrap();
            config
                .features
                .enable(Feature::ApiKeyModelDiscovery)
                .unwrap();
            if version == MultiAgentVersion::V2 {
                config.features.enable(Feature::MultiAgentV2).unwrap();
            } else {
                config.features.disable(Feature::MultiAgentV2).unwrap();
            }
            for feature in [Feature::Apps, Feature::CodeModeHost, Feature::ShellSnapshot] {
                config.features.disable(feature).unwrap();
            }
            config.multi_agent_v2.max_concurrent_threads_per_session = 8;
            let store = Arc::new(CatalogStore::default());
            let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("parent-only"));
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
            Self {
                harness: AgentControlHarness {
                    _home: home,
                    config,
                    state_db: None,
                    manager,
                    control,
                },
                store,
                _project: project,
                server,
                calls,
                version,
                owned: Default::default(),
            }
        }
        fn retain(&self, thread: &Arc<CodexThread>) {
            self.owned.lock().unwrap().push(Arc::clone(thread));
        }
        fn counts(&self) -> (usize, usize, usize) {
            (
                self.store.creates.load(Ordering::SeqCst),
                self.store.resumes.load(Ordering::SeqCst),
                self.store.legacy_creates.load(Ordering::SeqCst),
            )
        }
        fn auth_calls(&self) -> usize {
            match std::fs::read_to_string(&self.calls) {
                Ok(contents) => contents.lines().count(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => panic!("read owned auth observation: {error}"),
            }
        }
        async fn gets(&self) -> usize {
            self.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| {
                    request.method.as_str() == "GET" && request.url.path() == "/catalog"
                })
                .count()
        }
        async fn prepare(&self, parent: &Arc<CodexThread>, target: bool) -> Result<Config, String> {
            let turn = parent.session.new_default_turn().await;
            std::assert_eq!(
                turn.multi_agent_version, self.version,
                "actual configured native backend"
            );
            crate::agent::child_config::prepare_agent_spawn_config(
                &parent.session,
                &StepContext::for_test(turn),
                crate::agent::child_config::SpawnConfigOptions {
                    resolved_provider: None,
                    model_provider: target.then_some(TARGET),
                    version: if self.version == MultiAgentVersion::V2 {
                        crate::agent::child_config::SpawnConfigVersion::V2
                    } else {
                        crate::agent::child_config::SpawnConfigVersion::V1
                    },
                    full_history_fork: false,
                    role_name: None,
                    model: target.then_some(MODEL),
                    reasoning_effort: None,
                },
            )
            .await
            .map(|prepared| prepared.config)
        }
        async fn start_prepared(
            &self,
            parent: &Arc<CodexThread>,
            mut config: Config,
        ) -> CodexResult<Arc<CodexThread>> {
            let control = &self.harness.control;
            let state = control.runtime.upgrade()?;
            let membership = control.runtime.admit_start()?;
            let slot = if self.version == MultiAgentVersion::V2 {
                Some(
                    control
                        .reserve_v2_residency_slot(
                            &state,
                            &config,
                            &membership,
                            /*protected_thread_id*/ Some(parent.session.thread_id),
                        )
                        .await?,
                )
            } else {
                None
            };
            let mut reservation = control.runtime.registry.reserve_spawn_slot(
                if self.version == MultiAgentVersion::V2 {
                    None
                } else {
                    config.effective_agent_max_threads(self.version)
                },
            )?;
            let role = config
                .prepared_child_authorization
                .as_ref()
                .unwrap()
                .role_name();
            let (source, mut metadata) = control.prepare_thread_spawn(
                &mut reservation,
                &config,
                parent.session.thread_id,
                next_thread_spawn_depth(&parent.session_source),
                /*agent_path*/ None,
                role,
                /*preferred_agent_nickname*/ None,
            )?;
            let permit = config
                .prepared_child_authorization
                .take()
                .unwrap()
                .consume(
                    &control.runtime,
                    parent.session.thread_id,
                    &config,
                    /*full_history*/ false,
                    &source,
                )
                .await?;
            let child =
                fresh_lifetime_lower_start(&self.harness, parent, config, source, permit).await?;
            self.retain(&child.thread);
            let pending = super::super::spawn_guard::PendingSpawn::new(
                Arc::clone(&state),
                Arc::clone(&child.thread),
                membership,
            );
            let authority = control.runtime.retained_authority(child.thread_id).unwrap();
            assert!(authority.matches_session(&child.thread.session));
            metadata.agent_id = Some(child.thread_id);
            let threads = state.threads.write().await;
            assert!(
                threads
                    .get(&child.thread_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &child.thread))
            );
            reservation.commit_if_current(metadata, &authority)?;
            if let Some(slot) = slot {
                slot.commit(child.thread_id);
            }
            let _membership = pending.disarm();
            drop(threads);
            Ok(child.thread)
        }
        fn make_catalog_cold(&self) {
            // Only this fresh TempDir's partitioned catalog cache exists here.
            // The root and nested caller use the configured static root catalog.
            let cache = self.harness.config.codex_home.join("models_cache.d");
            let entries = std::fs::read_dir(&cache)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            std::assert_eq!(entries.len(), 1, "one exact owned target catalog partition");
            let entry = &entries[0];
            assert!(entry.file_type().unwrap().is_file());
            std::assert_eq!(entry.path().extension().unwrap(), "json");
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap();
            assert!(value.get("identity").is_some());
            std::fs::remove_file(entry.path()).unwrap();
            std::assert_eq!(std::fs::read_dir(cache).unwrap().count(), 0);
        }
        async fn cleanup(&self) -> Result<(), String> {
            let threads = self.owned.lock().unwrap().clone();
            let cleanup = async {
                let mut errors = Vec::new();
                let state = self.harness.control.runtime.upgrade().unwrap();
                for thread in threads.iter().rev() {
                    if let Err(error) = thread.shutdown_and_wait().await {
                        errors.push(error.to_string());
                    }
                    state
                        .remove_thread_if_matches(&thread.session.thread_id, thread, || {})
                        .await;
                }
                if !self.harness.manager.list_thread_ids().await.is_empty() {
                    errors.push("owned manager still has loaded threads".into());
                }
                if errors.is_empty() {
                    Ok(())
                } else {
                    Err(format!("{errors:?}"))
                }
            };
            timeout(Duration::from_secs(10), cleanup)
                .await
                .map_err(|_| "owned catalog cleanup deadline; ownership unresolved".to_owned())?
        }
    }

    struct Observed {
        denied: bool,
        error_is_policy: bool,
        auth_delta: usize,
        get_delta: usize,
        store_delta: (usize, usize, usize),
        registered: bool,
        prewarmed: usize,
        history_preserved: bool,
        publication_observed: bool,
        profile_changed: bool,
        file_policy_empty: bool,
        root_removed: bool,
        caller_unchanged: bool,
        owned_gate: bool,
        revision_before: u64,
        revision_after: u64,
    }

    #[test_case::test_case(MultiAgentVersion::V1, false, 0; "v1 fresh unchanged catalog admission")]
    #[test_case::test_case(MultiAgentVersion::V1, false, 1; "v1 fresh root permission before catalog")]
    #[test_case::test_case(MultiAgentVersion::V1, false, 2; "v1 fresh root file policy before catalog")]
    #[test_case::test_case(MultiAgentVersion::V1, false, 3; "v1 fresh removed root before catalog")]
    #[test_case::test_case(MultiAgentVersion::V2, false, 0; "v2 fresh unchanged catalog admission")]
    #[test_case::test_case(MultiAgentVersion::V2, false, 1; "v2 fresh root permission before catalog")]
    #[test_case::test_case(MultiAgentVersion::V2, false, 2; "v2 fresh root file policy before catalog")]
    #[test_case::test_case(MultiAgentVersion::V2, false, 3; "v2 fresh removed root before catalog")]
    #[test_case::test_case(MultiAgentVersion::V2, true, 0; "v2 private resume unchanged cold catalog")]
    #[test_case::test_case(MultiAgentVersion::V2, true, 1; "v2 private resume root permission before catalog")]
    #[test_case::test_case(MultiAgentVersion::V2, true, 2; "v2 private resume root file policy before catalog")]
    #[test_case::test_case(MultiAgentVersion::V2, true, 3; "v2 private resume removed root before catalog")]
    #[tokio::test]
    async fn original_root_admission_precedes_dynamic_catalog_auth_and_get(
        version: MultiAgentVersion,
        resume: bool,
        change: u8,
    ) {
        let fixture = CatalogFixture::new(version).await;
        let outcome = std::panic::AssertUnwindSafe(async {
            let harness = &fixture.harness;
            let (root_id, root) = harness.start_thread().await;
            fixture.retain(&root);
            assert!(root.config().await.model_catalog.is_some(), "host root is genuinely static");
            let host_file = harness.config.codex_home.join("config.toml");
            assert!(!root.config().await.permissions.file_system_sandbox_policy()
                .can_write_local_path_with_cwd(&host_file, fixture._project.path()),
                "real host policy is outside caller-writable roots and tmp allowances");
            let caller = fixture.start_prepared(&root,
                fixture.prepare(&root, /*target*/ false).await.unwrap()).await.unwrap();
            std::assert_eq!(fixture.gets().await, 0);
            std::assert_eq!(fixture.auth_calls(), 0);
            let target = if resume {
                let target = fixture.start_prepared(&caller,
                    fixture.prepare(&caller, /*target*/ true).await.unwrap()).await.unwrap();
                std::assert_eq!(fixture.gets().await, 1, "supported target catalog really fetched");
                assert!(fixture.auth_calls() > 0, "actual configured auth helper really executed");
                assert!(target.config().await.model_catalog.is_none(), "dynamic target is not static");
                let context = target.session.new_default_turn().await.to_turn_context_item();
                assert!(target.session.persist_rollout_items(&[RolloutItem::TurnContext(context)]).await);
                target.session.flush_rollout().await.unwrap();
                std::assert_eq!(harness.manager.try_evict_v2_thread(Arc::clone(&target)).await.unwrap(),
                    ThreadEvictionOutcome::Evicted, "genuine completed owned eviction before private resume");
                assert!(!target.is_running());
                assert!(harness.manager.get_thread(target.session.thread_id).await.is_err());
                fixture.make_catalog_cold();
                Some(target)
            } else {
                assert!(!harness.config.codex_home.join("models_cache.d").exists(),
                    "fresh target catalog has no fixture cache");
                None
            };
            let caller_id = caller.session.thread_id;
            let epoch = harness.control.runtime.retained_authority(caller_id).unwrap();
            let caller_before = crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
                &caller.session, &caller.session_source).await;
            let revision_before = *root.session.child_policy_revision.lock().unwrap();
            let profile_before = root.thread_settings_snapshot().await.permission_profile;
            assert!(root.config().await.config_layer_stack.cloud_config_binding().is_none());
            assert!(caller.config().await.config_layer_stack.cloud_config_binding().is_none());
            let target_history_before = if let Some(target) = &target {
                Some(serde_json::to_value(fixture.store.inner.load_history(
                    codex_thread_store::LoadThreadHistoryParams {
                        thread_id:target.session.thread_id, include_archived:true,
                    }).await.unwrap().items).unwrap())
            } else {None};
            let before_counts = fixture.counts();
            let before_auth = fixture.auth_calls();
            let before_gets = fixture.gets().await;
            let before_ids = harness.manager.list_thread_ids().await;
            let mut gate = crate::agent::child_config::catalog_preparation_gate::arm(caller_id, TARGET);
            let operation = async {
                if let Some(target) = &target {
                    harness.control.ensure_child_loaded(caller_id, target.session.thread_id).await?;
                    let loaded = harness.manager.get_thread(target.session.thread_id).await?;
                    fixture.retain(&loaded);
                    Ok(loaded)
                } else {
                    let config = fixture.prepare(&caller, /*target*/ true).await
                        .map_err(CodexErr::InvalidRequest)?;
                    fixture.start_prepared(&caller, config).await
                }
            };
            tokio::pin!(operation);
            let gate_observation = tokio::select! {
                result = &mut operation => panic!("real cold preparation must reach its owned continuation: {}", result.is_ok()),
                entered = timeout(Duration::from_secs(10), &mut gate.entered) => entered.unwrap().unwrap(),
            };
            let owned_gate = gate_observation == (caller_id, TARGET.to_owned());
            assert!(owned_gate, "actual selected caller/provider reached the continuation");
            std::assert_eq!(fixture.counts(), before_counts, "preparation gate precedes store effects");
            std::assert_eq!(fixture.auth_calls(), before_auth, "no target auth before controlled continuation");
            std::assert_eq!(fixture.gets().await, before_gets, "no target GET before controlled continuation");
            if change == 1 || change == 2 {
                publish_native_startup_policy_change(&root, change).await;
            } else if change == 3 {
                let removed = harness.manager.remove_thread_for_client(&root_id).await.unwrap().unwrap();
                assert!(Arc::ptr_eq(&removed, &root));
            }
            let revision_after = *root.session.child_policy_revision.lock().unwrap();
            let publication_observed = revision_before != revision_after;
            let profile_changed = profile_before != root.thread_settings_snapshot().await.permission_profile;
            let file_policy_empty = root.config().await.config_layer_stack.effective_config()["agents"]
                ["allowed_model_providers"].as_array().is_some_and(|list| list.is_empty());
            let root_removed = harness.manager.get_thread(root_id).await.is_err();
            std::assert_eq!(publication_observed, change == 1 || change == 2);
            std::assert_eq!(profile_changed, change == 1);
            std::assert_eq!(file_policy_empty, change == 2);
            std::assert_eq!(root_removed, change == 3);
            let caller_after = crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
                &caller.session, &caller.session_source).await;
            let caller_unchanged = caller.is_running()
                && *caller.session.fresh_child_admission_live.lock().unwrap()
                && Arc::ptr_eq(&caller, &harness.manager.get_thread(caller_id).await.unwrap())
                && Arc::ptr_eq(&epoch, &harness.control.runtime.retained_authority(caller_id).unwrap())
                && caller_before.same_registration(&caller_after)
                && caller_before.config.as_ref() == caller_after.config.as_ref()
                && caller_before.settings == caller_after.settings
                && caller_before.scope == caller_after.scope
                && harness.control.runtime.with_resume_caller(&epoch, /*live*/ true, || Ok(())).is_ok();
            assert!(caller_unchanged, "only original root authority changed during actual preparation await");
            gate.release.take().unwrap().send(()).unwrap();
            let result = timeout(Duration::from_secs(10), &mut operation).await
                .expect("preparation timeout is UNQUALIFIED, never causal denial");
            let denied = result.is_err();
            let error_is_policy = result.as_ref().err().is_some_and(|error|
                error.to_string().contains("child_provider_policy_conflict"));
            let registered = result.as_ref().ok().is_some_and(|child|
                child.session.thread_id != caller_id && child.is_running());
            let after_counts = fixture.counts();
            let store_delta = (after_counts.0-before_counts.0, after_counts.1-before_counts.1,
                after_counts.2-before_counts.2);
            let mut prewarmed = 0;
            for id in harness.manager.list_thread_ids().await {
                if !before_ids.contains(&id) {
                    if let Ok(child) = harness.manager.get_thread(id).await {
                        fixture.retain(&child);
                        prewarmed += usize::from(child.session.has_startup_prewarm_for_test().await);
                    }
                }
            }
            let history_preserved = if let Some(target) = &target {
                let after = serde_json::to_value(fixture.store.inner.load_history(
                    codex_thread_store::LoadThreadHistoryParams {
                        thread_id:target.session.thread_id, include_archived:true,
                    }).await.unwrap().items).unwrap();
                target_history_before.as_ref() == Some(&after)
            } else { store_delta == (0,0,0) };
            Observed {denied, error_is_policy, auth_delta:fixture.auth_calls()-before_auth,
                get_delta:fixture.gets().await-before_gets, store_delta, registered, prewarmed,
                history_preserved, publication_observed, profile_changed, file_policy_empty,
                root_removed, caller_unchanged, owned_gate, revision_before, revision_after}
        }).catch_unwind().await;
        let cleanup = fixture.cleanup().await;
        if let Err(error) = &cleanup {
            eprintln!("CATALOG_PREPARATION_CLEANUP_UNQUALIFIED: {error}");
        }
        let observed = match outcome {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(observed) => observed,
        };
        cleanup.expect("actual owned cleanup must complete before causal assertions");
        eprintln!(
            "CATALOG_PREPARATION_OBSERVED version={version:?} resume={resume} change={change} revision_before={} revision_after={} publication_observed={} profile_changed={} file_policy_empty={} root_removed={} caller_unchanged={} owned_gate={} denied={} error_is_policy={} auth_delta={} get_delta={} create_delta={} resume_delta={} legacy_create_delta={} registered={} prewarmed={} history_preserved={} cleanup=complete",
            observed.revision_before,
            observed.revision_after,
            observed.publication_observed,
            observed.profile_changed,
            observed.file_policy_empty,
            observed.root_removed,
            observed.caller_unchanged,
            observed.owned_gate,
            observed.denied,
            observed.error_is_policy,
            observed.auth_delta,
            observed.get_delta,
            observed.store_delta.0,
            observed.store_delta.1,
            observed.store_delta.2,
            observed.registered,
            observed.prewarmed,
            observed.history_preserved
        );
        if change == 0 {
            assert!(
                !observed.denied && observed.registered,
                "unchanged native preparation and startup succeed"
            );
            assert!(
                observed.auth_delta > 0,
                "unchanged cold catalog uses actual configured helper"
            );
            std::assert_eq!(
                observed.get_delta, 1,
                "unchanged cold catalog dispatches its real localhost GET"
            );
            std::assert_eq!(
                observed.store_delta,
                if resume { (0, 1, 0) } else { (1, 0, 0) }
            );
        } else {
            assert!(
                observed.denied && observed.error_is_policy,
                "completed root change denies native preparation specifically"
            );
            assert!(!observed.registered);
            std::assert_eq!(observed.store_delta, (0, 0, 0));
            std::assert_eq!(observed.prewarmed, 0);
            assert!(observed.history_preserved);
            std::assert_eq!(
                observed.auth_delta,
                0,
                "original root admission must precede target catalog credential work"
            );
            std::assert_eq!(
                observed.get_delta,
                0,
                "original root admission must precede target catalog GET dispatch"
            );
        }
    }
}
