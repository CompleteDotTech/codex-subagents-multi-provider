use super::verify_fd_mounts;
use pretty_assertions::assert_eq;
use std::fs::File;
use std::os::fd::IntoRawFd;

/// A matching mount authenticates its original inode and closes the inherited descriptor.
#[test]
fn matching_mount_closes_inherited_descriptor() {
    in_fresh_fd_observer_with_output(
        "matching_mount_closes_inherited_descriptor",
        "original_matching_mount_closes_inherited_descriptor",
        false,
        || {
            let root = tempfile::tempdir().expect("temporary directory should be created");
            let descriptor = File::open(root.path())
                .expect("directory descriptor should open")
                .into_raw_fd();
            let marker = format!("{descriptor}:{}", root.path().display());

            let result = verify_fd_mounts(&[marker]);
            let closed_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            let closed_errno = std::io::Error::last_os_error().raw_os_error();

            result.expect("matching mount should verify");

            assert_eq!(closed_flags, -1);
            assert_eq!(closed_errno, Some(libc::EBADF));
        },
    );
}

/// A swapped destination fails closed without leaking the original descriptor.
#[test]
fn mismatched_mount_closes_inherited_descriptor() {
    in_fresh_fd_observer_with_output(
        "mismatched_mount_closes_inherited_descriptor",
        "original_mismatched_mount_closes_inherited_descriptor",
        false,
        || {
            let source = tempfile::tempdir().expect("source directory should be created");
            let destination = tempfile::tempdir().expect("destination directory should be created");
            let descriptor = File::open(source.path())
                .expect("source directory descriptor should open")
                .into_raw_fd();
            let marker = format!("{descriptor}:{}", destination.path().display());

            let result = verify_fd_mounts(&[marker]);
            let closed_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            let closed_errno = std::io::Error::last_os_error().raw_os_error();

            let error = result.expect_err("different inodes must be rejected");

            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(closed_flags, -1);
            assert_eq!(closed_errno, Some(libc::EBADF));
        },
    );
}

/// A symlink to the original inode is not itself the authenticated mount.
#[test]
fn symlinked_mount_destination_closes_inherited_descriptor() {
    in_fresh_fd_observer_with_output(
        "symlinked_mount_destination_closes_inherited_descriptor",
        "original_symlinked_mount_destination_closes_inherited_descriptor",
        false,
        || {
            let root = tempfile::tempdir().expect("temporary directory should be created");
            let source = root.path().join("source");
            let destination = root.path().join("destination");
            std::fs::create_dir(&source).expect("source directory should be created");
            std::os::unix::fs::symlink(&source, &destination)
                .expect("mount destination symlink should be created");
            let descriptor = File::open(&source)
                .expect("source directory descriptor should open")
                .into_raw_fd();
            let marker = format!("{descriptor}:{}", destination.display());

            let result = verify_fd_mounts(&[marker]);
            let closed_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            let closed_errno = std::io::Error::last_os_error().raw_os_error();

            let error = result.expect_err("symlinked mounts must be rejected");

            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(closed_flags, -1);
            assert_eq!(closed_errno, Some(libc::EBADF));
        },
    );
}

/// Malformed mount markers cannot claim standard streams or relative destinations.
#[test]
fn malformed_mount_markers_are_rejected() {
    in_fresh_fd_observer_with_output(
        "malformed_mount_markers_are_rejected",
        "original_malformed_mount_markers_are_rejected",
        false,
        || {
            for marker in [
                "missing-separator",
                "invalid:/tmp",
                "0:/tmp",
                "1:/tmp",
                "2:/tmp",
            ] {
                let error = verify_fd_mounts(&[marker.to_string()])
                    .expect_err("malformed mount marker must be rejected");
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            }

            let root = tempfile::tempdir().expect("temporary directory should be created");
            let descriptor = File::open(root.path())
                .expect("directory descriptor should open")
                .into_raw_fd();
            let result = verify_fd_mounts(&[format!("{descriptor}:relative")]);
            let closed_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            let closed_errno = std::io::Error::last_os_error().raw_os_error();

            let error = result.expect_err("relative mount destinations must be rejected");

            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!(closed_flags, -1);
            assert_eq!(closed_errno, Some(libc::EBADF));
        },
    );
}

