//! Three fixed samples sharing owned paths and actual stored capabilities.

use super::*;
use crate::token::flag_experiment::FlagMode;
use crate::token::flag_experiment::Observation;
use crate::token::flag_experiment::capture_acls;
use pretty_assertions::assert_eq;
use std::io::Read;
use std::sync::Arc;

struct FlagFixture {
    root: PathBuf,
    workspace: PathBuf,
    temp: PathBuf,
    tmp: PathBuf,
    outside: PathBuf,
    git: PathBuf,
    script: PathBuf,
    targets: [PathBuf; 4],
    sentinels: [PathBuf; 4],
}

impl FlagFixture {
    fn new(root: &Path) -> Self {
        let workspace = root.join("workspace");
        let temp = root.join("temp");
        let tmp = root.join("tmp");
        let outside = root.join("outside");
        Self {
            root: root.to_path_buf(),
            git: workspace.join(".git"),
            script: workspace.join("delete-fixtures.cmd"),
            targets: [
                workspace.join("workspace-delete.txt"),
                temp.join("temp-delete.txt"),
                tmp.join("tmp-delete.txt"),
                outside.join("outside-delete.txt"),
            ],
            sentinels: [
                workspace.join("workspace-survives.txt"),
                temp.join("temp-survives.txt"),
                tmp.join("tmp-survives.txt"),
                outside.join("outside-survives.txt"),
            ],
            workspace,
            temp,
            tmp,
            outside,
        }
    }

    fn directories(&self) -> Vec<PathBuf> {
        vec![
            self.root.clone(),
            self.workspace.clone(),
            self.temp.clone(),
            self.tmp.clone(),
            self.outside.clone(),
            self.git.clone(),
        ]
    }

    fn objects(&self) -> Vec<PathBuf> {
        self.directories()
            .into_iter()
            .chain(self.targets.iter().cloned())
            .chain(self.sentinels.iter().cloned())
            .chain([self.script.clone()])
            .collect()
    }

    fn reset(&self, owner: &str, user: &[u8]) {
        // Fixed procedure, including ALL owned directory and file DACLs. No
        // production cache reset, post-preparation repair, or outside ancestor.
        for path in self.directories() {
            if !path.try_exists().expect("observe owned fixture directory") {
                assert!(path.parent().expect("owned directory parent").is_dir());
                fs::create_dir(&path).expect("recreate only fixed owned directory");
            }
            reset_full_owner_directory(&self.root, &path, owner);
        }
        for (path, contents) in self
            .targets
            .iter()
            .zip(["workspace", "temp", "tmp", "outside"])
        {
            fs::write(path, contents).expect("reset owned deletion target");
        }
        for (path, contents) in self.sentinels.iter().zip([
            "WORKSPACE_READ_OK\r\n",
            "TEMP_READ_OK\r\n",
            "TMP_READ_OK\r\n",
            "OUTSIDE_READ_OK\r\n",
        ]) {
            fs::write(path, contents).expect("reset owned read sentinel");
        }
        fs::write(
            &self.script,
            concat!(
                "@echo off\r\n",
                "echo EXECUTE_OK\r\n",
                "type \"%WORKSPACE_READ%\"\r\necho WORKSPACE_READ_STATUS=%errorlevel%\r\n",
                "type \"%TEMP_READ%\"\r\necho TEMP_READ_STATUS=%errorlevel%\r\n",
                "type \"%TMP_READ%\"\r\necho TMP_READ_STATUS=%errorlevel%\r\n",
                "type \"%OUTSIDE_READ%\"\r\necho OUTSIDE_READ_STATUS=%errorlevel%\r\n",
                "del /f /q \"%WORKSPACE_DELETE%\"\r\necho WORKSPACE_STATUS=%errorlevel%\r\n",
                "del /f /q \"%TEMP_DELETE%\"\r\necho TEMP_STATUS=%errorlevel%\r\n",
                "del /f /q \"%TMP_DELETE%\"\r\necho TMP_STATUS=%errorlevel%\r\n",
                "del /f /q \"%OUTSIDE_DELETE%\"\r\necho OUTSIDE_STATUS=%errorlevel%\r\n",
                "rmdir \"%PROTECTED_GIT_DIR%\"\r\necho GIT_STATUS=%errorlevel%\r\n",
                "exit /b 0\r\n",
            ),
        )
        .expect("write fixed owned command script");
        for path in self
            .targets
            .iter()
            .chain(&self.sentinels)
            .chain([&self.script])
        {
            set_genuinely_unprotected_owner_file(&self.root, path, owner, user);
        }
        for path in self.directories() {
            assert!(
                crate::acl::path_mask_allows(
                    &path,
                    &[user.as_ptr().cast_mut().cast()],
                    windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS,
                    /*require_all_bits*/ true
                )
                .expect("full owner directory recovery grant")
            );
        }
    }

