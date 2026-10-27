//! Test-only host-DACL controls; every mutation and command target is newly owned.

use super::*;
use std::os::windows::fs::MetadataExt;
use std::os::windows::fs::OpenOptionsExt;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::ACL_SIZE_INFORMATION;
use windows_sys::Win32::Security::AclSizeInformation;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::GetAclInformation;
use windows_sys::Win32::Security::GetSecurityDescriptorControl;
use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::SetFileSecurityW;
use windows_sys::Win32::Security::UNPROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;

#[derive(Clone, Copy, Debug)]
enum HostGrant {
    OwnerOnly,
    EveryoneOutsideDelete,
    EveryoneOutsideParentDeleteChild,
    LogonOutsideDelete,
    EveryoneWorkspaceDeleteChild,
}

#[test]
fn legacy_delete_diagnostic_owner_only_control() {
    run_diagnostic(HostGrant::OwnerOnly);
}

#[test]
fn legacy_delete_diagnostic_everyone_outside_delete() {
    run_diagnostic(HostGrant::EveryoneOutsideDelete);
}

#[test]
fn legacy_delete_diagnostic_everyone_outside_parent_delete_child() {
    run_diagnostic(HostGrant::EveryoneOutsideParentDeleteChild);
}

#[test]
fn legacy_delete_diagnostic_logon_outside_delete() {
    run_diagnostic(HostGrant::LogonOutsideDelete);
}

#[test]
fn legacy_delete_diagnostic_everyone_workspace_delete_child() {
    run_diagnostic(HostGrant::EveryoneWorkspaceDeleteChild);
}

/// Check existing ancestors too: a lexical prefix alone cannot confine ACL work.
fn assert_plain_owned(root: &Path, path: &Path) {
    // Inspect original spelling before canonicalization can erase a reparse
    // component, including existing ancestors outside the new private root.
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).expect("inspect existing ancestor");
        assert_eq!(metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT, 0);
    }
    let root = root.canonicalize().expect("canonical private fixture root");
    let path = path
        .canonicalize()
        .expect("canonical existing owned object");
    assert!(path.starts_with(&root), "object outside private fixture");
}

fn set_owner_only(root: &Path, path: &Path, owner: &str) {
    assert_plain_owned(root, path);
    // Protect only the new top-level root from ambient host inheritance. Child
    // inheritance must stay enabled so real production workspace/TEMP/TMP
    // capability grants can propagate and permit the positive deletions.
    let protection =
        if path.canonicalize().expect("owned path") == root.canonicalize().expect("owned root") {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
    // The original owner retains read/control/recovery rights in every case.
    let sddl = crate::winutil::to_wide(format!("D:(A;OICI;FA;;;{owner})"));
    unsafe {
        let mut descriptor = std::ptr::null_mut();
        assert_ne!(
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                /*SecurityDescriptorSize*/ std::ptr::null_mut(),
            ),
            0,
            "create private owner-only descriptor: {}",
            std::io::Error::last_os_error()
        );
        let result = SetFileSecurityW(
            crate::winutil::to_wide(path).as_ptr(),
            DACL_SECURITY_INFORMATION | protection,
            descriptor,
        );
        let error = std::io::Error::last_os_error();
        LocalFree(descriptor as HLOCAL);
        assert_ne!(result, 0, "set private owner-only descriptor: {error}");
    }
}

fn grant_owned(root: &Path, path: &Path, sid: &mut [u8], mask: u32) {
    assert_plain_owned(root, path);
    // No inheritance and no ancestor mutation: this is one adversarial host ACE.
    unsafe {
        crate::acl::ensure_allow_mask_aces_with_inheritance(
            path,
            &[sid.as_mut_ptr().cast()],
            mask,
            /*inheritance*/ 0,
        )
        .expect("add explicit owned-fixture host grant");
    }
}

fn describe_io<T: std::fmt::Debug>(result: std::io::Result<T>) -> String {
    match result {
        Ok(value) => format!("Ok({value:?})"),
        Err(error) => format!(
            "Err(kind={:?}, raw={:?})",
            error.kind(),
            error.raw_os_error()
        ),
    }
}

fn object_observation(path: &Path) -> String {
    let metadata = describe_io(
        fs::symlink_metadata(path)
            .map(|value| (value.is_dir(), value.len(), value.file_attributes())),
    );
    let read = describe_io(fs::read_to_string(path));
    let entries = path.parent().map(|parent| {
        describe_io(fs::read_dir(parent).and_then(|entries| {
            let entries: Vec<_> = entries
                .take(17)
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<std::io::Result<_>>()?;
            assert!(
                entries.len() <= 16,
                "unexpected private-fixture directory size"
            );
            Ok(entries)
        }))
    });
    let identity = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .and_then(|file| {
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok((
                info.dwVolumeSerialNumber,
                info.nFileIndexHigh,
                info.nFileIndexLow,
                info.nNumberOfLinks,
            ))
        });
    format!(
        "metadata={metadata} read={read} parent_entries={entries:?} handle={}",
        describe_io(identity)
    )
}

