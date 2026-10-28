use codex_utils_absolute_path::test_support::PathExt;
use std::fs;
use std::fs::FileTimes;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;

use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use uuid::Uuid;

use super::RolloutCompressionTrigger::Startup;
use super::*;
use crate::InitialHistory;
use crate::RolloutConfig;
use crate::RolloutItem;
use crate::RolloutLine;
use crate::RolloutRecorder;
use crate::RolloutRecorderParams;
use crate::append_rollout_item_to_path;
use crate::first_rollout_content_match_snippet;
use crate::read_session_meta_line;
use crate::search_rollout_matches;

#[tokio::test]
async fn load_rollout_items_reads_compressed_rollout() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(1);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "hello compressed")?;
    compress_now(&rollout_path)?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    assert!(!rollout_path.exists());
    assert!(compressed_rollout_path(&rollout_path).exists());
    Ok(())
}

#[tokio::test]
async fn full_history_load_preserves_records_and_errors_across_representations()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(19);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&path, thread_id, "message with unicode: π")?;
    let original = fs::read_to_string(&path)?;
    let expected = RolloutRecorder::load_rollout_items(&path).await?;
    let contents = format!(
        " \r\ninvalid JSON\n{{}}\n{}",
        original.trim_end().replace('\n', "\r\n")
    );
    fs::write(&path, &contents)?;
    let expected = (expected.0, expected.1, 2);
    assert_eq!(
        serde_json::to_value(RolloutRecorder::load_rollout_items(&path).await?)?,
        serde_json::to_value(&expected)?
    );
    compress_now(&path)?;
    assert_eq!(
        serde_json::to_value(RolloutRecorder::load_rollout_items(&path).await?)?,
        serde_json::to_value(&expected)?
    );

    for bytes in [b" \r\n\n".as_slice(), b"valid utf8\n\xff".as_slice()] {
        fs::write(&path, bytes)?;
        let plain = RolloutRecorder::load_rollout_items(&path)
            .await
            .unwrap_err();
        compress_now(&path)?;
        let compressed = RolloutRecorder::load_rollout_items(&path)
            .await
            .unwrap_err();
        assert_eq!(
            (compressed.kind(), compressed.to_string()),
            (plain.kind(), plain.to_string())
        );
    }
    Ok(())
}

#[tokio::test]
async fn read_session_meta_line_stops_before_invalid_utf8_tail() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(16);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "ignored tail")?;
    let mut contents = fs::read(&rollout_path)?;
    contents.resize(contents.len() + 64 * 1024, 0xff);
    fs::write(&rollout_path, contents)?;

    let session_meta = read_session_meta_line(&rollout_path).await?;

    assert_eq!(session_meta.meta.id, thread_id);
    Ok(())
}

#[tokio::test]
async fn read_session_meta_line_stops_before_invalid_utf8_compressed_tail() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(17);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "ignored compressed tail")?;
    let mut contents = fs::read(&rollout_path)?;
    contents.resize(contents.len() + 64 * 1024, 0xff);
    fs::write(&rollout_path, contents)?;
    compress_now(&rollout_path)?;

    let session_meta = read_session_meta_line(&rollout_path).await?;

    assert_eq!(session_meta.meta.id, thread_id);
    assert!(!rollout_path.exists());
    assert!(compressed_rollout_path(&rollout_path).exists());
    Ok(())
}

#[tokio::test]
async fn read_session_meta_line_preserves_pre_header_and_error_semantics() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(18);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "after metadata")?;
    let contents = fs::read_to_string(&rollout_path)?;
    let (session_meta, tail) = contents.split_once('\n').expect("session metadata line");
    let mut session_meta = serde_json::from_str::<serde_json::Value>(session_meta)?;
    session_meta["payload"]
        .as_object_mut()
        .expect("session metadata payload")
        .remove("session_id");
    let contents = format!("{session_meta}\n{tail}");
    let ignored_event = serde_json::json!({
        "timestamp": "2025-01-03T12:00:00Z",
        "type": "event_msg",
        "payload": {"type": "user_message", "message": "before metadata", "kind": "plain"}
    });
    fs::write(
        &rollout_path,
        format!("\nnot json\n{ignored_event}\n{contents}"),
    )?;

    let session_meta = read_session_meta_line(&rollout_path).await?;

    assert_eq!(session_meta.meta.id, thread_id);

    let response_item = serde_json::json!({
        "timestamp": "2025-01-03T12:00:00Z",
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "before metadata"}]
        }
    });
    fs::write(&rollout_path, format!("{response_item}\n{contents}"))?;

    let error = read_session_meta_line(&rollout_path)
        .await
        .expect_err("response item before metadata should fail");

    assert_eq!(
        error.to_string(),
        format!(
            "rollout at {} does not start with session metadata",
            rollout_path.display()
        )
    );

    let unknown_history_mode = serde_json::json!({
        "timestamp": "2025-01-03T12:00:00Z",
        "type": "session_meta",
        "payload": {
            "session_id": uuid.to_string(),
            "id": uuid.to_string(),
            "timestamp": "2025-01-03T12:00:00Z",
            "cwd": ".",
            "originator": "test",
            "cli_version": "test",
            "source": "cli",
            "model_provider": "test-provider",
            "history_mode": "future",
        },
    });
    fs::write(&rollout_path, format!("{unknown_history_mode}\n{contents}"))?;

    let error = read_session_meta_line(&rollout_path)
        .await
        .expect_err("unknown history mode before valid metadata should fail");

    assert!(
        error
            .to_string()
            .starts_with("invalid session metadata history_mode:"),
        "{error}"
    );

    fs::write(&rollout_path, "\nnot json\n")?;

    let error = read_session_meta_line(&rollout_path)
        .await
        .expect_err("rollout without selected records should fail");

    assert_eq!(
        error.to_string(),
        format!("rollout at {} is empty", rollout_path.display())
    );
    Ok(())
}

