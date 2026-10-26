//! Native descriptor isolation, socket stdio, and launch errors.

use super::*;
use pretty_assertions::assert_eq;
use std::fs;
use std::io::Seek;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn explicit_stdio_excludes_inheritable_descriptors_without_changing_parent()
-> anyhow::Result<()> {
    let file = fs::File::open("/dev/null")?;
    // SAFETY: Duplicate a live descriptor without CLOEXEC so the control inherits it.
    let raw = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 200) };
    assert!(raw >= 200);
    // SAFETY: fcntl returned a new descriptor owned by this test.
    let sentinel = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: The test owns this descriptor throughout both launches.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    let script = format!("if [ -e /dev/fd/{raw} ]; then printf open; else printf closed; fi");
    let mut control = Command::new("/bin/sh");
    control.args(["-c", &script]);
    let control = control.spawn()?.wait_with_output().await?;
    assert_eq!(
        (control.status.success(), control.stdout),
        (true, b"open".to_vec())
    );

    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", &script])
        .descriptor_policy(DescriptorPolicy::Explicit);
    let output = command.spawn()?.wait_with_output().await?;
    assert_eq!(
        (output.status.success(), output.stdout, output.stderr),
        (true, b"closed".to_vec(), vec![])
    );
    // SAFETY: Spawning must not mutate the parent's descriptor flags.
    assert_eq!(
        unsafe { libc::fcntl(sentinel.as_raw_fd(), libc::F_GETFD) },
        flags
    );
    Ok(())
}

#[tokio::test]
async fn socket_stdin_preserves_bidirectional_io_and_custom_argv0() -> anyhow::Result<()> {
    let (parent, socket) = UnixStream::pair()?;
    parent.set_nonblocking(true)?;
    let mut parent = tokio::net::UnixStream::from_std(parent)?;
    let mut command = Command::new("/bin/sh");
    command
        .args(["--norc", "-c", "IFS= read -r line; printf 'reply:%s\\n' \"$line\" >&0; printf '%s:%s:%s' \"$0\" \"$FS_TEST\" \"${HOME-unset}\"; printf diagnostic >&2; exit 19"])
        .arg0("helper")
        .env("FS_TEST", "literal ; $value")
        .stdin(ChildStdin::File(socket.into()))
        .descriptor_policy(DescriptorPolicy::Explicit)
        .fallback(SpawnFallback::ReturnError);
    let child = command.spawn()?;
    assert!(child.stdin.is_none());
    parent.write_all(b"socket\n").await?;
    parent.shutdown().await?;
    let mut reply = Vec::new();
    parent.read_to_end(&mut reply).await?;
    let output = child.wait_with_output().await?;
    assert_eq!(reply, b"reply:socket\n");
    assert_eq!(
        (output.status.code(), output.stdout, output.stderr),
        (
            Some(19),
            b"helper:literal ; $value:unset".to_vec(),
            b"diagnostic".to_vec()
        )
    );
    Ok(())
}

#[tokio::test]
async fn native_launch_can_reject_executable_text_without_shell_fallback() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let script = directory.path().join("no-shebang");
    codex_utils_cargo_bin::write_executable(&script, "printf unexpected-fallback\n")?;
    let mut command = Command::new(script);
    command.fallback(SpawnFallback::ReturnError);
    let error = command.spawn().err().expect("executable text must fail");
    assert_eq!(error.raw_os_error(), Some(libc::ENOEXEC));
    Ok(())
}

