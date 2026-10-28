use super::*;
use crate::AbsolutePathBufGuard;
use crate::ConfigLayerSource;
use crate::ConfigRequirementsToml;
use crate::FilesystemDenyReadPattern;
use crate::SandboxModeRequirement;
use crate::compose_requirements;
use crate::compose_requirements_for_hostname;
use crate::config_requirements::FilesystemRequirementsToml;
use crate::config_requirements::PermissionsRequirementsToml;
use crate::config_toml::ConfigToml;
use crate::types::SandboxWorkspaceWrite;
use codex_protocol::protocol::AskForApproval;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::tempdir;

#[test]
fn legacy_speed_denial_is_normalized_without_changing_other_fragments() {
    for (id, contents, expected_ultrafast) in [
        (
            "rbac-fast-mode",
            "[features]\nfast_mode = false",
            Some(false),
        ),
        ("rbac-fast-mode", "[features]\nfast_mode = true", None),
        ("admin-layer", "[features]\nfast_mode = false", None),
    ] {
        let requirements = compose_requirements(
            CloudRequirementsTomlBundle {
                enterprise_managed: vec![CloudRequirementsFragment {
                    id: id.to_string(),
                    name: id.to_string(),
                    contents: contents.to_string(),
                }],
            }
            .into_layers(),
        )
        .expect("valid cloud requirements")
        .expect("requirements layer")
        .into_toml();
        assert_eq!(
            requirements
                .feature_requirements
                .expect("feature requirements")
                .entries
                .get("ultrafast_mode")
                .copied(),
            expected_ultrafast,
            "fragment={id}",
        );
    }
}

#[test]
fn enterprise_speed_override_still_takes_precedence_over_legacy_baseline() {
    let requirements = compose_requirements(
        CloudRequirementsTomlBundle {
            enterprise_managed: vec![
                CloudRequirementsFragment {
                    id: "admin-layer".to_string(),
                    name: "Admin".to_string(),
                    contents: "[features]\nultrafast_mode = true".to_string(),
                },
                CloudRequirementsFragment {
                    id: "rbac-fast-mode".to_string(),
                    name: "Legacy RBAC".to_string(),
                    contents: "[features]\nfast_mode = false".to_string(),
                },
            ],
        }
        .into_layers(),
    )
    .expect("valid cloud requirements")
    .expect("requirements layers")
    .into_toml();
    assert_eq!(
        requirements
            .feature_requirements
            .expect("feature requirements")
            .entries,
        std::collections::BTreeMap::from([
            ("fast_mode".to_string(), false),
            ("ultrafast_mode".to_string(), true),
        ]),
    );
}

#[tokio::test]
async fn shared_future_runs_once() {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&counter);
    let loader = CloudConfigBundleLoader::new(async move {
        counter_clone.fetch_add(1, Ordering::SeqCst);
        Ok(Some(CloudConfigBundle::default()))
    });
    let cloned_loader = loader.clone();

    let (first, second) = tokio::join!(loader.get(), cloned_loader.get());
    assert_eq!(first, second);
    assert_eq!(loader.get().await, first);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn getter_returns_latest_result_across_clones() {
    let initial_error = CloudConfigBundleLoadError::new(
        CloudConfigBundleLoadErrorCode::RequestFailed,
        /*status_code*/ None,
        "initial load failed",
    );
    let latest = Arc::new(RwLock::new(Err(initial_error.clone())));
    let getter_latest = Arc::clone(&latest);
    let loader = CloudConfigBundleLoader::from_getter(move || {
        let latest = Arc::clone(&getter_latest);
        async move { latest.read().expect("bundle state lock").clone() }
    });
    let cloned_loader = loader.clone();

    assert_eq!(loader.get().await, Err(initial_error));

    let bundle = CloudConfigBundle {
        config_toml: CloudConfigTomlBundle {
            enterprise_managed: vec![CloudConfigFragment {
                id: "managed".to_string(),
                name: "Managed".to_string(),
                contents: "model = \"managed\"".to_string(),
            }],
        },
        ..Default::default()
    };
    *latest.write().expect("bundle state lock") = Ok(Some(bundle.clone()));

    assert_eq!(cloned_loader.get().await, Ok(Some(bundle)));
    assert_eq!(CloudConfigBundleLoader::default().get().await, Ok(None));

    *latest.write().expect("bundle state lock") = Ok(None);

    assert_eq!(loader.get().await, Ok(None));
}

