//! Closed unit-test experiment; no release export or authority-changing observer.

use super::TokenGroup;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use codex_utils_pty::JobObject;
use std::io::Read;
use std::io::Write;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Security::TokenIsRestricted;
use windows_sys::Win32::Security::TokenPrimary;
use windows_sys::Win32::Security::TokenRestrictedSids;
use windows_sys::Win32::Security::TokenType;
use windows_sys::Win32::System::Threading::OpenProcessToken;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::ResumeThread;
use windows_sys::Win32::System::Threading::TerminateProcess;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

#[derive(Clone, Copy, Debug)]
pub(crate) enum FlagMode {
    Default,
    WithoutWriteRestricted,
}

impl FlagMode {
    pub(crate) fn input_flags(self) -> u32 {
        match self {
            Self::Default => 0x0d,
            Self::WithoutWriteRestricted => 0x05,
        }
    }
}

// Intentionally no Debug: these values contain account and capability identities.
#[derive(PartialEq, Eq)]
pub(crate) struct TokenSnapshot {
    groups: Vec<TokenGroup>,
    restricting: Vec<TokenGroup>,
    user: Vec<u8>,
    token_type: u32,
    is_restricted: u32,
}

#[derive(PartialEq, Eq)]
pub(crate) struct AclSnapshot {
    control: u16,
    bytes: Vec<u8>,
}

type StoredRootSids = Vec<(String, Vec<u8>)>;

pub(crate) struct ObservationResult {
    pub(crate) token: TokenSnapshot,
    pub(crate) roots: StoredRootSids,
    pub(crate) record: Vec<u8>,
    pub(crate) acls: Vec<AclSnapshot>,
}

pub(crate) struct Observation {
    pub(crate) mode: FlagMode,
    root: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
    roots: Vec<PathBuf>,
    objects: Vec<PathBuf>,
    pub(crate) result: Mutex<Option<ObservationResult>>,
}

impl Observation {
    pub(crate) fn new(
        mode: FlagMode,
        root: PathBuf,
        home: PathBuf,
        workspace: PathBuf,
        roots: Vec<PathBuf>,
        objects: Vec<PathBuf>,
    ) -> Self {
        Self {
            mode,
            root,
            home,
            workspace,
            roots,
            objects,
            result: Mutex::new(None),
        }
    }