#[test]
fn rollout_file_from_path_normalizes_compressed_file_names() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(7);
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    let compressed_path = compressed_rollout_path(&rollout_path);

    assert_eq!(
        RolloutFile::from_path(compressed_path.clone()),
        Some(RolloutFile {
            path: compressed_path,
            plain_file_name: format!("rollout-2025-01-03T12-00-00-{uuid}.jsonl"),
        })
    );
    Ok(())
}

#[test]
fn rollout_file_from_path_hides_compressed_sibling_when_plain_exists() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(8);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "plain wins")?;

    assert_eq!(
        RolloutFile::from_path(compressed_rollout_path(&rollout_path)),
        None
    );
    Ok(())
}

#[tokio::test]
async fn append_rollout_item_materializes_compressed_rollout() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(2);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "hello before append")?;
    compress_now(&rollout_path)?;

    append_rollout_item_to_path(
        &rollout_path,
        &RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: "hello after append".to_string(),
            ..Default::default()
        })),
    )
    .await?;

    assert!(rollout_path.exists());
    assert!(!compressed_rollout_path(&rollout_path).exists());
    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 3);
    Ok(())
}

#[tokio::test]
async fn search_rollout_matches_uses_logical_path_for_compressed_rollout() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(15);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "targeted search term")?;
    compress_now(&rollout_path)?;

    // An explicit missing path reliably yields NotFound even when inherited PATH
    // contains inaccessible Windows directories under WSL.
    let missing_ripgrep = home.path().join("missing-rg-for-test");
    let matches = search_rollout_matches(
        &missing_ripgrep,
        home.path(),
        /*archived*/ false,
        "search term",
    )
    .await?;

    assert_eq!(
        matches.get(rollout_path.as_path()),
        Some(&Some("targeted search term".to_string()))
    );
    Ok(())
}