    fn env(&self) -> HashMap<String, String> {
        let mut env = HashMap::from([(
            "SystemRoot".to_string(),
            std::env::var("SystemRoot").expect("host SystemRoot"),
        )]);
        for (name, path) in [
            ("TEMP", &self.temp),
            ("TMP", &self.tmp),
            ("WORKSPACE_DELETE", &self.targets[0]),
            ("TEMP_DELETE", &self.targets[1]),
            ("TMP_DELETE", &self.targets[2]),
            ("OUTSIDE_DELETE", &self.targets[3]),
            ("PROTECTED_GIT_DIR", &self.git),
            ("WORKSPACE_READ", &self.sentinels[0]),
            ("TEMP_READ", &self.sentinels[1]),
            ("TMP_READ", &self.sentinels[2]),
            ("OUTSIDE_READ", &self.sentinels[3]),
        ] {
            env.insert(name.to_string(), path.to_string_lossy().into_owned());
        }
        env
    }

    fn command(&self) -> Vec<String> {
        vec![
            "C:\\Windows\\System32\\cmd.exe".to_string(),
            "/d".to_string(),
            "/c".to_string(),
            self.script.display().to_string(),
        ]
    }
}

fn reset_full_owner_directory(root: &Path, path: &Path, owner: &str) {
    assert_plain_owned(root, path);
    assert!(
        fs::symlink_metadata(path)
            .expect("owned directory metadata")
            .is_dir()
    );
    unsafe {
        let sddl = crate::winutil::to_wide(format!("D:(A;OICI;FA;;;{owner})"));
        let mut descriptor = std::ptr::null_mut();
        assert_ne!(
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                /*SecurityDescriptorSize*/ std::ptr::null_mut()
            ),
            0
        );
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = std::ptr::null_mut();
        let valid = windows_sys::Win32::Security::GetSecurityDescriptorDacl(
            descriptor,
            &mut present,
            &mut acl,
            &mut defaulted,
        );
        if valid == 0 || present == 0 || acl.is_null() {
            LocalFree(descriptor as HLOCAL);
            panic!("owned directory ownerFA DACL must be present and non-null");
        }
        let mut name = crate::winutil::to_wide(path);
        let code = windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW(
            name.as_mut_ptr(),
            windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            /*psidOwner*/ std::ptr::null_mut(),
            /*psidGroup*/ std::ptr::null_mut(),
            acl,
            /*pSacl*/ std::ptr::null_mut(),
        );
        LocalFree(descriptor as HLOCAL);
        assert_eq!(
            code,
            windows_sys::Win32::Foundation::ERROR_SUCCESS,
            "reset only fixed owned directory DACL"
        );
    }
}

struct SampleResult {
    executed: bool,
    compatibility: bool,
    statuses: bool,
    security: bool,
    all_five_deleted: bool,
}

async fn record_flag_sample(
    fixture: &FlagFixture,
    label: &str,
    spawned: anyhow::Result<codex_utils_pty::SpawnedProcess>,
    unrestricted: bool,
) -> SampleResult {
    let (executed, stdout, stderr, exit_code) = match spawned {
        Ok(spawned) => {
            let (stdout, stderr, exit_code) = collect_diagnostic(spawned).await;
            (true, stdout, stderr, exit_code)
        }
        Err(error) => {
            // No spawn error is a recoverable sample: do not reset/reuse paths
            // or gather a subsequent arm after failed experimental setup.
            panic!("LEGACY_TOKEN_FLAGS invalid_setup sample={label} spawn_error={error:#}");
        }
    };
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
        executed,
        compatibility,
        statuses,
        security,
        all_five_deleted,
    }
}