    // This fixed observer may only query retained handles and owned fixture state.
    pub(crate) unsafe fn observe(&self, launch: HANDLE, child_process: HANDLE) -> Result<()> {
        let mut raw = std::ptr::null_mut();
        ensure!(
            OpenProcessToken(child_process, TOKEN_QUERY, &mut raw) != 0,
            "actual child OpenProcessToken(TOKEN_QUERY) failed: {}",
            GetLastError()
        );
        let child = OwnedHandle::from_raw_handle(raw);
        let launch = snapshot(launch)?;
        let child = snapshot(child.as_raw_handle())?;
        ensure!(
            launch == child,
            "actual child and exact launch token metadata differ"
        );
        ensure!(
            child.token_type == TokenPrimary as u32,
            "child token is not primary"
        );
        ensure!(
            child.is_restricted != 0,
            "child restriction query did not confirm restriction"
        );
        let (record, roots) = stored_roots(&self.home, &self.workspace, &self.roots)?;
        for (_, sid) in &roots {
            ensure!(
                child.restricting.iter().any(|group| group.sid == *sid),
                "actual stored root SID absent from actual child restricting SIDs"
            );
            ensure!(
                !child.groups.iter().any(|group| group.sid == *sid),
                "capability appears in actual child normal groups; pair is not the expected category"
            );
        }
        let world = super::world_sid()?;
        let logon = child
            .groups
            .iter()
            .filter(|group| group.attributes & 0xc0000000 == 0xc0000000)
            .collect::<Vec<_>>();
        ensure!(
            logon.len() == 1,
            "actual child logon group category is ambiguous"
        );
        ensure!(
            child
                .restricting
                .iter()
                .any(|group| group.sid == logon[0].sid),
            "actual child restricting logon membership missing"
        );
        ensure!(
            child.restricting.iter().any(|group| group.sid == world),
            "actual child restricting world membership missing"
        );
        let acls = capture_acls(&self.root, &self.objects)?;
        for (path, acl) in self.objects.iter().zip(&acls) {
            ensure!(
                crate::acl::path_mask_allows(
                    path,
                    &[child.user.as_ptr().cast_mut().cast()],
                    windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS,
                    /*require_all_bits*/ true
                )?,
                "owner full access missing before actual child resume"
            );
            if std::fs::symlink_metadata(path)?.is_file() {
                ensure!(
                    acl.control & windows_sys::Win32::Security::SE_DACL_PROTECTED == 0,
                    "file inheritance not enabled after production preparation"
                );
                for root in &self.roots {
                    // Roots in stored_roots are sorted independently. Resolve by
                    // actual path key instead of relying on iteration order.
                    let key = crate::path_normalization::canonical_path_key(root);
                    let sid = roots
                        .iter()
                        .find(|(stored_key, _)| *stored_key == key)
                        .map(|(_, bytes)| bytes)
                        .ok_or_else(|| anyhow!("missing actual root observation"))?;
                    if path.starts_with(root) {
                        ensure!(
                            crate::acl::path_mask_allows(
                                path,
                                &[sid.as_ptr().cast_mut().cast()],
                                windows_sys::Win32::Storage::FileSystem::DELETE,
                                /*require_all_bits*/ true
                            )?,
                            "actual stored root SID DELETE missing on allowed file before resume"
                        );
                    }
                }
            }
        }
        let owner_groups = child
            .groups
            .iter()
            .filter(|group| group.sid == child.user)
            .count();
        let owner_restricting = child
            .restricting
            .iter()
            .filter(|group| group.sid == child.user)
            .count();
        let enabled = child
            .groups
            .iter()
            .filter(|group| group.attributes & 0x4 != 0)
            .count();
        let deny_only = child
            .groups
            .iter()
            .filter(|group| group.attributes & 0x10 != 0)
            .count();
        eprintln!(
            "LEGACY_TOKEN_FLAGS input={:#04x} actual_child_query=true launch_child_equal=true primary=true restricted_query={} normal_count={} enabled_count={} deny_only_count={} restricting_count={} root_count={} roots_restricting=true roots_normal=false owner_normal_count={} owner_restricting_count={} logon_normal_count={} world_restricting=true acl_count={} write_restricted_state_query=unavailable",
            self.mode.input_flags(),
            child.is_restricted,
            child.groups.len(),
            enabled,
            deny_only,
            child.restricting.len(),
            roots.len(),
            owner_groups,
            owner_restricting,
            logon.len(),
            acls.len(),
        );
        let mut result = self
            .result
            .lock()
            .map_err(|_| anyhow!("observation mutex poisoned"))?;
        ensure!(result.is_none(), "observer called more than once");
        *result = Some(ObservationResult {
            token: child,
            roots,
            record,
            acls,
        });
        Ok(())
    }
}

