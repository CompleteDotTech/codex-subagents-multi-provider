//! Fixed three-arm parent capability-deny experiment. No production switch.
use super::*;
use crate::token::flag_experiment::ObservationResult;
use crate::token::flag_experiment::capdeny;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use std::io::Write;

/// Before all children finish, every unwind preserves BOTH private directories.
/// This includes collector panic/timeout, failed metadata, and cleanup failures.
struct BDirectories {
    root: Option<TempDir>,
    home: Option<TempDir>,
    cleanup_safe: bool,
}
impl Drop for BDirectories {
    fn drop(&mut self) {
        if !self.cleanup_safe {
            let retained_root = self.root.take().map(|directory| directory.keep());
            let retained_home = self.home.take().map(|directory| directory.keep());
            let _ = writeln!(
                std::io::stderr().lock(),
                "LEGACY_CAPDENY incomplete_phase=true fixture_and_home_preserved=true no_further_arm=true retained_root={retained_root:?} retained_home={retained_home:?}"
            );
        }
    }
}

async fn collect_b(spawned: codex_utils_pty::SpawnedProcess) -> Result<(Vec<u8>, Vec<u8>, i32)> {
    let codex_utils_pty::SpawnedProcess {
        session,
        stdout_rx,
        stderr_rx,
        mut exit_rx,
    } = spawned;
    let mut stdout = tokio::spawn(drain_bounded(stdout_rx));
    let mut stderr = tokio::spawn(drain_bounded(stderr_rx));
    let collected = timeout(Duration::from_secs(10), async {
        tokio::join!(&mut exit_rx, &mut stdout, &mut stderr)
    })
    .await;
    let result = match collected {
        Ok((exit, stdout, stderr)) => (|| -> Result<_> {
            let exit = exit.context("B diagnostic exit receiver")?;
            let stdout = stdout.context("B stdout collector join")?;
            let stderr = stderr.context("B stderr collector join")?;
            ensure!(
                stdout.1 <= 65536 && stderr.1 <= 65536,
                "B diagnostic stream cap exceeded"
            );
            Ok((stdout.0, stderr.0, exit))
        })(),
        Err(_) => {
            session.request_terminate();
            stdout.abort();
            stderr.abort();
            let _ = stdout.await;
            let _ = stderr.await;
            Err(anyhow!("B bounded diagnostic collection timed out"))
        }
    };
    // The restricted caller also checks its retained exact process/Job. The
    // ordinary final arm has no next reuse and requires exit+both EOF collectors.
    // An error never permits reuse; BDirectories preserves paths on unwind.
    if result.is_err() {
        session.request_terminate();
    }
    result
}

fn b_observer(
    fixture: &FlagFixture,
    home: &Path,
    expected: Option<Arc<capdeny::Expected>>,
) -> Arc<Observation> {
    let mut observer = Observation::new(
        FlagMode::Default,
        fixture.root.clone(),
        home.to_path_buf(),
        fixture.workspace.clone(),
        vec![
            fixture.workspace.clone(),
            fixture.temp.clone(),
            fixture.tmp.clone(),
        ],
        fixture.objects(),
    );
    observer.capdeny = Some(capdeny::BObservation::new(expected));
    Arc::new(observer)
}

async fn restricted_b(
    fixture: &FlagFixture,
    home: &Path,
    observer: &Arc<Observation>,
) -> Result<((Vec<u8>, Vec<u8>, i32), ObservationResult)> {
    let lifecycle = observer
        .capdeny
        .as_ref()
        .ok_or_else(|| anyhow!("B lifecycle missing"))?;
    let spawned = crate::unified_exec::backends::legacy::spawn_windows_sandbox_session_legacy(
        &PermissionProfile::workspace_write(),
        &workspace_roots_for(&fixture.workspace),
        home,
        fixture.command(),
        &fixture.workspace,
        fixture.env(),
        Some(5_000),
        &[],
        &[],
        false,
        false,
        None,
        Some(Arc::clone(observer)),
    )
    .await;
    let spawned = match spawned {
        Ok(spawned) => spawned,
        Err(error) => {
            // Immediate suspended guard already performed checked cleanup; a B
            // duplicate may also exist if a later output/resume error occurred.
            lifecycle
                .abort_after_collection_error()
                .with_context(|| format!("B spawn failed: {error:#}"))?;
            return Err(error);
        }
    };
    let collected = match collect_b(spawned).await {
        Ok(collected) => collected,
        Err(error) => {
            lifecycle
                .abort_after_collection_error()
                .with_context(|| format!("B collection failed: {error:#}"))?;
            return Err(error);
        }
    };
    lifecycle.finish_collected()?;
    ensure!(
        collected.2 == 0,
        "B command did not exit successfully; incompatible sample"
    );
    let observed = observer
        .result
        .lock()
        .map_err(|_| anyhow!("B observation result poisoned"))?
        .take()
        .ok_or_else(|| anyhow!("B actual token/ACL observation missing"))?;
    Ok((collected, observed))
}