#[test]
fn bundle_layers_preserve_enterprise_managed_bucket_order() {
    let tempdir = tempdir().expect("tempdir");
    let base_dir = AbsolutePathBuf::from_absolute_path(tempdir.path()).expect("absolute path");
    let layers = CloudConfigBundleLayers::from_bundle(
        CloudConfigBundle {
            config_toml: CloudConfigTomlBundle {
                enterprise_managed: vec![
                    CloudConfigFragment {
                        id: "cfg_high".to_string(),
                        name: "High config".to_string(),
                        contents: "model = \"high\"".to_string(),
                    },
                    CloudConfigFragment {
                        id: "cfg_low".to_string(),
                        name: "Low config".to_string(),
                        contents: "model = \"low\"".to_string(),
                    },
                ],
            },
            requirements_toml: CloudRequirementsTomlBundle {
                enterprise_managed: vec![
                    CloudRequirementsFragment {
                        id: "req_high".to_string(),
                        name: "High requirements".to_string(),
                        contents: "allowed_approval_policies = [\"on-request\"]".to_string(),
                    },
                    CloudRequirementsFragment {
                        id: "req_low".to_string(),
                        name: "Low requirements".to_string(),
                        contents: "allowed_approval_policies = [\"never\"]".to_string(),
                    },
                ],
            },
        },
        &base_dir,
    )
    .expect("bundle should be converted into layers");

    assert_eq!(
        layers
            .enterprise_managed_config
            .iter()
            .map(|layer| layer.name.clone())
            .collect::<Vec<_>>(),
        vec![
            ConfigLayerSource::EnterpriseManaged {
                id: "cfg_low".to_string(),
                name: "Low config".to_string(),
            },
            ConfigLayerSource::EnterpriseManaged {
                id: "cfg_high".to_string(),
                name: "High config".to_string(),
            },
        ]
    );
    assert_eq!(
        compose_requirements(layers.enterprise_managed_requirements)
            .expect("requirements should compose")
            .expect("requirements should be present")
            .into_toml(),
        ConfigRequirementsToml {
            allowed_approval_policies: Some(vec![AskForApproval::OnRequest]),
            ..Default::default()
        }
    );
}

#[test]
fn bundle_layers_can_strict_validate_enterprise_managed_config() {
    let tempdir = tempdir().expect("tempdir");
    let base_dir = AbsolutePathBuf::from_absolute_path(tempdir.path()).expect("absolute path");
    let err = CloudConfigBundleLayers::from_bundle_strict_config(
        CloudConfigBundle {
            config_toml: CloudConfigTomlBundle {
                enterprise_managed: vec![CloudConfigFragment {
                    id: "cfg".to_string(),
                    name: "Cloud config".to_string(),
                    contents: "unknown_key = true".to_string(),
                }],
            },
            requirements_toml: CloudRequirementsTomlBundle {
                enterprise_managed: Vec::new(),
            },
        },
        &base_dir,
    )
    .expect_err("strict config should reject unknown fields");

    assert_eq!(
        err,
        CloudConfigLayerError::Invalid {
            fragment: crate::CloudConfigFragmentSource {
                id: "cfg".to_string(),
                name: "Cloud config".to_string(),
            },
            message: "unknown configuration field `unknown_key`".to_string(),
        }
    );
}