unsafe fn snapshot(token: HANDLE) -> Result<TokenSnapshot> {
    let mut groups = super::token_groups(token, 65536)?;
    let mut needed = 0;
    GetTokenInformation(
        token,
        TokenRestrictedSids,
        std::ptr::null_mut(),
        0,
        &mut needed,
    );
    ensure!(
        needed > 0 && needed <= 65536,
        "invalid restricted group query size: {}",
        GetLastError()
    );
    let mut buffer = vec![0u8; needed as usize];
    ensure!(
        GetTokenInformation(
            token,
            TokenRestrictedSids,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed
        ) != 0,
        "GetTokenInformation(TokenRestrictedSids) failed: {}",
        GetLastError()
    );
    ensure!(
        needed as usize <= buffer.len(),
        "invalid restricted group result size"
    );
    let mut restricting = super::decode_token_groups(&buffer[..needed as usize])?;
    // Sort without deduplicating: both SID bytes and attributes, including multiplicity, matter.
    groups.sort_by(|a, b| a.sid.cmp(&b.sid).then(a.attributes.cmp(&b.attributes)));
    restricting.sort_by(|a, b| a.sid.cmp(&b.sid).then(a.attributes.cmp(&b.attributes)));
    Ok(TokenSnapshot {
        groups,
        restricting,
        user: super::get_user_sid_bytes(token)?,
        token_type: scalar(token, TokenType)?,
        is_restricted: scalar(token, TokenIsRestricted)?,
    })
}

unsafe fn scalar(token: HANDLE, class: i32) -> Result<u32> {
    let mut value = 0u32;
    let mut needed = 0;
    ensure!(
        GetTokenInformation(
            token,
            class,
            (&mut value as *mut u32).cast(),
            4,
            &mut needed
        ) != 0,
        "GetTokenInformation scalar class {} failed: {}",
        class,
        GetLastError()
    );
    ensure!(needed == 4, "invalid scalar token result size");
    Ok(value)
}

fn stored_roots(
    home: &Path,
    workspace: &Path,
    roots: &[PathBuf],
) -> Result<(Vec<u8>, StoredRootSids)> {
    let path = crate::cap::cap_sid_file(home);
    plain_owned(home, &path)?;
    let mut record = Vec::new();
    std::fs::File::open(&path)?
        .take(65537)
        .read_to_end(&mut record)?;
    ensure!(
        record.len() <= 65536,
        "private capability record exceeds bound"
    );
    let value: serde_json::Value =
        serde_json::from_slice(&record).map_err(|_| anyhow!("invalid private capability JSON"))?;
    let keys = value
        .as_object()
        .ok_or_else(|| anyhow!("invalid private capability object"))?
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        keys == std::collections::BTreeSet::from([
            "workspace",
            "readonly",
            "workspace_by_cwd",
            "writable_root_by_path"
        ]),
        "unexpected private capability schema"
    );
    let caps: crate::cap::CapSids = serde_json::from_slice(&record)
        .map_err(|_| anyhow!("invalid private capability schema"))?;
    let workspace_key = crate::path_normalization::canonical_path_key(workspace);
    let mut out = Vec::new();
    for root in roots {
        let key = crate::path_normalization::canonical_path_key(root);
        let text = if key == workspace_key {
            caps.workspace_by_cwd.get(&key)
        } else {
            caps.writable_root_by_path.get(&key)
        }
        .ok_or_else(|| anyhow!("actual root missing from existing private capability record"))?;
        let sid = super::LocalSid::from_string(text)
            .map_err(|_| anyhow!("invalid stored root capability SID"))?;
        let length = unsafe { windows_sys::Win32::Security::GetLengthSid(sid.as_ptr()) };
        ensure!(
            (8..=68).contains(&length),
            "invalid stored capability SID length"
        );
        let mut bytes = vec![0u8; length as usize];
        ensure!(
            unsafe {
                windows_sys::Win32::Security::CopySid(
                    length,
                    bytes.as_mut_ptr().cast(),
                    sid.as_ptr(),
                )
            } != 0,
            "copy stored root capability SID failed"
        );
        out.push((key, bytes));
    }
    out.sort();
    Ok((record, out))
}

fn plain_owned(root: &Path, path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        ensure!(
            std::fs::symlink_metadata(ancestor)?.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                == 0,
            "fixture observation encounters reparse object"
        );
    }
    ensure!(
        path.canonicalize()?.starts_with(root.canonicalize()?),
        "fixture observation escapes owned root"
    );
    Ok(())
}