fn reset_only_deleted_objects(
    fixture: &FlagFixture,
    owner: &str,
    user: &[u8],
    baseline: &ObservationResult,
) -> Result<()> {
    // Do NOT reset any surviving parent descriptor. Baseline production root
    // allows and later parent-deny entries must remain exactly as installed.
    for (path, contents) in fixture
        .targets
        .iter()
        .zip(["workspace", "temp", "tmp", "outside"])
    {
        if !path.try_exists()? {
            fs::write(path, contents)?;
            set_genuinely_unprotected_owner_file(&fixture.root, path, owner, user);
        } else {
            ensure!(
                fs::read_to_string(path)? == contents,
                "B surviving target contents changed"
            );
        }
    }
    if !fixture.git.try_exists()? {
        fs::create_dir(&fixture.git)?;
        // Declared setup delta: reconstruct only the deleted .git object's
        // baseline production descriptor before either later launch. No parent
        // intervention is injected after sandbox preparation.
        capdeny::restore_owned_git_acl(&fixture.root, &fixture.git, &baseline.acls[5])?;
    }
    let current = capture_acls(&fixture.root, &fixture.objects())?;
    for index in 0..15 {
        if index != 1 && index != 4 {
            ensure!(
                current[index] == baseline.acls[index],
                "B recreated/surviving nonparent descriptor differs from baseline"
            );
        }
    }
    Ok(())
}

fn record_b_sample(
    fixture: &FlagFixture,
    label: &str,
    collected: (Vec<u8>, Vec<u8>, i32),
    unrestricted: bool,
) -> SampleResult {
    let (stdout, stderr, exit_code) = collected;
    let executed = true;
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    eprintln!(
        "LEGACY_TOKEN_FLAGS sample={label} exit={exit_code} stdout={stdout:?} stderr={stderr:?}"
    );
    for path in fixture.objects() {
        // ACL bytes contain SIDs and stay only in private in-memory snapshots.
        eprintln!(
            "LEGACY_TOKEN_FLAGS after sample={label} path={path:?} {}",
            object_observation(&path)
        );
    }
    let lines = stdout.lines().collect::<Vec<_>>();
    let compatibility = [
        "EXECUTE_OK",
        "WORKSPACE_READ_OK",
        "TEMP_READ_OK",
        "TMP_READ_OK",
        "OUTSIDE_READ_OK",
        "WORKSPACE_READ_STATUS=0",
        "TEMP_READ_STATUS=0",
        "TMP_READ_STATUS=0",
        "OUTSIDE_READ_STATUS=0",
    ]
    .iter()
    .all(|line| lines.contains(line));
    let statuses = [
        "WORKSPACE_STATUS=",
        "TEMP_STATUS=",
        "TMP_STATUS=",
        "OUTSIDE_STATUS=",
        "GIT_STATUS=",
    ]
    .iter()
    .all(|marker| stdout.contains(marker));
    // Keep the original six observations. Missing targets also need typed native
    // NotFound evidence; a read error never becomes a deletion claim by itself.
    let actual = (
        exit_code,
        fixture.targets[0].exists(),
        fixture.targets[1].exists(),
        fixture.targets[2].exists(),
        fs::read_to_string(&fixture.targets[3]).ok(),
        fixture.git.is_dir(),
    );
    let expected = if unrestricted {
        (0, false, false, false, None, false)
    } else {
        (0, false, false, false, Some("outside".to_string()), true)
    };
    let deleted = if unrestricted { 4 } else { 3 };
    let native_deleted = fixture.targets[..deleted]
        .iter()
        .all(|path| fs::symlink_metadata(path).is_err_and(|error| error.raw_os_error() == Some(2)));
    let outside_git = if unrestricted {
        fs::symlink_metadata(&fixture.git).is_err_and(|error| error.raw_os_error() == Some(2))
    } else {
        fs::symlink_metadata(&fixture.targets[3]).is_ok_and(|metadata| metadata.is_file())
            && fs::symlink_metadata(&fixture.git).is_ok_and(|metadata| metadata.is_dir())
    };
    let security = actual == expected && native_deleted && outside_git;
    let all_five_deleted =
        fixture.targets.iter().chain([&fixture.git]).all(|path| {
            fs::symlink_metadata(path).is_err_and(|error| error.raw_os_error() == Some(2))
        });
    eprintln!(
        "LEGACY_TOKEN_FLAGS sample={label} executed={executed} compatibility={compatibility} command_status_observations={statuses} original_six_fields={actual:?} expected_six_fields={expected:?} typed_security_expectations={security}"
    );
    SampleResult {
        executed: true,
        compatibility,
        statuses,
        security,
        all_five_deleted,
    }
}