#[tokio::test]
async fn compressed_search_preserves_first_visible_snippet_and_no_match() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(22);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&path, thread_id, "an unrelated first message")?;
    let mut contents = fs::read(&path)?;
    contents.extend_from_slice(b"{malformed needle raw-only}\n");
    fs::write(&path, contents)?;
    for message in [
        "the first Needle [x] is visible",
        "the second needle is visible",
    ] {
        append_rollout_item_to_path(
            &path,
            &RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: message.to_string(),
                ..Default::default()
            })),
        )
        .await?;
    }

    for compressed in [false, true] {
        if compressed {
            compress_now(&path)?;
        }
        for (query, expected) in [
            ("NEEDLE", Some("the first Needle [x] is visible")),
            ("[x]", Some("the first Needle [x] is visible")),
            ("raw-only", None),
            ("absent", None),
        ] {
            assert_eq!(
                first_rollout_content_match_snippet(&path, query).await?,
                expected.map(str::to_string),
                "compressed={compressed}, query={query}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn compressed_search_only_reports_stream_errors_before_a_match() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(23);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&path, thread_id, "needle before unreadable tail")?;
    let mut contents = fs::read(&path)?;
    contents.extend_from_slice(b"\xff\n");
    fs::write(&path, contents)?;
    compress_now(&path)?;
    assert_eq!(
        first_rollout_content_match_snippet(&path, "needle").await?,
        Some("needle before unreadable tail".to_owned())
    );
    assert_eq!(
        first_rollout_content_match_snippet(&path, "absent")
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    Ok(())
}

#[tokio::test]
async fn compressed_search_stops_between_lines_when_cancelled() -> anyhow::Result<()> {
    struct NotifyOnDrop(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for NotifyOnDrop {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(24);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&path, thread_id, "multiple lines are available")?;
    compress_now(&path)?;
    let reader = open_rollout_line_reader(&path).await?;
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let (finished, done) = tokio::sync::oneshot::channel();
    let guard = NotifyOnDrop(Some(finished));
    let mut entered = Some(entered);
    let handle = tokio::spawn(reader.find_map(move |_| {
        let _guard = &guard;
        seen.fetch_add(1, Ordering::SeqCst);
        if let Some(entered) = entered.take() {
            let _ = entered.send(());
            let _ = blocked.recv();
        }
        None::<()>
    }));
    waiting.await?;
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    release.send(())?;
    tokio::time::timeout(Duration::from_secs(5), done).await??;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn worker_compresses_old_active_and_archived_rollouts() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let active_uuid = Uuid::from_u128(3);
    let active_id = ThreadId::from_string(&active_uuid.to_string())?;
    let active_path = rollout_path(home.path(), "2025-01-03T12-00-00", active_uuid);
    write_rollout(&active_path, active_id, "old active")?;
    set_old_mtime(&active_path)?;

    let archived_uuid = Uuid::from_u128(4);
    let archived_id = ThreadId::from_string(&archived_uuid.to_string())?;
    let archived_path = archived_rollout_path(home.path(), "2025-01-04T12-00-00", archived_uuid);
    write_rollout(&archived_path, archived_id, "old archived")?;
    set_old_mtime(&archived_path)?;

    let fresh_uuid = Uuid::from_u128(5);
    let fresh_id = ThreadId::from_string(&fresh_uuid.to_string())?;
    let fresh_path = rollout_path(home.path(), "2025-01-05T12-00-00", fresh_uuid);
    write_rollout(&fresh_path, fresh_id, "fresh active")?;

    let stale_temp = active_path.with_file_name("rollout-stale.jsonl.zst.tmp");
    fs::write(&stale_temp, "stale temp")?;
    set_old_mtime(&stale_temp)?;
    // A crash during metadata restoration can leave the data file unreadable.
    #[cfg(unix)]
    fs::set_permissions(&stale_temp, fs::Permissions::from_mode(0o000))?;

    let fresh_temp = active_path.with_file_name("rollout-fresh.jsonl.zst.tmp");
    fs::write(&fresh_temp, "fresh temp")?;

    // Pause materialization after restoring the old metadata, before publication.
    let mut live_temp = tempfile::Builder::new()
        .prefix(".rollout-materialize-")
        .tempfile_in(active_path.parent().unwrap())?;
    live_temp.write_all(b"live materialization")?;
    access_metadata::copy(&fs::File::open(&active_path)?, live_temp.as_file())?;
    let source_modified = fs::metadata(&active_path)?.modified()?;

    worker::run(home.path().to_path_buf(), Startup).await?;

    assert!(!active_path.exists());
    assert!(compressed_rollout_path(&active_path).exists());
    assert!(!archived_path.exists());
    assert!(compressed_rollout_path(&archived_path).exists());
    assert!(fresh_path.exists());
    assert!(!compressed_rollout_path(&fresh_path).exists());
    assert!(!stale_temp.exists());
    assert!(fresh_temp.exists());
    let materialized_path = home.path().join("materialized.jsonl");
    persist_temp_file_noclobber(live_temp, &materialized_path)?;
    assert_eq!(
        fs::metadata(&materialized_path)?.modified()?,
        source_modified
    );
    assert_eq!(fs::read(&materialized_path)?, b"live materialization");
    assert!(
        home.path()
            .join(".tmp")
            .join("rollout-compression.lock")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn worker_waits_for_rollout_maintenance_before_compressing() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(26);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&path, thread_id, "migration in progress")?;
    set_old_mtime(&path)?;
    let guard = crate::try_acquire_rollout_maintenance_lock(home.path())?
        .expect("claim rollout maintenance lock");

    worker::run(home.path().to_path_buf(), Startup).await?;
    assert!(path.exists());
    assert!(!compressed_rollout_path(&path).exists());

    drop(guard);
    worker::run(home.path().to_path_buf(), Startup).await?;
    assert!(!path.exists());
    assert!(compressed_rollout_path(&path).exists());
    Ok(())
}

#[tokio::test]
async fn worker_compresses_archived_fork_chain() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let thread_id = ThreadId::from_string(&Uuid::from_u128(15).to_string())?;
    let source_uuid = Uuid::from_u128(16);
    let source_rollout_id = ThreadId::from_string(&source_uuid.to_string())?;
    let source_path = rollout_path(home.path(), "2025-01-03T12-00-00", source_uuid);
    write_rollout(&source_path, thread_id, "referenced source")?;
    set_old_mtime(&source_path)?;

    let child_uuid = Uuid::from_u128(17);
    let child_path = archived_rollout_path(home.path(), "2025-01-03T12-00-01", child_uuid);
    write_rollout(&child_path, thread_id, "fork child")?;
    set_history_base(
        child_path.as_path(),
        HistoryPosition {
            thread_id: source_rollout_id,
            end_ordinal_exclusive: 2,
            end_byte_offset: std::fs::metadata(source_path.as_path())?.len(),
        },
    )?;
    set_old_mtime(&child_path)?;
    let original_source = fs::read(&source_path)?;
    let original_child = fs::read(&child_path)?;

    worker::run(home.path().to_path_buf(), Startup).await?;

    for (path, original) in [
        (&source_path, original_source),
        (&child_path, original_child),
    ] {
        assert!(!path.exists(), "compression should replace {path:?}");
        let mut restored = Vec::new();
        crate::open_rollout_seekable_reader(path, || false)?.read_to_end(&mut restored)?;
        assert_eq!(restored, original);
    }
    Ok(())
}

#[tokio::test]
async fn worker_skips_unreadable_metadata_without_blocking_other_compression() -> anyhow::Result<()>
{
    let home = TempDir::new()?;
    let source_uuid = Uuid::from_u128(20);
    let source_id = ThreadId::from_string(&source_uuid.to_string())?;
    let source_path = rollout_path(home.path(), "2025-01-03T12-00-00", source_uuid);
    write_rollout(&source_path, source_id, "candidate")?;
    set_old_mtime(&source_path)?;

    let unreadable_path = rollout_path(home.path(), "2025-01-03T12-00-01", Uuid::from_u128(21));
    fs::write(unreadable_path.as_path(), "{not json}\n")?;

    worker::run(home.path().to_path_buf(), Startup).await?;

    assert!(!source_path.exists());
    assert!(compressed_rollout_path(&source_path).exists());
    assert!(unreadable_path.exists());
    Ok(())
}

#[tokio::test]
async fn resume_materializes_compressed_rollout_path() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let config = RolloutConfig {
        codex_home: home.path().to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        cwd: home.path().to_path_buf(),
        model_provider_id: "test-provider".to_string(),
        generate_memories: true,
    };
    let uuid = Uuid::from_u128(3);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "hello before resume")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);
    set_old_mtime(compressed_path.as_path())?;
    let compressed_modified = fs::metadata(compressed_path.as_path())?.modified()?;

    let InitialHistory::Resumed(history) =
        RolloutRecorder::get_rollout_history(compressed_path.as_path()).await?
    else {
        panic!("expected compressed rollout to load as resumed history");
    };
    assert_eq!(history.rollout_path, Some(rollout_path.clone()));

    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::resume(compressed_path.clone()),
    )
    .await?;

    assert_eq!(recorder.rollout_path(), rollout_path.as_path());
    assert!(rollout_path.exists());
    assert!(!compressed_path.exists());
    assert!(fs::metadata(rollout_path.as_path())?.modified()? > compressed_modified);
    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(EventMsg::UserMessage(
            UserMessageEvent {
                message: "hello after resume".to_string(),
                ..Default::default()
            },
        ))])
        .await?;
    recorder.flush().await?;
    recorder.shutdown().await?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 3);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn compression_preserves_rollout_permissions() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(6);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "restricted transcript")?;
    fs::set_permissions(&rollout_path, fs::Permissions::from_mode(0o600))?;
    set_old_mtime(&rollout_path)?;

    worker::run(home.path().to_path_buf(), Startup).await?;

    let compressed_path = compressed_rollout_path(&rollout_path);
    assert!(!rollout_path.exists());
    assert_eq!(
        fs::metadata(&compressed_path)?.permissions().mode() & 0o777,
        0o600
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_preserves_inherited_acl_through_append() -> anyhow::Result<()> {
    use anyhow::Context;
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;

    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(61);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&path, thread_id, "ACL-protected transcript")?;
    set_old_mtime(&path)?;
    // An archived rollout can retain inherited denies from its former directory.
    let acl = Command::new("/bin/chmod")
        .args(["+ai#", "0", "everyone deny writeattr"])
        .arg(&path)
        .output()?;
    assert!(acl.status.success(), "{acl:?}");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000))?;
    let acl = Command::new("/bin/chmod")
        .args(["+a#", "1", "everyone allow read,write,append,writeattr"])
        .arg(&path)
        .output()?;
    assert!(acl.status.success(), "{acl:?}");
    let before = fs::metadata(&path)?;
    let expected = RolloutRecorder::load_rollout_items(&path)
        .await
        .context("read the ACL-protected source")?;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .context("the source ACL must permit appending before compression")?;
    assert_eq!(
        fs::File::open(&path)?
            .set_times(FileTimes::new().set_modified(SystemTime::now()))
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );

    // Exercise discovery, compression, final metadata and publication through
    // the real worker, not the test-only compress_now helper.
    worker::run(home.path().to_path_buf(), Startup).await?;
    let compressed = compressed_rollout_path(&path);
    assert!(!path.exists());
    assert_eq!(
        fs::File::open(&compressed)?
            .set_times(FileTimes::new().set_modified(SystemTime::now()))
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let after = fs::metadata(&compressed)?;
    assert_eq!(after.modified()?, before.modified()?);
    assert_eq!(
        (after.uid(), after.gid(), after.permissions().mode()),
        (before.uid(), before.gid(), before.permissions().mode())
    );
    assert_eq!(
        serde_json::to_value(
            RolloutRecorder::load_rollout_items(&path)
                .await
                .context("read the worker's compressed replacement")?
        )?,
        serde_json::to_value(&expected)?
    );

    let appended = RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        message: "append through the preserved ACL".to_string(),
        ..Default::default()
    }));
    append_rollout_item_to_path(&path, &appended)
        .await
        .context("materialize and append through the preserved ACL")?;
    assert!(!compressed.exists());
    assert_eq!(
        fs::File::open(&path)?
            .set_times(FileTimes::new().set_modified(SystemTime::now()))
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let after = fs::metadata(&path)?;
    assert_eq!(
        (after.uid(), after.gid(), after.permissions().mode()),
        (before.uid(), before.gid(), before.permissions().mode())
    );
    let mut expected_items = expected.0;
    expected_items.push(appended);
    assert_eq!(
        serde_json::to_value(RolloutRecorder::load_rollout_items(&path).await?)?,
        serde_json::to_value((expected_items, expected.1, expected.2))?
    );
    Ok(())
}