#[test]
fn legacy_delete_diagnostic_actual_child_token_flag_pair() {
    let _guard = legacy_process_test_guard();
    current_thread_runtime().block_on(async {
        let owned = TempDir::new_in(sandbox_cwd()).expect("new private flag experiment root");
        let fixture = FlagFixture::new(owned.path());
        let home = sandbox_home("legacy-token-flags-pair");
        let token = unsafe { OwnedHandle::from_raw_handle(crate::token::get_current_token_for_restriction().expect("current token").cast()) };
        let user = unsafe { crate::token::get_user_sid_bytes(token.as_raw_handle()).expect("current owner") };
        let owner = crate::winutil::string_from_sid_bytes(&user).expect("owned fixture owner text");
        fixture.reset(&owner, &user);
        let prelaunch = capture_acls(&fixture.root, &fixture.objects()).expect("ordinary prelaunch owned ACLs");
        let ordinary = codex_utils_pty::spawn_pipe_process_no_stdin(
            "C:\\Windows\\System32\\cmd.exe", &fixture.command()[1..], &fixture.workspace, &fixture.env(),
            /*arg0*/ &None, /*inherited_fds*/ &[],
        ).await;
        let ordinary = record_flag_sample(&fixture, "ordinary-full-owner", ordinary, /*unrestricted*/ true).await;
        let mut observations = Vec::new();
        let mut samples = Vec::new();
        // Exactly two restricted samples. Do not short-circuit security failures
        // in the known-failing default arm before gathering the paired evidence.
        for mode in [FlagMode::Default, FlagMode::WithoutWriteRestricted] {
            fixture.reset(&owner, &user);
            let reset = capture_acls(&fixture.root, &fixture.objects()).expect("reset owned ACLs");
            assert!(prelaunch == reset, "all owned prelaunch directory/file DACLs must reset identically");
            let observer = Arc::new(Observation::new(
                mode, fixture.root.clone(), home.path().to_path_buf(), fixture.workspace.clone(),
                vec![fixture.workspace.clone(), fixture.temp.clone(), fixture.tmp.clone()], fixture.objects(),
            ));
            let spawned = crate::unified_exec::backends::legacy::spawn_windows_sandbox_session_legacy(
                &PermissionProfile::workspace_write(), &workspace_roots_for(&fixture.workspace), home.path(),
                fixture.command(), &fixture.workspace, fixture.env(), /*timeout_ms*/ Some(5_000),
                /*additional_deny_read_paths*/ &[], /*additional_deny_write_paths*/ &[],
                /*tty*/ false, /*stdin_open*/ false, /*private_desktop_name*/ None, Some(Arc::clone(&observer)),
            ).await;
            if let Err(error) = &spawned {
                if error.downcast_ref::<crate::token::flag_experiment::CleanupFailure>().is_some() {
                    // The exact child may still be live. Retain BOTH private
                    // directories, including on unwind, with no further ACL,
                    // file or capability mutation. ROOT owns the failed receipt.
                    let _retained_fixture = owned.keep();
                    let _retained_home = home.keep();
                    panic!("LEGACY_TOKEN_FLAGS cleanup_uncertain experiment_aborted=true fixture_preserved=true error={error:#}");
                }
            }
            samples.push(record_flag_sample(&fixture, &format!("restricted-input-{:#04x}", mode.input_flags()), spawned, /*unrestricted*/ false).await);
            let result = observer.result.lock().expect("observation lock").take();
            observations.push(result);
        }
        let pair = match (&observations[0], &observations[1]) {
            (Some(first), Some(second)) => {
                let token_equal = first.token == second.token;
                let roots_equal = first.roots == second.roots;
                let record_equal = first.record == second.record;
                let mut final_record = Vec::new();
                fs::File::open(crate::cap::cap_sid_file(home.path())).expect("existing private capability record after both samples").take(65537).read_to_end(&mut final_record).expect("bounded final private record read");
                assert!(final_record.len() <= 65536, "bounded final private record");
                let record_unchanged_after_samples = final_record == first.record;
                let postprep_acls_equal = first.acls == second.acls;
                eprintln!("LEGACY_TOKEN_FLAGS pair actual_sid_vectors_and_attributes_equal={token_equal} actual_root_map_equal={roots_equal} actual_store_bytes_equal={record_equal} actual_store_unchanged_after_samples={record_unchanged_after_samples} postprep_owned_acls_equal={postprep_acls_equal}");
                token_equal && roots_equal && record_equal && record_unchanged_after_samples && postprep_acls_equal
            },
            _ => false,
        };
        let default_reproduced_prior_full_owner_deletions = samples[0].executed && samples[0].compatibility && samples[0].statuses && samples[0].all_five_deleted;
        eprintln!("LEGACY_TOKEN_FLAGS default_reproduced_prior_full_owner_deletions={default_reproduced_prior_full_owner_deletions} pair_inputs_equal={pair} causal_interpretation_requires_both=true");
        assert!(ordinary.executed && ordinary.compatibility && ordinary.statuses && ordinary.security, "ordinary full-owner positive failed");
        assert!(default_reproduced_prior_full_owner_deletions, "invalid_seam_reproduction: default observer arm must reproduce prior full-owner deletions with execution/read positives and command-status observations before interpreting the flag pair");
        assert!(pair, "flag pair is not isolated; missing metadata or unequal token/root/ACL inputs earns no causal credit");
        for sample in samples {
            assert!(sample.executed && sample.compatibility && sample.statuses && sample.security, "restricted sample must satisfy execution/read positives and original six security expectations; no production repair is asserted");
        }
    });
}