#[test]
fn bundle_layers_resolve_paths_and_requirements_for_the_execution_host() {
    let temp_dir = tempdir().expect("temporary directories");
    let executor_home = temp_dir.path().join("executor-home");
    let executor_codex_home = AbsolutePathBuf::from_absolute_path(executor_home.join(".codex"))
        .expect("absolute executor Codex home");
    let bundle = CloudConfigBundle {
        config_toml: CloudConfigTomlBundle {
            enterprise_managed: vec![CloudConfigFragment {
                id: "config".to_string(),
                name: "Executor config".to_string(),
                contents: r#"
[sandbox_workspace_write]
writable_roots = ["~/cloud-root", "./relative-root"]
"#
                .to_string(),
            }],
        },
        requirements_toml: CloudRequirementsTomlBundle {
            enterprise_managed: vec![CloudRequirementsFragment {
                id: "requirements".to_string(),
                name: "Executor requirements".to_string(),
                contents: r#"
[permissions.filesystem]
deny_read = ["~/private"]

[[remote_sandbox_config]]
hostname_patterns = ["executor-*"]
allowed_sandbox_modes = ["read-only"]
"#
                .to_string(),
            }],
        },
    };

    let (config, requirements) = AbsolutePathBufGuard::with_home_directory(&executor_home, || {
        let layers =
            CloudConfigBundleLayers::from_bundle_strict_config(bundle, &executor_codex_home)
                .expect("executor bundle should convert into layers");
        let config: ConfigToml = layers.enterprise_managed_config[0]
            .config
            .clone()
            .try_into()
            .expect("deserialize executor config");
        let requirements = compose_requirements_for_hostname(
            layers.enterprise_managed_requirements,
            Some("executor-01"),
        )
        .expect("compose executor requirements")
        .expect("executor requirements should be present")
        .into_toml();
        (config, requirements)
    });

    assert_eq!(
        config.sandbox_workspace_write,
        Some(SandboxWorkspaceWrite {
            writable_roots: vec![
                AbsolutePathBuf::from_absolute_path(executor_home.join("cloud-root"))
                    .expect("absolute cloud root"),
                AbsolutePathBuf::from_absolute_path(
                    executor_codex_home.as_path().join("relative-root"),
                )
                .expect("absolute relative root"),
            ],
            ..Default::default()
        })
    );
    assert_eq!(
        requirements,
        ConfigRequirementsToml {
            allowed_sandbox_modes: Some(vec![SandboxModeRequirement::ReadOnly]),
            permissions: Some(PermissionsRequirementsToml {
                filesystem: Some(FilesystemRequirementsToml {
                    deny_read: Some(vec![FilesystemDenyReadPattern::from(
                        AbsolutePathBuf::from_absolute_path(executor_home.join("private"))
                            .expect("absolute private root"),
                    )]),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    );
}

fn aggregate_test_binding() -> (CloudConfigBundlePolicy, CloudConfigBundleBinding) {
    let policy = CloudConfigBundlePolicy::default();
    let mut snapshot = CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    (policy, snapshot.binding.expect("actual published binding"))
}

fn aggregate_changed_bundle() -> CloudConfigBundle {
    let mut bundle = CloudConfigBundle::default();
    bundle
        .config_toml
        .enterprise_managed
        .push(CloudConfigFragment {
            id: "aggregate-policy-change".into(),
            name: "changed caller policy".into(),
            contents: "[agents]\nallowed_model_providers=['openai']\n".into(),
        });
    bundle
}

#[test]
fn aggregate_bindings_validate_distinct_and_every_duplicate_observation() {
    let (a, a_old) = aggregate_test_binding();
    let (b, b_old) = aggregate_test_binding();
    let mut calls = 0;
    assert_eq!(
        CloudConfigBundleBinding::with_current_bindings(&[&a_old, &b_old, &a_old], || {
            calls += 1;
            7
        }),
        Some(7)
    );
    assert_eq!(
        CloudConfigBundleBinding::with_current_bindings(&[&b_old, &a_old, &b_old], || {
            calls += 1;
            8
        }),
        Some(8)
    );
    assert_eq!(
        calls, 2,
        "reversed inputs and shared channels have the same authority"
    );
    let bundle = aggregate_changed_bundle();
    b.observe_remote_bundle(&bundle).unwrap();
    assert_eq!(a_old.read().status, CloudConfigBundleBindingStatus::Current);
    assert_eq!(
        CloudConfigBundleBinding::with_current_bindings(&[&a_old, &b_old], || {
            calls += 1;
        }),
        None
    );
    assert_eq!(
        calls, 2,
        "an independent invalidated caller cannot run admission"
    );
    let mut snapshot = CloudConfigBundleSnapshot {
        bundle: Ok(Some(bundle)),
        binding: None,
        config_server: None,
    };
    b.publish_snapshot(&mut snapshot);
    let b_current = snapshot.binding.unwrap();
    assert_eq!(
        b_current.read().status,
        CloudConfigBundleBindingStatus::Current
    );
    assert_eq!(b_old.read().status, CloudConfigBundleBindingStatus::Stale);
    assert_eq!(
        CloudConfigBundleBinding::with_current_bindings(&[&b_current, &b_old, &b_current], || {
            calls += 1;
        }),
        None
    );
    assert_eq!(
        calls, 2,
        "deduplicating locks cannot discard a stale input revision"
    );
    assert_eq!(
        CloudConfigBundleBinding::with_current_bindings(&[&b_current, &a_old], || 9),
        Some(9)
    );
    assert_eq!(
        CloudConfigBundleBinding::with_current_bindings(&[], || 10),
        Some(10),
        "empty inputs impose no managed restriction and grant no other authority"
    );
    drop(a);
}

#[test]
fn aggregate_duplicate_channel_does_not_recursively_borrow_under_writer_pressure() {
    use std::sync::mpsc;
    use std::time::Duration;
    let (policy, binding) = aggregate_test_binding();
    let first = binding.clone();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let (first_done_tx, first_done_rx) = mpsc::sync_channel(1);
    let first_reader = std::thread::spawn(move || {
        let result =
            CloudConfigBundleBinding::with_current_bindings(&[&first, &first, &first], || {
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("bounded admission release");
                11
            });
        first_done_tx.send(result).unwrap();
    });
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("duplicate positive must enter admission");
    let (writer_entered_tx, writer_entered_rx) = mpsc::sync_channel(1);
    let (writer_done_tx, writer_done_rx) = mpsc::sync_channel(1);
    let writer = std::thread::spawn(move || {
        writer_entered_tx.send(()).unwrap();
        policy
            .observe_remote_bundle(&aggregate_changed_bundle())
            .unwrap();
        writer_done_tx.send(()).unwrap();
    });
    writer_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(
        matches!(
            writer_done_rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "managed publication must wait for the already admitted pure decision"
    );
    let after_publication = binding.clone();
    let (second_done_tx, second_done_rx) = mpsc::sync_channel(1);
    let second_reader = std::thread::spawn(move || {
        let result = CloudConfigBundleBinding::with_current_bindings(&[&binding, &binding], || 12);
        second_done_tx.send(result).unwrap();
    });
    release_tx.send(()).unwrap();
    assert_eq!(
        first_done_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        Some(11)
    );
    writer_done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("writer must complete without recursive read deadlock");
    let racing_reader = second_done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("racing duplicate reader must finish");
    assert!(
        racing_reader.is_none_or(|value| value == 12),
        "a reader admitted before publication may finish; an observation after publication denies"
    );
    assert_eq!(
        CloudConfigBundleBinding::with_current_bindings(
            &[&after_publication, &after_publication],
            || 13
        ),
        None,
        "a reader starting after completed publication must deny"
    );
    first_reader.join().unwrap();
    writer.join().unwrap();
    second_reader.join().unwrap();
}