pub(crate) fn capture_acls(root: &Path, paths: &[PathBuf]) -> Result<Vec<AclSnapshot>> {
    paths
        .iter()
        .map(|path| {
            plain_owned(root, path)?;
            unsafe {
                let (acl, descriptor) = crate::acl::fetch_dacl_handle(path)?;
                struct Descriptor(HLOCAL);
                impl Drop for Descriptor {
                    fn drop(&mut self) {
                        unsafe {
                            LocalFree(self.0);
                        }
                    }
                }
                let _owner = Descriptor(descriptor.cast());
                ensure!(!acl.is_null(), "fixture must not have a null DACL");
                let mut control = 0;
                let mut revision = 0;
                ensure!(
                    windows_sys::Win32::Security::GetSecurityDescriptorControl(
                        descriptor,
                        &mut control,
                        &mut revision
                    ) != 0,
                    "descriptor control query failed: {}",
                    GetLastError()
                );
                let mut info: windows_sys::Win32::Security::ACL_SIZE_INFORMATION =
                    std::mem::zeroed();
                ensure!(
                    windows_sys::Win32::Security::GetAclInformation(
                        acl,
                        (&mut info as *mut windows_sys::Win32::Security::ACL_SIZE_INFORMATION)
                            .cast(),
                        std::mem::size_of_val(&info) as u32,
                        windows_sys::Win32::Security::AclSizeInformation
                    ) != 0,
                    "ACL size query failed: {}",
                    GetLastError()
                );
                ensure!(
                    info.AclBytesInUse
                        >= std::mem::size_of::<windows_sys::Win32::Security::ACL>() as u32
                        && info.AclBytesInUse <= 65536,
                    "invalid fixture ACL size"
                );
                Ok(AclSnapshot {
                    control,
                    bytes: std::slice::from_raw_parts(
                        acl.cast::<u8>(),
                        info.AclBytesInUse as usize,
                    )
                    .to_vec(),
                })
            }
        })
        .collect()
}

/// Own the just-created suspended child before any observation can fail or unwind.
pub(crate) struct SuspendedChild {
    pi: PROCESS_INFORMATION,
    job: Arc<JobObject>,
    armed: bool,
    resume_attempted: bool,
}

impl SuspendedChild {
    pub(crate) fn new(pi: PROCESS_INFORMATION, job: Arc<JobObject>) -> Self {
        Self {
            pi,
            job,
            armed: true,
            resume_attempted: false,
        }
    }

    pub(crate) unsafe fn resume_once(&mut self) -> Result<()> {
        ensure!(!self.resume_attempted, "second resume attempt prohibited");
        self.resume_attempted = true;
        let previous = ResumeThread(self.pi.hThread);
        let native_error = if previous == u32::MAX {
            Some(GetLastError())
        } else {
            None
        };
        eprintln!(
            "LEGACY_TOKEN_FLAGS resume_attempts=1 previous_suspend_count={previous} native_error={native_error:?} valid={}",
            previous == 1
        );
        ensure!(
            previous == 1,
            "invalid resume result; zero does not establish that the child never ran"
        );
        Ok(())
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SuspendedChild {
    fn drop(&mut self) {
        if self.armed {
            // Never panic during cleanup; retain the exact handles through the bounded wait.
            let job_terminated = self.job.terminate().is_ok();
            let process_terminated = unsafe { TerminateProcess(self.pi.hProcess, 1) } != 0;
            let wait = unsafe { WaitForSingleObject(self.pi.hProcess, 2000) };
            let thread_closed = unsafe { CloseHandle(self.pi.hThread) } != 0;
            let process_closed = unsafe { CloseHandle(self.pi.hProcess) } != 0;
            let _ = writeln!(
                std::io::stderr().lock(),
                "LEGACY_TOKEN_FLAGS cleanup_owned_child=true job_terminated={job_terminated} process_terminated={process_terminated} wait={wait} thread_closed={thread_closed} process_closed={process_closed}"
            );
        }
    }
}