#[cfg(unix)]
struct AlternateGroupDiagnostics {
    remaining: usize,
}

#[cfg(unix)]
impl AlternateGroupDiagnostics {
    fn emit(&mut self, message: &str) {
        use std::io::Write;
        let line = format!("ROLLOUT_GID_DIAG {message}\n");
        let bytes = line.as_bytes();
        let count = bytes.len().min(self.remaining);
        self.remaining -= count;
        // Diagnostic output must never replace the original test result.
        let _ = std::io::stderr().lock().write_all(&bytes[..count]);
    }

    fn stage<T, E: Into<anyhow::Error>>(
        &mut self,
        stage: &'static str,
        result: Result<T, E>,
    ) -> anyhow::Result<T> {
        match result {
            Ok(value) => {
                self.emit(&format!("stage={stage} completed=1"));
                Ok(value)
            }
            Err(error) => {
                let error = error.into();
                if let Some(io) = error.downcast_ref::<std::io::Error>() {
                    self.emit(&format!(
                        "stage={stage} completed=0 errno={:?}",
                        io.raw_os_error()
                    ));
                } else {
                    self.emit(&format!("stage={stage} completed=0"));
                }
                Err(error.context(stage))
            }
        }
    }

    fn process(&mut self) {
        use std::io::Read;
        // Fixed files, bounded reads, selected numeric fields only; no environment,
        // account names, arbitrary paths, or subprocesses are collected here.
        for (role, path, limit) in [
            ("uid_map", "/proc/self/uid_map", 16384),
            ("gid_map", "/proc/self/gid_map", 16384),
            ("status", "/proc/self/status", 16384),
            ("setgroups", "/proc/self/setgroups", 64),
        ] {
            let result = (|| -> std::io::Result<Vec<u8>> {
                let mut bytes = Vec::new();
                fs::File::open(path)?
                    .take(limit + 1)
                    .read_to_end(&mut bytes)?;
                Ok(bytes)
            })();
            match result {
                Ok(bytes) => {
                    self.emit(&format!(
                        "proc={role} truncated={}",
                        bytes.len() as u64 > limit
                    ));
                    let bytes = &bytes[..bytes.len().min(limit as usize)];
                    if role == "setgroups" {
                        self.emit(&format!(
                            "proc=setgroups allow={} deny={}",
                            bytes == b"allow\n",
                            bytes == b"deny\n"
                        ));
                        continue;
                    }
                    for line in String::from_utf8_lossy(bytes).lines().take(64) {
                        let (field, values) = if role == "status" {
                            let Some((field, values)) = line.split_once(':') else {
                                continue;
                            };
                            if !["Uid", "Gid", "Groups", "NoNewPrivs", "Seccomp", "CapEff"]
                                .contains(&field)
                            {
                                continue;
                            }
                            (field, values)
                        } else {
                            (role, line)
                        };
                        let numeric: Vec<_> = values
                            .split_whitespace()
                            .take(32)
                            .filter(|value| value.bytes().all(|byte| byte.is_ascii_hexdigit()))
                            .collect();
                        self.emit(&format!(
                            "proc={role} field={field} numeric={}",
                            numeric.join(" ")
                        ));
                    }
                }
                Err(error) => self.emit(&format!(
                    "proc={role} unavailable=1 errno={:?}",
                    error.raw_os_error()
                )),
            }
        }
    }