/// A transferred descriptor can be consumed only once even if a marker is repeated.
#[test]
fn duplicate_mount_descriptors_are_rejected() {
    in_fresh_fd_observer_with_output(
        "duplicate_mount_descriptors_are_rejected",
        "original_duplicate_mount_descriptors_are_rejected",
        false,
        || {
            let root = tempfile::tempdir().expect("temporary directory should be created");
            let descriptor = File::open(root.path())
                .expect("directory descriptor should open")
                .into_raw_fd();
            let marker = format!("{descriptor}:{}", root.path().display());

            let result = verify_fd_mounts(&[marker.clone(), marker]);
            let closed_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            let closed_errno = std::io::Error::last_os_error().raw_os_error();

            let error = result.expect_err("a mount descriptor must not be consumed twice");

            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!(closed_flags, -1);
            assert_eq!(closed_errno, Some(libc::EBADF));
        },
    );
}

/// Run the observer in a newly exec-created exact-test process. The outer suite
/// stays parallel; only this child has one synchronous test and no async runtime.
fn in_fresh_fd_observer(test: &str, observation: &str, observe: impl FnOnce()) {
    in_fresh_fd_observer_with_output(test, observation, true, observe);
}

fn in_fresh_fd_observer_with_output(
    test: &str,
    observation: &str,
    relay_output: bool,
    observe: impl FnOnce(),
) {
    const CHILD: &str = "CODEX_FD_CLOSE_OBSERVER_CHILD";
    let module = module_path!()
        .split_once("::")
        .expect("test module has a crate prefix")
        .1;
    let exact_name = format!("{module}::{test}");
    if let Some(requested) = std::env::var_os(CHILD) {
        assert_eq!(requested, std::ffi::OsStr::new(&exact_name));
        assert_eq!(
            std::env::args().skip(1).collect::<Vec<_>>(),
            [
                exact_name.clone(),
                "--exact".to_string(),
                "--nocapture".to_string(),
                "--test-threads=1".to_string(),
            ]
        );
        assert_eq!(std::env::var("RUST_TEST_THREADS").as_deref(), Ok("1"));
        observe();
        println!("\nFD_CLOSE_OBSERVER_COMPLETE {observation}");
    } else {
        let output = std::process::Command::new(
            std::env::current_exe().expect("current test executable should be known"),
        )
        .args([
            exact_name.as_str(),
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, &exact_name)
        .env("RUST_TEST_THREADS", "1")
        .output()
        .expect("fresh exact observer should start");
        let stdout = String::from_utf8(output.stdout).expect("observer stdout should be UTF-8");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "fresh observer failed: {}\n{stdout}\n{stderr}",
            output.status
        );
        assert_eq!(
            stdout
                .lines()
                .filter(|line| *line == format!("FD_CLOSE_OBSERVER_COMPLETE {observation}"))
                .count(),
            1,
            "unexpected completion marker: {stdout}\n{stderr}"
        );
        assert_eq!(
            stdout
                .lines()
                .filter(|line| *line == "running 1 test")
                .count(),
            1,
            "unexpected child test count: {stdout}\n{stderr}"
        );
        assert_eq!(
            stdout.matches(&format!("test {exact_name} ... ")).count(),
            1,
            "unexpected child name: {stdout}\n{stderr}"
        );
        let summaries = stdout
            .lines()
            .filter(|line| line.starts_with("test result:"))
            .collect::<Vec<_>>();
        assert_eq!(
            summaries.len(),
            1,
            "unexpected observer summaries: {stdout}"
        );
        assert!(
            summaries[0]
                .starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; "),
            "unexpected observer result: {stdout}"
        );
        assert_eq!(
            stdout.lines().filter(|line| !line.is_empty()).last(),
            Some(summaries[0]),
            "child output after terminal summary: {stdout}\n{stderr}"
        );
        if relay_output {
            print!("{stdout}");
        }
    }
}