#[tokio::test]
async fn inherited_descriptors_survive_native_and_executable_text_spawns() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let data = directory.path().join("data");
    fs::write(&data, "captured descriptor")?;
    let mut file = fs::File::open(&data)?;
    for native in [true, false] {
        file.rewind()?;
        // SAFETY: Duplicate this live file into a new inheritable descriptor.
        let raw = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 100) };
        assert!(raw >= 100);
        // SAFETY: fcntl returned a newly owned descriptor.
        let _original = unsafe { OwnedFd::from_raw_fd(raw) };
        let descriptor = format!("/dev/fd/{raw}");
        let mut command = if native {
            let mut command = Command::new("/bin/cat");
            command.arg(&descriptor);
            command
        } else {
            let script = directory.path().join("executable-text");
            codex_utils_cargo_bin::write_executable(&script, &format!("/bin/cat {descriptor}\n"))?;
            Command::new(script)
        };
        command
            .descriptor_policy(DescriptorPolicy::Explicit)
            .preserve_fds(&[raw]);
        let child = command.spawn()?;
        assert_eq!(
            matches!(child.inner, crate::child::ChildKind::Native(_)),
            native
        );
        let output = child.wait_with_output().await?;
        assert_eq!(
            (output.status.code(), output.stdout, output.stderr),
            (Some(0), b"captured descriptor".to_vec(), vec![])
        );
    }
    Ok(())
}

