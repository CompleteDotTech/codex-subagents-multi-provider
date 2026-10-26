//! Writable PATH candidates must never run while constructing the sandbox.

#![cfg(target_os = "linux")]

use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cargo_bin::cargo_bin;
use codex_utils_cargo_bin::write_executable;
use pretty_assertions::assert_eq;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn denied_dev_fd_starts_with_standard_devices_and_stdio_intact() {
    use codex_protocol::permissions::NetworkSandboxPolicy;
    use codex_protocol::protocol::FileSystemAccessMode;
    use codex_protocol::protocol::FileSystemPath;
    use codex_protocol::protocol::FileSystemSandboxEntry;
    use codex_utils_cargo_bin::copy_executable;
    use std::io::Write;
    use std::process::Stdio;

    let package = tempfile::tempdir().expect("package");
    let sandbox = package.path().join("codex-linux-sandbox");
    copy_executable(&cargo_bin("codex-linux-sandbox").unwrap(), &sandbox).unwrap();
    let resources = package.path().join("codex-resources");
    std::fs::create_dir(&resources).unwrap();
    copy_executable(
        &cargo_bin("bwrap").expect("build bundled bwrap for this test"),
        &resources.join("bwrap"),
    )
    .unwrap();
    let fake_bin = package.path().join("old-system");
    std::fs::create_dir(&fake_bin).unwrap();
    write_executable(&fake_bin.join("bwrap"), "#!/bin/sh\nif [ \"$1\" = --help ]; then printf '%s\\n' '--as-pid-1 --perms'; else exit 92; fi\n").unwrap();
    let old_system_path = std::env::join_paths([
        fake_bin.as_path(),
        std::path::Path::new("/usr/bin"),
        std::path::Path::new("/bin"),
    ])
    .unwrap();
    let mut denied_profile = String::new();
    let common = r#"
        for node in null zero full random urandom tty; do test -c /dev/$node || exit 31; done
        test -c /dev/pts/ptmx || exit 32
        test -L /dev/stdin && test -L /dev/stdout && test -L /dev/stderr || exit 33
        read line < /dev/stdin
        test "$line" = input || exit 34
    "#;
    for denied in [false, true] {
        let mut policy = PermissionProfile::read_only().file_system_sandbox_policy();
        if denied {
            policy.entries.push(FileSystemSandboxEntry::new(
                FileSystemPath::Path {
                    path: AbsolutePathBuf::from_absolute_path("/dev/fd")
                        .unwrap()
                        .into(),
                },
                FileSystemAccessMode::Deny,
            ));
        }
        let profile =
            PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Enabled);
        let profile = serde_json::to_string(&profile).unwrap();
        if denied {
            denied_profile = profile.clone();
        }
        let check = if denied {
            r#"test -d /dev/fd && test ! -L /dev/fd || exit 35
               test "$(stat -c %a /dev/fd)" = 0 || exit 36
               if ls /dev/fd >/dev/null 2>&1; then exit 37; fi
               if cat /dev/fd/0 >/dev/null 2>&1; then exit 38; fi"#
        } else {
            "test -L /dev/fd && test -d /dev/fd || exit 39"
        };
        let mut command = Command::new(&sandbox);
        if denied {
            command.env("PATH", &old_system_path);
        }
        let mut child = command
            .args([
                "--sandbox-policy-cwd",
                "/",
                "--command-cwd",
                "/",
                "--permission-profile",
            ])
            .arg(&profile)
            .args([
                "--",
                "/bin/sh",
                "-c",
                &format!("{common}\n{check}\nprintf started"),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start sandbox");
        child.stdin.take().unwrap().write_all(b"input\n").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "denied={denied}: {output:?}");
        assert_eq!(output.stdout, b"started");
    }
    std::fs::remove_file(resources.join("bwrap")).unwrap();
    let absent = Command::new(&sandbox)
        .args([
            "--sandbox-policy-cwd",
            "/",
            "--permission-profile",
            &denied_profile,
            "--",
            "/bin/true",
        ])
        .env("PATH", &old_system_path)
        .output()
        .unwrap();
    assert!(
        !absent.status.success(),
        "must fail closed without a capable helper"
    );
    assert!(
        String::from_utf8_lossy(&absent.stderr)
            .contains("no trusted bundled bubblewrap is available"),
        "{absent:?}"
    );
}