    fn file(&mut self, role: &'static str, path: &std::path::Path) {
        use std::os::unix::fs::MetadataExt;
        match fs::metadata(path) {
            Ok(meta) => self.emit(&format!(
                "file={role} uid={} gid={} dev={} ino={} mode={} mtime={} mtime_nsec={}",
                meta.uid(),
                meta.gid(),
                meta.dev(),
                meta.ino(),
                meta.mode(),
                meta.mtime(),
                meta.mtime_nsec()
            )),
            Err(error) => self.emit(&format!(
                "file={role} unavailable=1 errno={:?}",
                error.raw_os_error()
            )),
        }
    }
}

#[cfg(target_os = "linux")]
fn mapped_alternate_group(
    groups: &[u32],
    original_group: u32,
    gid_map: &str,
) -> anyhow::Result<Option<u32>> {
    // The first column is the namespace-visible ID range. In particular, an
    // overflowgid reported by `id -G` is not necessarily a mapped member group.
    let mut ranges = Vec::new();
    for line in gid_map.lines() {
        let values = line
            .split_whitespace()
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(values.len() == 3, "invalid gid_map column count");
        let end = values[0].checked_add(values[2]);
        let parent_end = values[1].checked_add(values[2]);
        anyhow::ensure!(
            values[2] > 0
                && end.is_some_and(|end| end <= u64::from(u32::MAX))
                && parent_end.is_some_and(|end| end <= u64::from(u32::MAX)),
            "invalid gid_map range"
        );
        ranges.push((values[0], end.expect("validated range")));
    }
    Ok(groups.iter().copied().find(|group| {
        *group != original_group
            && ranges
                .iter()
                .any(|(start, end)| *start <= u64::from(*group) && u64::from(*group) < *end)
    }))
}