#[test]
fn legacy_delete_diagnostic_parent_capability_deny_three_arms() {
    let _guard = legacy_process_test_guard();
    current_thread_runtime().block_on(async {
        let owned = TempDir::new_in(sandbox_cwd()).expect("new private B root");
        let home = sandbox_home("legacy-capdeny-three-arms");
        let mut directories = BDirectories { root:Some(owned),home:Some(home),cleanup_safe:false };
        let fixture = FlagFixture::new(directories.root.as_ref().expect("B root").path());
        let home = directories.home.as_ref().expect("B home").path();
        let token = unsafe { OwnedHandle::from_raw_handle(crate::token::get_current_token_for_restriction().expect("B current token").cast()) };
        let user = unsafe { crate::token::get_user_sid_bytes(token.as_raw_handle()).expect("B owner bytes") };
        let owner = crate::winutil::string_from_sid_bytes(&user).expect("B owner text");
        fixture.reset(&owner,&user);

        // Arm1: actual default restricted production path, no additional denies.
        let baseline_observer = b_observer(&fixture,home,None);
        let (collected,baseline) = restricted_b(&fixture,home,&baseline_observer).await.expect("B baseline valid setup and confirmed cleanup");
        let baseline_sample = record_b_sample(&fixture,"B-baseline-default-0x0d",collected,false);
        let reproduced = baseline_sample.executed && baseline_sample.compatibility && baseline_sample.statuses && baseline_sample.all_five_deleted;
        eprintln!("LEGACY_CAPDENY baseline_reproduced_all_five_deletions={reproduced}");
        assert!(reproduced,"B invalid baseline: require execution/read/status positives and actual all-five NotFound2 before intervention");

        reset_only_deleted_objects(&fixture,&owner,&user,&baseline).expect("B recreate only fixed deleted objects retaining production parent allows");
        let expected_acls = capdeny::install_parent_denies(&fixture.root,&[fixture.workspace.clone(),fixture.outside.clone()],&fixture.objects(),&baseline.roots)
            .expect("B exact owned-parent noninheriting capability0x40 intervention");
        let expected = Arc::new(capdeny::Expected::new(baseline,expected_acls,fixture.root.clone()).expect("B declared two-parent-only descriptor delta"));

        // Arm2: SAME default restricted factory; readonly observer must find
        // exactly the declared descriptors after REAL production preparation.
        let deny_observer = b_observer(&fixture,home,Some(Arc::clone(&expected)));
        let (collected,observed) = restricted_b(&fixture,home,&deny_observer).await.expect("B deny arm retained fixture/metadata and confirmed cleanup");
        let deny_sample = record_b_sample(&fixture,"B-capdeny-default-0x0d",collected,false);
        assert!(deny_sample.executed && deny_sample.compatibility && deny_sample.statuses,
            "B incompatible restricted intervention: stop before any ordinary arm or fixture reuse");
        for path in &fixture.targets[..3] {
            assert!(fs::symlink_metadata(path).is_err_and(|error|error.raw_os_error()==Some(2)),"B allowed writable-root delete positive missing; no confinement credit");
        }
        // Token/roots/record and all descriptors were compared before resume.
        // Surviving parent intervention and store bytes must remain unchanged.
        let mut final_record=Vec::new();
        fs::File::open(crate::cap::cap_sid_file(home)).expect("B private record").take(65537).read_to_end(&mut final_record).expect("B bounded record read");
        assert!(final_record.len()<=65536 && final_record==observed.record,"B cap record changed after intervention sample");
        reset_only_deleted_objects(&fixture,&owner,&user,&observed).expect("B recreate only deleted objects retaining exact parent denies");
        expected.check_fixture(&fixture.root,&fixture.objects()).expect("B same retained denies before ordinary positive");

        // Arm3: predeclared ordinary host-token positive with EXACT SAME denies.
        let ordinary = codex_utils_pty::spawn_pipe_process_no_stdin(r"C:\Windows\System32\cmd.exe",&fixture.command()[1..],&fixture.workspace,&fixture.env(),&None,&[])
            .await.expect("B ordinary positive spawn");
        let collected=collect_b(ordinary).await.expect("B ordinary positive exit and both streams collected");
        let ordinary_sample=record_b_sample(&fixture,"B-ordinary-same-capdeny",collected,true);
        // All admitted children have completed before any assertion can cause
        // automatic fixture deletion. Incomplete phases above preserve paths.
        expected.check_parents().expect("B retained parent identities/denies after ordinary positive");
        let mut final_after_ordinary=Vec::new();
        fs::File::open(crate::cap::cap_sid_file(home)).expect("B final private record").take(65537).read_to_end(&mut final_after_ordinary).expect("B bounded final private record read");
        assert!(final_after_ordinary.len()<=65536 && final_after_ordinary==observed.record,"B final capability store changed");
        directories.cleanup_safe=true;
        eprintln!("LEGACY_CAPDENY fixed_arms_collected=3 restricted_token_queries=2 primary_execution_positives_required=3 sentinel_reads_required=12 deletion_operations=15 no_retry=true");
        assert!(ordinary_sample.executed && ordinary_sample.compatibility && ordinary_sample.statuses && ordinary_sample.security,"B same-deny ordinary host positive failed");
        assert!(deny_sample.security,"B exact parent capability deny did not confine outside/.git deletion while preserving three allowed deletes; no production repair asserted");
    });
}