/// A fresh observer retains both the denial and immediate closed-slot oracles.
#[test]
fn fresh_process_symlink_denial_observes_closed_descriptor() {
    in_fresh_fd_observer(
        "fresh_process_symlink_denial_observes_closed_descriptor",
        "denial_then_closed",
        || {
            let root = tempfile::tempdir().expect("observer fixture should be created");
            let source = root.path().join("source");
            let destination = root.path().join("destination");
            std::fs::create_dir(&source).expect("observer source should be created");
            std::os::unix::fs::symlink(&source, &destination)
                .expect("observer destination should be a symlink");
            let descriptor = File::open(&source)
                .expect("observer source descriptor should open")
                .into_raw_fd();
            let marker = format!("{descriptor}:{}", destination.display());
            // A live descriptor is a positive sensitivity control: a leaked
            // descriptor could not satisfy the -1/EBADF oracle below.
            let live_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            let live_errno = std::io::Error::last_os_error().raw_os_error();
            assert_eq!(live_flags, libc::FD_CLOEXEC, "live errno={live_errno:?}");

            // No logging, fixture work, allocation of another FD, or assertion
            // formatting intervenes between return and the captured observation.
            let result = verify_fd_mounts(&[marker]);
            let closed_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            let closed_errno = std::io::Error::last_os_error().raw_os_error();

            assert_eq!(
                result
                    .expect_err("symlink destination must remain denied")
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert_eq!(closed_flags, -1);
            assert_eq!(closed_errno, Some(libc::EBADF));
            println!(
                "FD_CLOSE_OBSERVED descriptor={descriptor} denied=true immediate_flags={closed_flags} immediate_errno={closed_errno:?}"
            );
        },
    );
}

/// Deliberate safe reuse can reproduce flags=1 AFTER denial and proven closure.
/// It demonstrates observer ambiguity, not the cause of a particular CI failure.
#[test]
fn fresh_process_closed_slot_can_be_reused_with_cloexec() {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;

    in_fresh_fd_observer(
        "fresh_process_closed_slot_can_be_reused_with_cloexec",
        "denial_closed_then_safe_reuse",
        || {
            let root = tempfile::tempdir().expect("reuse fixture should be created");
            let source = root.path().join("source");
            let destination = root.path().join("destination");
            std::fs::create_dir(&source).expect("reuse source should be created");
            std::os::unix::fs::symlink(&source, &destination)
                .expect("reuse destination should be a symlink");
            // Open every fixture FD before transferring the source. /dev/null
            // is harmless, and its File remains the unique owner throughout.
            let sentinel = File::open("/dev/null").expect("harmless sentinel should open");
            let descriptor = File::open(&source)
                .expect("reuse source descriptor should open")
                .into_raw_fd();
            let marker = format!("{descriptor}:{}", destination.display());

            let result = verify_fd_mounts(&[marker]);
            let closed_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            let closed_errno = std::io::Error::last_os_error().raw_os_error();

            assert_eq!(
                result
                    .expect_err("symlink destination must remain denied")
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert_eq!(closed_flags, -1);
            assert_eq!(closed_errno, Some(libc::EBADF));

            // F_DUPFD_CLOEXEC allocates an UNUSED slot at or above the minimum;
            // unlike dup3 it cannot overwrite any unknown live descriptor.
            let duplicated =
                unsafe { libc::fcntl(sentinel.as_raw_fd(), libc::F_DUPFD_CLOEXEC, descriptor) };
            let duplicate_errno = std::io::Error::last_os_error().raw_os_error();
            assert!(duplicated >= 0, "duplication errno={duplicate_errno:?}");
            // SAFETY: successful F_DUPFD_CLOEXEC returned a newly owned FD.
            // Own it before checking exact slot identity, including panic paths.
            let reused = unsafe { OwnedFd::from_raw_fd(duplicated) };
            let reused_flags = unsafe { libc::fcntl(reused.as_raw_fd(), libc::F_GETFD) };
            let reused_errno = std::io::Error::last_os_error().raw_os_error();

            assert_eq!(reused.as_raw_fd(), descriptor);
            assert_eq!(reused_flags, libc::FD_CLOEXEC);
            println!(
                "FD_CLOSE_REUSE_OBSERVED descriptor={descriptor} denied=true immediate_flags={closed_flags} immediate_errno={closed_errno:?} reused_flags={reused_flags} reused_errno={reused_errno:?}"
            );
            // `reused` and `sentinel` each close exactly their own live FD.
            // The transferred source number has no remaining File owner here.
        },
    );
}