#[cfg(target_os = "linux")]
#[test]
fn alternate_group_selection_requires_a_namespace_mapping() -> anyhow::Result<()> {
    assert_eq!(
        mapped_alternate_group(&[65534, 1000, 65534], 1000, "1000 1000 1\n")?,
        None,
        "unmapped supplementary groups must not masquerade as overflowgid membership"
    );
    assert_eq!(
        mapped_alternate_group(&[4, 1000], 1000, "0 0 4294967295\n")?,
        Some(4)
    );
    assert_eq!(
        mapped_alternate_group(&[65534, 1000], 1000, "1000 1000 1\n65534 65534 1\n")?,
        Some(65534),
        "a genuinely mapped 65534 is valid; do not blacklist the numeric overflow default"
    );
    assert_eq!(mapped_alternate_group(&[4, 1000], 1000, "")?, None);
    for invalid in [
        "1000 1000",
        "1 1 0",
        "4294967295 1 1",
        "1 18446744073709551615 1",
    ] {
        assert!(mapped_alternate_group(&[4, 1000], 1000, invalid).is_err());
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn worker_preserves_alternate_group_through_materialization() -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;

    let mut diagnostics = AlternateGroupDiagnostics {
        remaining: 64 * 1024,
    };
    diagnostics.process();
    let home = diagnostics.stage("create_home", TempDir::new())?;
    let uuid = Uuid::from_u128(62);
    let thread_id =
        diagnostics.stage("parse_thread_id", ThreadId::from_string(&uuid.to_string()))?;
    let path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    diagnostics.stage(
        "write_fixture",
        write_rollout(&path, thread_id, "different group"),
    )?;
    diagnostics.stage("age_fixture", set_old_mtime(&path))?;
    let original_group = diagnostics
        .stage("read_original_metadata", fs::metadata(&path))?
        .gid();
    let groups = diagnostics.stage("list_groups", Command::new("id").arg("-G").output())?;
    assert!(groups.status.success());
    let groups_text =
        diagnostics.stage("parse_groups_utf8", std::str::from_utf8(&groups.stdout))?;
    let groups = diagnostics.stage(
        "parse_groups",
        groups_text
            .split_whitespace()
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>(),
    )?;
    #[cfg(target_os = "linux")]
    let group = {
        let gid_map =
            diagnostics.stage("read_gid_map", fs::read_to_string("/proc/self/gid_map"))?;
        diagnostics.stage(
            "select_mapped_group",
            mapped_alternate_group(&groups, original_group, &gid_map),
        )?
    };
    #[cfg(not(target_os = "linux"))]
    let group = groups.into_iter().find(|group| *group != original_group);
    let required = match std::env::var_os("CODEX_ROLLOUT_REQUIRE_ALTERNATE_GROUP") {
        None => false,
        Some(value) => {
            anyhow::ensure!(
                value == "1",
                "invalid required alternate-group test setting"
            );
            true
        }
    };
    let Some(group) = group else {
        anyhow::ensure!(
            !required,
            "required ownership regression has no mapped alternate supplementary group"
        );
        diagnostics.emit("coverage=not_exercised reason=no_mapped_alternate_group");
        eprintln!(
            "ownership regression not exercised: no mapped alternate supplementary group; \
             the required Linux host companion must exercise this case"
        );
        return Ok(());
    };
    diagnostics.emit(&format!(
        "selected_gid={group} original_gid={original_group}"
    ));
    diagnostics.file("before_chown", &path);
    diagnostics.stage(
        "set_alternate_group",
        std::os::unix::fs::chown(&path, /*uid*/ None, Some(group)),
    )?;
    diagnostics.file("after_chown", &path);
    assert_eq!(
        diagnostics
            .stage("verify_alternate_metadata", fs::metadata(&path))?
            .gid(),
        group
    );

    diagnostics.emit("stage=run_worker begin=1");
    diagnostics.stage(
        "run_worker",
        worker::run(home.path().to_path_buf(), Startup).await,
    )?;
    assert!(!path.exists());
    assert_eq!(
        diagnostics
            .stage(
                "read_compressed_metadata",
                fs::metadata(compressed_rollout_path(&path))
            )?
            .gid(),
        group
    );
    diagnostics.file("before_materialize", &compressed_rollout_path(&path));
    diagnostics.emit("stage=materialize_append begin=1");
    diagnostics.stage(
        "materialize_append",
        materialize_rollout_for_append(&path, /*writer_lock*/ None).await,
    )?;
    assert_eq!(
        diagnostics
            .stage("verify_restored_metadata", fs::metadata(&path))?
            .gid(),
        group
    );
    diagnostics.emit("coverage=exercised stages=chown_worker_materialize");
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_does_not_leak_temps_for_deletion_protected_rollouts() -> anyhow::Result<()> {
    use std::process::Command;

    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(63);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&path, thread_id, "deletion-protected transcript")?;
    set_old_mtime(&path)?;
    let acl = Command::new("/bin/chmod")
        .args(["+a", "everyone deny delete,writesecurity"])
        .arg(&path)
        .output()?;
    assert!(acl.status.success(), "{acl:?}");
    let contents = fs::read(&path)?;
    let modified = fs::metadata(&path)?.modified()?;
    let access = Command::new("/bin/ls").arg("-le").arg(&path).output()?;
    assert!(access.status.success(), "{access:?}");

    // Collect results before asserting so even a regression's protected temps
    // have their ACLs cleared before TempDir tries to remove them.
    let result: anyhow::Result<_> = async {
        let mut files_after_passes = Vec::new();
        for _ in 0..2 {
            worker::run(home.path().to_path_buf(), Startup).await?;
            files_after_passes.push(
                fs::read_dir(path.parent().unwrap())?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<io::Result<Vec<_>>>()?,
            );
            // Allow the next pass without waiting for the run marker to expire.
            fs::remove_file(home.path().join(".tmp/rollout-compression.lock"))?;
        }
        let access_after = Command::new("/bin/ls").arg("-le").arg(&path).output()?;
        Ok((
            files_after_passes,
            fs::read(&path)?,
            fs::metadata(&path)?.modified()?,
            access_after,
            fs::remove_file(&path).map_err(|err| err.kind()),
        ))
    }
    .await;
    for entry in fs::read_dir(path.parent().unwrap())? {
        let cleanup = Command::new("/bin/chmod")
            .arg("-N")
            .arg(entry?.path())
            .output()?;
        assert!(cleanup.status.success(), "{cleanup:?}");
    }
    let (files_after_passes, contents_after, modified_after, access_after, deletion) = result?;
    assert!(access_after.status.success(), "{access_after:?}");
    assert_eq!(files_after_passes, vec![vec![path.clone()], vec![path]]);
    assert_eq!(
        (
            contents_after,
            modified_after,
            access_after.stdout,
            deletion
        ),
        (
            contents,
            modified,
            access.stdout,
            Err(io::ErrorKind::PermissionDenied)
        )
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn append_materialization_preserves_compressed_rollout_permissions() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(6);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "restricted transcript")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);
    fs::set_permissions(&compressed_path, fs::Permissions::from_mode(0o600))?;

    append_rollout_item_to_path(
        &rollout_path,
        &RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: "materialize restricted transcript".to_string(),
            ..Default::default()
        })),
    )
    .await?;

    assert!(rollout_path.exists());
    assert!(!compressed_path.exists());
    assert_eq!(
        fs::metadata(&rollout_path)?.permissions().mode() & 0o777,
        0o600
    );
    Ok(())
}