fn acl_observation(path: &Path) -> String {
    unsafe {
        let (acl, descriptor) = match crate::acl::fetch_dacl_handle(path) {
            Ok(value) => value,
            Err(error) => return format!("acl_error={error:#}"),
        };
        let result = (|| {
            let mut control = 0;
            let mut revision = 0;
            if GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) == 0 {
                return format!("control_error={}", std::io::Error::last_os_error());
            }
            if acl.is_null() {
                return format!("control={control} null_dacl=true");
            }
            let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
            if GetAclInformation(
                acl,
                (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            ) == 0
            {
                return format!("acl_info_error={}", std::io::Error::last_os_error());
            }
            assert!(info.AclBytesInUse <= 65535, "invalid ACL size");
            let bytes = std::slice::from_raw_parts(acl.cast::<u8>(), info.AclBytesInUse as usize);
            format!(
                "control={control} ace_count={} dacl={bytes:02x?}",
                info.AceCount
            )
        })();
        LocalFree(descriptor as HLOCAL);
        result
    }
}

async fn drain_bounded(mut receiver: mpsc::Receiver<Vec<u8>>) -> (Vec<u8>, usize) {
    let mut saved = Vec::new();
    let mut total = 0usize;
    while let Some(chunk) = receiver.recv().await {
        total = total.saturating_add(chunk.len());
        let keep = chunk.len().min(65536usize.saturating_sub(saved.len()));
        saved.extend_from_slice(&chunk[..keep]);
    }
    (saved, total)
}

async fn collect_diagnostic(spawned: codex_utils_pty::SpawnedProcess) -> (Vec<u8>, Vec<u8>, i32) {
    let codex_utils_pty::SpawnedProcess {
        session: _session,
        stdout_rx,
        stderr_rx,
        exit_rx,
    } = spawned;
    // Drain independently even after the retention cap, avoiding pipe backpressure.
    let stdout = tokio::spawn(drain_bounded(stdout_rx));
    let stderr = tokio::spawn(drain_bounded(stderr_rx));
    let (exit, stdout, stderr) = timeout(Duration::from_secs(/*secs*/ 10), async {
        let (exit, stdout, stderr) = tokio::join!(exit_rx, stdout, stderr);
        (
            exit.expect("diagnostic exit receiver"),
            stdout.expect("stdout collector"),
            stderr.expect("stderr collector"),
        )
    })
    .await
    .expect("bounded legacy diagnostic completion");
    assert!(
        stdout.1 <= 65536 && stderr.1 <= 65536,
        "diagnostic stream cap exceeded"
    );
    (stdout.0, stderr.0, exit)
}