#[tokio::test]
async fn high_stdio_descriptors_use_compatible_fallback() -> anyhow::Result<()> {
    if std::env::var_os("CODEX_TEST_HIGH_STDIO_FDS").is_none() {
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "child_command::descriptor_tests::high_stdio_descriptors_use_compatible_fallback",
                "--nocapture",
            ])
            .env("CODEX_TEST_HIGH_STDIO_FDS", "1")
            .output()?;
        let stderr = &output.stderr[..output.stderr.len().min(8 * 1024)];
        eprintln!(
            "high-fd isolated child status={} stderr (at most 8192 bytes):\n{}",
            output.status,
            String::from_utf8_lossy(stderr)
        );
        assert!(output.status.success(), "{output:?}");
        return Ok(());
    }

    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: getrlimit initializes writable storage. Only this isolated test
    // subprocess raises its soft limit; the test runner and host are unchanged.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) },
        0,
        "high-fd phase=original-limit errno={:?}",
        io::Error::last_os_error().raw_os_error()
    );
    let mut limit = unsafe { limit.assume_init() };
    eprintln!(
        "high-fd phase=original-limit pid={} soft={} hard={}",
        std::process::id(),
        limit.rlim_cur,
        limit.rlim_max
    );
    limit.rlim_cur = limit.rlim_cur.max(16_384);
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) },
        0,
        "high-fd phase=set-limit requested_soft={} requested_hard={} errno={:?}",
        limit.rlim_cur,
        limit.rlim_max,
        io::Error::last_os_error().raw_os_error()
    );
    let mut readback = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: getrlimit initializes writable storage without changing limits.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, readback.as_mut_ptr()) },
        0,
        "high-fd phase=limit-readback errno={:?}",
        io::Error::last_os_error().raw_os_error()
    );
    let readback = unsafe { readback.assume_init() };
    eprintln!(
        "high-fd phase=limit-readback requested_soft={} requested_hard={} soft={} hard={}",
        limit.rlim_cur, limit.rlim_max, readback.rlim_cur, readback.rlim_max
    );
    // This fixed, read-only query finishes before descriptor exhaustion. Its
    // availability is diagnostic only and does not affect the test assertions.
    match std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.maxfiles", "kern.maxfilesperproc"])
        .output()
    {
        Ok(output) => {
            let values = std::str::from_utf8(&output.stdout)
                .ok()
                .filter(|_| output.stdout.len() <= 128)
                .and_then(|text| {
                    let mut lines = text.split_whitespace();
                    let maxfiles = lines.next()?.parse::<u64>().ok()?;
                    let maxfilesperproc = lines.next()?.parse::<u64>().ok()?;
                    lines
                        .next()
                        .is_none()
                        .then_some((maxfiles, maxfilesperproc))
                });
            if output.status.success()
                && let Some((maxfiles, maxfilesperproc)) = values
            {
                eprintln!(
                    "high-fd phase=kernel-limits kern.maxfiles={maxfiles} kern.maxfilesperproc={maxfilesperproc} status={}",
                    output.status
                );
            } else {
                eprintln!(
                    "high-fd phase=kernel-limits unavailable status={} stdout_bytes={} stderr_bytes={} numeric_pair={}",
                    output.status,
                    output.stdout.len(),
                    output.stderr.len(),
                    values.is_some()
                );
            }
        }
        Err(error) => eprintln!(
            "high-fd phase=kernel-limits unavailable errno={:?} error={error}",
            error.raw_os_error()
        ),
    }
    let diagnostic = |phase: &str, count: usize, highest: i32, error: io::Error| {
        let context = format!(
            "high-fd phase={phase} allocated={count} highest_fd={highest} soft={} hard={} errno={:?}",
            readback.rlim_cur,
            readback.rlim_max,
            error.raw_os_error()
        );
        anyhow::Error::new(error).context(context)
    };

    let file =
        fs::File::open("/dev/null").map_err(|error| diagnostic("open-dev-null", 0, -1, error))?;
    // SAFETY: Create a new inheritable descriptor to check fallback isolation.
    let raw = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 3) };
    anyhow::ensure!(
        raw >= 3,
        "{}",
        diagnostic(
            "sentinel-dup",
            0,
            file.as_raw_fd(),
            io::Error::last_os_error()
        )
    );
    let sentinel = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut occupied = Vec::new();
    let mut highest = file.as_raw_fd().max(sentinel.as_raw_fd());
    eprintln!("high-fd phase=before-allocation allocated=0 highest_fd={highest}");
    loop {
        let descriptor = file
            .try_clone()
            .map_err(|error| diagnostic("allocate", occupied.len(), highest, error))?;
        let raw = descriptor.as_raw_fd();
        highest = highest.max(raw);
        occupied.push(descriptor);
        // Darwin's spawn file actions reject valid descriptors >= OPEN_MAX
        // (10240), even when RLIMIT_NOFILE allows the descriptor to be opened.
        if raw >= 10_240 {
            break;
        }
    }

    let mut strict = Command::new("/usr/bin/true");
    strict.fallback(SpawnFallback::ReturnError);
    eprintln!(
        "high-fd phase=before-strict-spawn allocated={} highest_fd={highest}",
        occupied.len()
    );
    let error = strict
        .spawn()
        .err()
        .expect("native high-fd action must fail");
    eprintln!(
        "high-fd phase=strict-spawn-result allocated={} highest_fd={highest} errno={:?}",
        occupied.len(),
        error.raw_os_error()
    );
    assert_eq!(error.raw_os_error(), Some(libc::EBADF));

    for mode in [ProcessMode::NewGroup, ProcessMode::NewSession] {
        let mode_name = match mode {
            ProcessMode::NewGroup => "new-group",
            ProcessMode::NewSession => "new-session",
            ProcessMode::Inherit => unreachable!("only isolated process modes are tested"),
        };
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", &format!(
                "test ! -e /dev/fd/{} || exit 99; IFS= read -r line; printf '%s' \"$line\"; printf diagnostic >&2; exit 23",
                sentinel.as_raw_fd()
            )])
            .descriptor_policy(DescriptorPolicy::Explicit)
            .process_mode(mode);
        eprintln!(
            "high-fd phase=before-fallback-spawn mode={mode_name} allocated={} highest_fd={highest}",
            occupied.len()
        );
        let mut child = command.spawn().map_err(|error| {
            diagnostic(
                &format!("fallback-spawn-{mode_name}"),
                occupied.len(),
                highest,
                error,
            )
        })?;
        assert!(matches!(child.inner, ChildKind::Tokio(_)));
        let pid = child.id().expect("live child") as libc::pid_t;
        // SAFETY: The child waits for input and still owns this PID.
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(b"round trip\n")
            .await
            .map_err(|error| {
                diagnostic(
                    &format!("fallback-write-{mode_name}"),
                    occupied.len(),
                    highest,
                    error,
                )
            })?;
        let output = child.wait_with_output().await.map_err(|error| {
            diagnostic(
                &format!("fallback-wait-{mode_name}"),
                occupied.len(),
                highest,
                error,
            )
        })?;
        assert_eq!(
            (output.status.code(), output.stdout, output.stderr),
            (Some(23), b"round trip".to_vec(), b"diagnostic".to_vec())
        );
    }
    // SAFETY: Child setup must not close or change the parent's descriptor.
    assert_eq!(
        unsafe { libc::fcntl(sentinel.as_raw_fd(), libc::F_GETFD) },
        0
    );
    Ok(())
}