#[test]
fn persist_temp_file_noclobber_installs_completed_temp() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let mut temp_file = tempfile::NamedTempFile::new_in(home.path())?;
    let temp_path = temp_file.path().to_path_buf();
    let destination = home.path().join("rollout.jsonl");
    temp_file.write_all(b"completed rollout")?;

    persist_temp_file_noclobber(temp_file, &destination)?;

    assert!(!temp_path.exists());
    assert_eq!(fs::read_to_string(destination)?, "completed rollout");
    Ok(())
}

#[test]
fn persist_temp_file_noclobber_does_not_replace_existing_destination() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let mut temp_file = tempfile::NamedTempFile::new_in(home.path())?;
    let temp_path = temp_file.path().to_path_buf();
    let destination = home.path().join("rollout.jsonl");
    temp_file.write_all(b"candidate rollout")?;
    fs::write(&destination, "existing rollout")?;

    persist_temp_file_noclobber(temp_file, &destination)?;

    assert!(!temp_path.exists());
    assert_eq!(fs::read_to_string(destination)?, "existing rollout");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn compression_preserves_read_only_rollout_permissions() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(7);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "read-only transcript")?;
    set_old_mtime(&rollout_path)?;
    fs::set_permissions(&rollout_path, fs::Permissions::from_mode(0o400))?;
    let source_modified = fs::metadata(&rollout_path)?.modified()?;
    #[cfg(target_os = "macos")]
    {
        // New files inherit an allow that is absent from the existing source.
        let acl = std::process::Command::new("/bin/chmod")
            .args([
                "+a",
                "everyone allow write,append,file_inherit,only_inherit",
            ])
            .arg(rollout_path.parent().unwrap())
            .output()?;
        assert!(acl.status.success(), "{acl:?}");
    }

    worker::run(home.path().to_path_buf(), Startup).await?;

    let compressed_path = compressed_rollout_path(&rollout_path);
    let compressed_metadata = fs::metadata(&compressed_path)?;
    assert!(!rollout_path.exists());
    assert_eq!(compressed_metadata.permissions().mode() & 0o777, 0o400);
    assert_eq!(compressed_metadata.modified()?, source_modified);
    #[cfg(target_os = "macos")]
    {
        assert_eq!(
            fs::OpenOptions::new()
                .append(true)
                .open(&compressed_path)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        crate::materialize_rollout_for_reference(&rollout_path).await?;
        assert_eq!(fs::metadata(&rollout_path)?.modified()?, source_modified);
        assert_eq!(
            fs::OpenOptions::new()
                .append(true)
                .open(&rollout_path)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }
    Ok(())
}

#[tokio::test]
async fn worker_skips_existing_compressed_archived_rollouts() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(10);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "already compressed")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);
    set_old_mtime(&compressed_path)?;

    worker::run(home.path().to_path_buf(), Startup).await?;

    assert!(!rollout_path.exists());
    assert!(compressed_path.exists());
    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    Ok(())
}

#[tokio::test]
async fn worker_skips_when_fresh_run_marker_exists() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(11);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "throttled worker")?;
    set_old_mtime(&rollout_path)?;
    let marker_dir = home.path().join(".tmp");
    fs::create_dir_all(marker_dir.as_path())?;
    fs::write(marker_dir.join("rollout-compression.lock"), "recent run")?;

    worker::run(home.path().to_path_buf(), Startup).await?;

    assert!(rollout_path.exists());
    assert!(!compressed_rollout_path(&rollout_path).exists());
    Ok(())
}

#[test]
fn run_marker_is_removed_unless_persisted() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let marker_path = home.path().join(".tmp").join("rollout-compression.lock");

    {
        let marker = worker::CompressionRunMarker::try_claim(home.path())?;
        assert!(marker.is_some());
    }
    assert!(!marker_path.exists());

    let marker = worker::CompressionRunMarker::try_claim(home.path())?;
    let Some(marker) = marker else {
        panic!("expected run marker claim");
    };
    marker.persist();
    assert!(marker_path.exists());
    assert!(worker::CompressionRunMarker::try_claim(home.path())?.is_none());
    Ok(())
}