fn run_diagnostic(grant: HostGrant) {
    let _guard = legacy_process_test_guard();
    current_thread_runtime().block_on(async move {
        let test_root = TempDir::new_in(sandbox_cwd()).expect("new private diagnostic root");
        let root = test_root.path();
        assert_plain_owned(root, root);
        let codex_home = sandbox_home("legacy-delete-diagnostic");
        let workspace = root.join("workspace");
        let temp_root = root.join("temp");
        let tmp_root = root.join("tmp");
        let outside_root = root.join("outside");
        for path in [&workspace, &temp_root, &tmp_root, &outside_root] {
            fs::create_dir(path).expect("new private fixture directory");
        }
        let git = workspace.join(".git");
        fs::create_dir(&git).expect("new protected fixture directory");
        let workspace_file = workspace.join("workspace-delete.txt");
        let temp_file = temp_root.join("temp-delete.txt");
        let tmp_file = tmp_root.join("tmp-delete.txt");
        let outside_file = outside_root.join("outside-delete.txt");
        for (path, contents) in [(&workspace_file, "workspace"), (&temp_file, "temp"), (&tmp_file, "tmp"), (&outside_file, "outside")] {
            fs::write(path, contents).expect("seed new private fixture");
        }
        let script = workspace.join("delete-fixtures.cmd");
        fs::write(&script, concat!(
            "@echo off\r\n",
            "del /f /q \"%WORKSPACE_DELETE%\"\r\necho WORKSPACE_STATUS=%errorlevel%\r\n",
            "del /f /q \"%TEMP_DELETE%\"\r\necho TEMP_STATUS=%errorlevel%\r\n",
            "del /f /q \"%TMP_DELETE%\"\r\necho TMP_STATUS=%errorlevel%\r\n",
            "del /f /q \"%OUTSIDE_DELETE%\"\r\necho OUTSIDE_STATUS=%errorlevel%\r\n",
            "rmdir \"%PROTECTED_GIT_DIR%\"\r\necho GIT_STATUS=%errorlevel%\r\n",
            "exit /b 0\r\n",
        )).expect("write owned diagnostic script");
        let token = unsafe {
            OwnedHandle::from_raw_handle(crate::token::get_current_token_for_restriction().expect("current token").cast())
        };
        let user = unsafe { crate::token::get_user_sid_bytes(token.as_raw_handle() as _).expect("current owner SID") };
        let mut logon = unsafe { crate::token::get_logon_sid_bytes(token.as_raw_handle() as _).expect("actual logon SID") };
        let mut everyone = unsafe { crate::token::world_sid().expect("Everyone SID") };
        let owner = crate::winutil::string_from_sid_bytes(&user).expect("owner SID text");
        eprintln!(
            "LEGACY_DELETE_DIAGNOSTIC case={grant:?} base_owner={owner} base_logon={}",
            crate::winutil::string_from_sid_bytes(&logon).expect("logon SID text")
        );
        let objects = [root, &workspace, &temp_root, &tmp_root, &outside_root, &git, &workspace_file, &temp_file, &tmp_file, &outside_file, &script];
        for path in objects {
            set_owner_only(root, path, &owner);
        }
        match grant {
            HostGrant::OwnerOnly => {},
            HostGrant::EveryoneOutsideDelete => grant_owned(root, &outside_file, &mut everyone, DELETE),
            HostGrant::EveryoneOutsideParentDeleteChild => grant_owned(root, &outside_root, &mut everyone, FILE_DELETE_CHILD),
            HostGrant::LogonOutsideDelete => grant_owned(root, &outside_file, &mut logon, DELETE),
            HostGrant::EveryoneWorkspaceDeleteChild => grant_owned(root, &workspace, &mut everyone, FILE_DELETE_CHILD),
        }
        for path in objects {
            eprintln!("LEGACY_DELETE_DIAGNOSTIC before case={grant:?} path={path:?} {} {}", object_observation(path), acl_observation(path));
        }
        let env_map = HashMap::from([
            ("TEMP".to_string(), temp_root.to_string_lossy().into_owned()),
            ("TMP".to_string(), tmp_root.to_string_lossy().into_owned()),
            ("WORKSPACE_DELETE".to_string(), workspace_file.to_string_lossy().into_owned()),
            ("TEMP_DELETE".to_string(), temp_file.to_string_lossy().into_owned()),
            ("TMP_DELETE".to_string(), tmp_file.to_string_lossy().into_owned()),
            ("OUTSIDE_DELETE".to_string(), outside_file.to_string_lossy().into_owned()),
            ("PROTECTED_GIT_DIR".to_string(), git.to_string_lossy().into_owned()),
        ]);
        let spawned = spawn_windows_sandbox_session_legacy(
            &PermissionProfile::workspace_write(),
            &workspace_roots_for(&workspace),
            codex_home.path(),
            vec!["C:\\Windows\\System32\\cmd.exe".to_string(), "/d".to_string(), "/c".to_string(), script.display().to_string()],
            &workspace,
            env_map,
            /*timeout_ms*/ Some(5_000),
            /*additional_deny_write_paths*/ &[],
            /*additional_deny_read_paths*/ &[],
            /*tty*/ false,
            /*stdin_open*/ false,
        ).await.expect("actual legacy diagnostic session");
        let (stdout, stderr, exit_code) = collect_diagnostic(spawned).await;
        for path in objects {
            eprintln!("LEGACY_DELETE_DIAGNOSTIC after case={grant:?} path={path:?} {} {}", object_observation(path), acl_observation(path));
        }
        let stdout = String::from_utf8_lossy(&stdout);
        let stderr = String::from_utf8_lossy(&stderr);
        eprintln!("LEGACY_DELETE_DIAGNOSTIC case={grant:?} exit={exit_code} stdout={stdout:?} stderr={stderr:?}");
        for marker in ["WORKSPACE_STATUS=", "TEMP_STATUS=", "TMP_STATUS=", "OUTSIDE_STATUS=", "GIT_STATUS="] {
            assert!(stdout.contains(marker), "missing command observation {marker}");
        }
        // Keep the original six security postconditions, with concrete host
        // observations above so None/false can never be misreported as deletion.
        assert_eq!((exit_code, workspace_file.exists(), temp_file.exists(), tmp_file.exists(), fs::read_to_string(&outside_file).ok(), git.is_dir()),
            (0, false, false, false, Some("outside".to_string()), true),
            "case={grant:?} stdout={stdout:?} stderr={stderr:?}\n{}", sandbox_log(codex_home.path()));
    });
}