#[test]
fn child_workdir_does_not_probe_or_launch_writable_parent_bwrap() {
    // Bazel can mount the default temp directory noexec.
    let temp = tempfile::tempdir_in(std::env::current_dir().expect("current directory"))
        .or_else(|_| tempfile::tempdir())
        .expect("create fixture");
    let workspace = temp.path().join("workspace");
    let cwd = workspace.join("service");
    let bin = workspace.join("bin");
    let marker = temp.path().join("outside-workspace");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    std::fs::create_dir_all(&bin).expect("create bin");
    let fake_bwrap = bin.join("bwrap");
    std::fs::write(
        &fake_bwrap,
        "#!/bin/sh\n: > \"$BWRAP_PROBE_MARKER\"\nprintf '%s\\n' '--as-pid-1 --perms --argv0 --ro-bind-fd'\n",
    )
    .expect("write fake bwrap");
    std::fs::set_permissions(&fake_bwrap, std::fs::Permissions::from_mode(0o755))
        .expect("make fake bwrap executable");
    let policy = PermissionProfile::read_only()
        .file_system_sandbox_policy()
        .with_additional_writable_roots(
            &workspace,
            &[AbsolutePathBuf::try_from(workspace.clone()).expect("absolute workspace")],
        );
    let profile =
        PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Restricted);
    let profile_json = serde_json::to_string(&profile).expect("serialize profile");
    let trusted_path = std::env::var_os("PATH").expect("PATH is set");
    let run = |search_path: &std::ffi::OsStr, script: &str| {
        Command::new(cargo_bin("codex-linux-sandbox").expect("resolve sandbox binary"))
            .arg("--sandbox-policy-cwd")
            .arg(&workspace)
            .arg("--command-cwd")
            .arg(&cwd)
            .arg("--permission-profile")
            .arg(&profile_json)
            .args(["--", "/bin/sh", "-c", script])
            .current_dir(&cwd)
            .env("PATH", search_path)
            .env("BWRAP_PROBE_MARKER", &marker)
            .output()
            .expect("run sandbox")
    };
    let baseline = run(&trusted_path, "printf sandboxed");
    let stderr = String::from_utf8_lossy(&baseline.stderr);
    if !baseline.status.success()
        && [
            "bubblewrap is unavailable",
            "No permissions to create a new namespace",
            "setting up uid map: Permission denied",
        ]
        .iter()
        .any(|message| stderr.contains(message))
    {
        eprintln!(
            "sandbox unavailable: {}",
            String::from_utf8_lossy(&baseline.stderr)
        );
        return;
    }
    assert_eq!(baseline.status.success(), true, "{baseline:?}");
    let denied_write = run(&trusted_path, ": > \"$BWRAP_PROBE_MARKER\"");
    assert_eq!(denied_write.status.success(), false, "{denied_write:?}");
    assert!(
        !marker.exists(),
        "sandbox must deny the outside-workspace write"
    );
    let hostile_path = std::env::join_paths(
        [bin]
            .into_iter()
            .chain(std::env::split_paths(&trusted_path)),
    )
    .expect("join PATH");
    let output = run(&hostile_path, "printf sandboxed");
    assert!(!marker.exists(), "writable bwrap ran before confinement");
    assert_eq!(output.status.success(), true, "{output:?}");
    assert_eq!(output.stdout, b"sandboxed");
}

#[test]
fn full_disk_write_with_managed_network_does_not_probe_writable_bwrap() {
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let bin = temp.path().join("bin");
    let cwd = temp.path().join("cwd");
    std::fs::create_dir(&bin).unwrap();
    std::fs::create_dir(&cwd).unwrap();
    let bwrap = bin.join("bwrap");
    write_executable(&bwrap, "#!/bin/sh\n: > \"$BWRAP_PROBE_MARKER\"\nexit 1\n").unwrap();
    let run = |profile: &PermissionProfile, marker: &std::path::Path| {
        Command::new(cargo_bin("codex-linux-sandbox").expect("resolve sandbox binary"))
            .arg("--sandbox-policy-cwd")
            .arg(&cwd)
            .arg("--permission-profile")
            .arg(serde_json::to_string(profile).unwrap())
            .args(["--managed-network", "{}", "--", "/bin/true"])
            .current_dir(&cwd)
            .env_clear()
            .env("PATH", &bin)
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("BWRAP_PROBE_MARKER", marker)
            .output()
            .unwrap()
    };
    // Confirm this host reaches the capability probe without needing working
    // namespaces or bundled bubblewrap. Both invocations use managed networking.
    let control_marker = temp.path().join("control");
    let control = run(&PermissionProfile::read_only(), &control_marker);
    assert!(
        control_marker.exists(),
        "probe was not reached: {control:?}"
    );

    let marker = temp.path().join("unrestricted");
    let output = run(&PermissionProfile::Disabled, &marker);
    assert!(
        !marker.exists(),
        "writable bwrap ran before network confinement: {output:?}"
    );
}