#[tokio::test]
async fn find_thread_path_by_id_handles_compressed_rollout_filenames() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(8);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "compressed filename lookup")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);

    for id in 1000..1064 {
        let unrelated_path = compressed_rollout_path(&rollout_path.with_file_name(format!(
            "rollout-2025-01-03T12-00-00-{}.jsonl",
            Uuid::from_u128(id)
        )));
        fs::write(unrelated_path, zstd::stream::encode_all(&b""[..], 0)?)?;
    }

    assert_eq!(
        crate::find_thread_path_by_id_str(
            home.path(),
            &uuid.to_string(),
            /*state_db_ctx*/ None
        )
        .await?,
        Some(compressed_path.clone())
    );
    assert_eq!(
        crate::find_rollout_path_by_rollout_id(home.path(), thread_id).await?,
        Some(compressed_path)
    );

    write_rollout(&rollout_path, thread_id, "plain filename lookup")?;
    assert_eq!(
        crate::find_thread_path_by_id_str(
            home.path(),
            &uuid.to_string(),
            /*state_db_ctx*/ None
        )
        .await?,
        Some(rollout_path.clone())
    );
    assert_eq!(
        crate::find_rollout_path_by_rollout_id(home.path(), thread_id).await?,
        Some(rollout_path)
    );
    let missing_rollout_id = ThreadId::from_string(&Uuid::from_u128(2000).to_string())?;
    assert_eq!(
        crate::find_rollout_path_by_rollout_id(home.path(), missing_rollout_id).await?,
        None
    );
    assert_eq!(
        crate::find_thread_path_by_id_str(home.path(), "not-a-uuid", /*state_db_ctx*/ None).await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn find_thread_path_by_id_ignores_compression_temp_matches() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(9);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let temp_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid).with_file_name(format!(
        "rollout-2025-01-03T12-00-00-{uuid}.jsonl.zst.compress.1.0.tmp"
    ));
    write_rollout(&temp_path, thread_id, "temporary file should not resolve")?;

    assert_eq!(
        crate::find_thread_path_by_id_str(
            home.path(),
            &uuid.to_string(),
            /*state_db_ctx*/ None
        )
        .await?,
        None
    );
    Ok(())
}

fn rollout_path(home: &std::path::Path, ts: &str, uuid: Uuid) -> std::path::PathBuf {
    home.join("sessions/2025/01/03")
        .join(format!("rollout-{ts}-{uuid}.jsonl"))
}

fn archived_rollout_path(home: &std::path::Path, ts: &str, uuid: Uuid) -> std::path::PathBuf {
    home.join("archived_sessions")
        .join(format!("rollout-{ts}-{uuid}.jsonl"))
}

fn write_rollout(path: &std::path::Path, thread_id: ThreadId, message: &str) -> anyhow::Result<()> {
    let parent = path.parent().expect("rollout path should have parent");
    fs::create_dir_all(parent)?;
    let session_meta_line = SessionMetaLine {
        meta: SessionMeta {
            creator_user_id: None,
            creator_account_id: None,
            session_id: thread_id.into(),
            id: thread_id,
            forked_from_id: None,
            forked_from_ordinal_exclusive: None,
            parent_thread_id: None,
            timestamp: "2025-01-03T12:00:00Z".to_string(),
            cwd: parent.to_path_buf(),
            runtime_workspace_roots: None,
            originator: "test".to_string(),
            cli_version: "test".to_string(),
            source: SessionSource::Cli,
            thread_source: None,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
            model_provider: None,
            base_instructions: None,
            dynamic_tools: None,
            selected_capability_roots: Vec::new(),
            memory_mode: None,
            history_mode: Default::default(),
            history_base: None,
            subagent_history_start_ordinal: None,
            multi_agent_version: None,
            context_window: None,
        },
        git: None,
    };
    let lines = [
        RolloutLine {
            timestamp: "2025-01-03T12:00:00Z".to_string(),
            ordinal: None,
            item: RolloutItem::SessionMeta(Box::new(session_meta_line)),
        },
        RolloutLine {
            timestamp: "2025-01-03T12:00:01Z".to_string(),
            ordinal: None,
            item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: message.to_string(),
                ..Default::default()
            })),
        },
    ];
    let jsonl = lines
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    fs::write(path, format!("{jsonl}\n"))?;
    Ok(())
}

fn set_history_base(path: &std::path::Path, history_base: HistoryPosition) -> anyhow::Result<()> {
    let contents = fs::read_to_string(path)?;
    let mut lines = contents.lines();
    let mut head: serde_json::Value = serde_json::from_str(lines.next().expect("session meta"))?;
    head["payload"]["history_base"] = serde_json::to_value(history_base)?;
    let mut updated = serde_json::to_string(&head)?;
    for line in lines {
        updated.push('\n');
        updated.push_str(line);
    }
    updated.push('\n');
    fs::write(path, updated)?;
    Ok(())
}

fn compress_now(path: &std::path::Path) -> anyhow::Result<()> {
    let compressed_path = compressed_rollout_path(path);
    let input = fs::File::open(path)?;
    let output = fs::File::create(compressed_path)?;
    let mut encoder = zstd::stream::write::Encoder::new(output, 3)?;
    let mut input = std::io::BufReader::new(input);
    std::io::copy(&mut input, &mut encoder)?;
    encoder.finish()?;
    fs::remove_file(path)?;
    Ok(())
}

fn set_old_mtime(path: &std::path::Path) -> anyhow::Result<()> {
    let old = SystemTime::now()
        .checked_sub(Duration::from_secs(8 * 24 * 60 * 60))
        .expect("old timestamp should be representable");
    let times = FileTimes::new().set_modified(old);
    fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_times(times)?;
    Ok(())
}

#[tokio::test]
async fn worker_compresses_parallel_cold_candidates_without_skipping_other_publications()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let mut paths = Vec::new();
    for index in 0..32 {
        let id = Uuid::from_u128(2000 + index);
        let path = rollout_path(home.path(), "2025-01-03T12-00-00", id);
        write_rollout(
            &path,
            ThreadId::from_string(&id.to_string()).unwrap(),
            "cold candidate",
        )?;
        set_old_mtime(&path)?;
        paths.push(path);
    }
    worker::run(home.path().to_path_buf(), Startup).await?;
    let representations = paths
        .iter()
        .map(|path| (path.exists(), compressed_rollout_path(path).exists()))
        .collect::<Vec<_>>();
    assert_eq!(representations, vec![(false, true); paths.len()]);
    Ok(())
}
