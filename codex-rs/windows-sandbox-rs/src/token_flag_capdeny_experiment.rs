//! Private fixture intervention and readonly exact-child lifetime observations.
use super::*;
use std::fs::File;
use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt;
use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
use windows_sys::Win32::Foundation::DuplicateHandle;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::ACL_SIZE_INFORMATION;
use windows_sys::Win32::Security::AclSizeInformation;
use windows_sys::Win32::Security::Authorization::DENY_ACCESS;
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
use windows_sys::Win32::Security::Authorization::SetEntriesInAclW;
use windows_sys::Win32::Security::Authorization::SetSecurityInfo;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_SID;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_UNKNOWN;
use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::GetAclInformation;
use windows_sys::Win32::Security::GetSecurityDescriptorControl;
use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::SE_DACL_PROTECTED;
use windows_sys::Win32::Security::UNPROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_BASIC_ACCOUNTING_INFORMATION;
use windows_sys::Win32::System::JobObjects::JobObjectBasicAccountingInformation;
use windows_sys::Win32::System::JobObjects::QueryInformationJobObject;
use windows_sys::Win32::System::SystemServices::MAXIMUM_ALLOWED;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

pub(crate) struct Installed {
    parents: [File; 2],
    paths: [PathBuf; 2],
    acls: Vec<AclSnapshot>,
}

pub(crate) struct Expected {
    baseline: ObservationResult,
    acls: Vec<AclSnapshot>,
    parents: [File; 2],
    paths: [PathBuf; 2],
    root: PathBuf,
}

impl Expected {
    pub(crate) fn new(
        baseline: ObservationResult,
        installed: Installed,
        root: PathBuf,
    ) -> Result<Self> {
        let Installed {
            parents,
            paths,
            acls,
        } = installed;
        ensure!(
            baseline.acls.len() == 15 && acls.len() == 15,
            "fixed fifteen-object fixture required"
        );
        for (index, (before, after)) in baseline.acls.iter().zip(&acls).enumerate() {
            if index == 1 || index == 4 {
                require_exact_deny_delta(before, after, &baseline.roots)?;
            } else {
                ensure!(
                    before == after,
                    "unexpected fixture descriptor delta outside two parents"
                );
            }
        }
        // Fixed object order: directories6, delete-files4, read-sentinels4, script1.
        // In particular, the outside file and its surviving sentinel cannot gain
        // a writable-root capability allow while the intervention is installed.
        for index in [9, 13] {
            for ace in ace_bytes(&acls[index].bytes)? {
                for (_, sid) in &baseline.roots {
                    ensure!(
                        !simple_ace_matches(
                            &ace,
                            0,
                            sid,
                            FILE_GENERIC_WRITE | 0x10000 | FILE_DELETE_CHILD,
                            false
                        ),
                        "outside object acquired writable capability allow"
                    );
                }
            }
        }
        Ok(Self {
            baseline,
            acls,
            parents,
            paths,
            root,
        })
    }

    pub(crate) fn check_parents(&self) -> Result<()> {
        for (index, (path, retained)) in self.paths.iter().zip(&self.parents).enumerate() {
            require_identity(&self.root, path, retained)?;
            ensure!(
                read_retained_acl(retained)? == self.acls[if index == 0 { 1 } else { 4 }],
                "B actual retained intervention parent descriptor changed"
            );
        }
        Ok(())
    }

    pub(crate) fn check_fixture(&self, root: &Path, objects: &[PathBuf]) -> Result<()> {
        self.check_parents()?;
        ensure!(
            capture_acls(root, objects)? == self.acls,
            "owned intervention fixture changed"
        );
        Ok(())
    }
}

pub(crate) struct BObservation {
    expected: Option<Arc<Expected>>,
    lifetime: Mutex<Option<RetainedChild>>,
}

impl BObservation {
    pub(crate) fn new(expected: Option<Arc<Expected>>) -> Self {
        Self {
            expected,
            lifetime: Mutex::new(None),
        }
    }

    pub(super) unsafe fn check_and_retain(
        &self,
        token: &TokenSnapshot,
        roots: &StoredRootSids,
        record: &[u8],
        acls: &[AclSnapshot],
        child: HANDLE,
        job: &Arc<JobObject>,
    ) -> Result<()> {
        ensure!(
            roots.iter().all(|(_, sid)| *sid != token.user),
            "B actual capability SID aliases owner; no owner deny admitted"
        );
        if let Some(expected) = &self.expected {
            expected.check_parents()?;
            ensure!(
                *token == expected.baseline.token,
                "B actual restricted token changed"
            );
            ensure!(
                *roots == expected.baseline.roots,
                "B actual root capability map changed"
            );
            ensure!(
                record == expected.baseline.record,
                "B private capability record changed"
            );
            ensure!(
                acls == expected.acls,
                "fixture-not-retained: real preparation altered intervention or unrelated descriptors"
            );
            eprintln!(
                "LEGACY_CAPDENY postprep actual_token_vectors_and_attributes_equal=true actual_root_map_equal=true actual_store_bytes_equal=true exact_parent_denies_retained=true declared_fifteen_descriptors_equal=true owner_capability_distinct=true"
            );
        }
        let mut lifetime = self
            .lifetime
            .lock()
            .map_err(|_| anyhow!("B lifetime mutex poisoned"))?;
        ensure!(lifetime.is_none(), "B lifetime already retained");
        let mut duplicate = std::ptr::null_mut();
        ensure!(
            DuplicateHandle(
                GetCurrentProcess(),
                child,
                GetCurrentProcess(),
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS
            ) != 0,
            "B retained actual process duplicate failed: {}",
            GetLastError()
        );
        // Retain before any later fallible query/output. No PID rediscovery.
        *lifetime = Some(RetainedChild {
            process: Some(duplicate),
            job: Arc::clone(job),
        });
        eprintln!(
            "LEGACY_CAPDENY actual_process_retained=true same_job_retained=true readonly_metadata_valid=true"
        );
        Ok(())
    }

    pub(crate) fn finish_collected(&self) -> Result<()> {
        let mut lifetime = self
            .lifetime
            .lock()
            .map_err(|_| anyhow!("B lifetime mutex poisoned"))?;
        let mut child = lifetime
            .take()
            .ok_or_else(|| anyhow!("B actual child lifetime missing"))?;
        let handle = child
            .process
            .ok_or_else(|| anyhow!("B actual process handle missing"))?;
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        let wait_error = (wait == u32::MAX).then(|| unsafe { GetLastError() });
        let active = child.active_processes();
        if wait != WAIT_OBJECT_0 || !matches!(&active, Ok(0)) {
            let initial = format!(
                "B collection cleanup not confirmed: wait={wait} wait_error={wait_error:?} active={active:?}"
            );
            return match child.abort_checked() {
                Ok(()) => Err(anyhow!("{initial}; owned bounded abort confirmed")),
                Err(error) => Err(error.context(initial)),
            };
        }
        child.close_process()?;
        eprintln!(
            "LEGACY_CAPDENY after_collect root_signaled=true active_job_processes=0 retained_handle_closed=true streams_collected=true"
        );
        Ok(())
    }

    pub(crate) fn abort_after_collection_error(&self) -> Result<()> {
        let mut lifetime = self
            .lifetime
            .lock()
            .map_err(|_| anyhow!("B lifetime mutex poisoned"))?;
        if let Some(mut child) = lifetime.take() {
            child.abort_checked()?;
        }
        Ok(())
    }
}

// The handle is owned, process-wide and used only through native handle APIs;
// Mutex serializes its lifetime. It carries no borrowed token or SID pointers.
struct RetainedChild {
    process: Option<HANDLE>,
    job: Arc<JobObject>,
}
unsafe impl Send for RetainedChild {}

impl RetainedChild {
    fn active_processes(&self) -> Result<u32> {
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe {
                QueryInformationJobObject(
                    self.job.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    std::mem::size_of_val(&accounting) as u32,
                    std::ptr::null_mut(),
                )
            } != 0,
            "B exact Job accounting query failed: {}",
            unsafe { GetLastError() }
        );
        Ok(accounting.ActiveProcesses)
    }
    fn close_process(&mut self) -> Result<()> {
        if let Some(handle) = self.process.take() {
            ensure!(
                unsafe { CloseHandle(handle) } != 0,
                "B retained handle close failed: {}",
                unsafe { GetLastError() }
            );
        }
        Ok(())
    }
    fn abort_checked(&mut self) -> Result<()> {
        let Some(handle) = self.process else {
            return Ok(());
        };
        let job_error = self
            .job
            .terminate()
            .err()
            .map(|error| error.raw_os_error().unwrap_or(-1));
        let process_error = unsafe { (TerminateProcess(handle, 1) == 0).then(|| GetLastError()) };
        let wait = unsafe { WaitForSingleObject(handle, 2000) };
        let wait_error = (wait == u32::MAX).then(|| unsafe { GetLastError() });
        let active = self.active_processes();
        let close = self.close_process();
        let confirmed = job_error.is_none()
            && process_error.is_none()
            && wait == WAIT_OBJECT_0
            && matches!(&active, Ok(0))
            && close.is_ok();
        let _ = writeln!(
            std::io::stderr().lock(),
            "LEGACY_CAPDENY bounded_abort confirmed={confirmed} job_error={job_error:?} process_error={process_error:?} wait={wait} wait_error={wait_error:?} active={active:?} close={close:?}"
        );
        ensure!(
            confirmed,
            "B owned cleanup uncertain: job_error={job_error:?} process_error={process_error:?} wait={wait} wait_error={wait_error:?} active={active:?} close={close:?}"
        );
        Ok(())
    }
}
impl Drop for RetainedChild {
    fn drop(&mut self) {
        if self.process.is_some() {
            let _ = self.abort_checked();
        }
    }
}

struct LocalAllocation(HLOCAL);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn open_owned(root: &Path, path: &Path) -> Result<File> {
    open_owned_access(root, path, MAXIMUM_ALLOWED)
}
fn open_owned_access(root: &Path, path: &Path, access: u32) -> Result<File> {
    plain_owned(root, path)?;
    let file = OpenOptions::new()
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    require_identity(root, path, &file)?;
    Ok(file)
}
fn identity(file: &File) -> Result<(u32, u32, u32, u32)> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
        "B owned handle identity query failed: {}",
        unsafe { GetLastError() }
    );
    ensure!(
        info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "B owned handle is reparse object"
    );
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
        info.dwFileAttributes,
    ))
}
fn require_identity(root: &Path, path: &Path, retained: &File) -> Result<()> {
    plain_owned(root, path)?;
    let reopened = OpenOptions::new()
        .access_mode(windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    ensure!(
        identity(retained)? == identity(&reopened)?,
        "B owned parent identity drift"
    );
    Ok(())
}
unsafe fn acl_snapshot(acl: *mut ACL, descriptor: *mut std::ffi::c_void) -> Result<AclSnapshot> {
    ensure!(!acl.is_null(), "B null DACL rejected");
    let mut control = 0;
    let mut revision = 0;
    ensure!(
        GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) != 0,
        "B descriptor control query failed: {}",
        GetLastError()
    );
    let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
    ensure!(
        GetAclInformation(
            acl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of_val(&info) as u32,
            AclSizeInformation
        ) != 0,
        "B ACL size query failed: {}",
        GetLastError()
    );
    ensure!(
        (8..=65536).contains(&info.AclBytesInUse),
        "B invalid ACL size"
    );
    Ok(AclSnapshot {
        control,
        bytes: std::slice::from_raw_parts(acl.cast::<u8>(), info.AclBytesInUse as usize).to_vec(),
    })
}
fn read_retained_acl(file: &File) -> Result<AclSnapshot> {
    unsafe {
        let mut descriptor = std::ptr::null_mut();
        let mut acl = std::ptr::null_mut();
        let code = GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut acl,
            std::ptr::null_mut(),
            &mut descriptor,
        );
        let _allocation = LocalAllocation(descriptor.cast());
        ensure!(
            code == ERROR_SUCCESS,
            "B retained GetSecurityInfo failed: {code}"
        );
        acl_snapshot(acl, descriptor)
    }
}
fn ace_bytes(bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    ensure!(
        bytes.len() >= 8 && bytes.len() <= 65536,
        "B invalid bounded ACL bytes"
    );
    let size = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    ensure!(size >= bytes.len(), "B ACL declared size too short");
    let mut offset = 8;
    let mut out = Vec::new();
    for _ in 0..count {
        ensure!(offset + 4 <= bytes.len(), "B ACE header exceeds ACL");
        let length = u16::from_le_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        ensure!(
            length >= 4 && offset + length <= bytes.len(),
            "B ACE exceeds ACL"
        );
        out.push(bytes[offset..offset + length].to_vec());
        offset += length;
    }
    ensure!(offset == bytes.len(), "B unexpected ACL trailing bytes");
    Ok(out)
}
fn simple_ace_matches(ace: &[u8], kind: u8, sid: &[u8], mask: u32, exact: bool) -> bool {
    if ace.len() < 16 || ace[0] != kind {
        return false;
    }
    let actual_mask = u32::from_le_bytes([ace[4], ace[5], ace[6], ace[7]]);
    let sid_length = 8 + 4 * ace[9] as usize;
    ace[8] == 1
        && ace.len() == 8 + sid_length
        && &ace[8..] == sid
        && if exact {
            actual_mask == mask && ace[1] == 0
        } else {
            actual_mask & mask != 0
        }
}
fn require_exact_deny_delta(
    before: &AclSnapshot,
    after: &AclSnapshot,
    roots: &StoredRootSids,
) -> Result<()> {
    ensure!(
        before.control == after.control,
        "B parent descriptor control changed"
    );
    ensure!(
        roots.len() == 3,
        "B requires exactly three actual stored root SIDs"
    );
    let mut retained = Vec::new();
    let mut counts = [0usize; 3];
    for ace in ace_bytes(&after.bytes)? {
        if let Some(index) = roots
            .iter()
            .position(|(_, sid)| simple_ace_matches(&ace, 1, sid, FILE_DELETE_CHILD, true))
        {
            counts[index] += 1;
        } else {
            retained.push(ace);
        }
    }
    ensure!(
        counts == [1, 1, 1],
        "B exact noninheriting parent0x40 deny entries missing or duplicated"
    );
    ensure!(
        retained == ace_bytes(&before.bytes)?,
        "B intervention altered unrelated ACE bytes/order"
    );
    Ok(())
}

/// The only intervention: retained MAXIMUM_ALLOWED handles to these two parents.
pub(crate) fn install_parent_denies(
    root: &Path,
    parents: &[PathBuf; 2],
    objects: &[PathBuf],
    roots: &StoredRootSids,
) -> Result<Installed> {
    ensure!(
        objects.len() == 15 && parents[0] == objects[1] && parents[1] == objects[4],
        "B fixed parent/object mapping required"
    );
    ensure!(roots.len() == 3, "B requires actual three-root map");
    let before = capture_acls(root, objects)?;
    let mut retained_parents = Vec::new();
    for (index, path) in parents.iter().enumerate() {
        let file = open_owned(root, path)?;
        ensure!(
            identity(&file)?.3 & FILE_ATTRIBUTE_DIRECTORY != 0,
            "B intervention target is not directory"
        );
        let observed = read_retained_acl(&file)?;
        ensure!(
            observed == before[if index == 0 { 1 } else { 4 }],
            "B retained parent snapshot mismatch"
        );
        for ace in ace_bytes(&observed.bytes)? {
            for (_, sid) in roots {
                ensure!(
                    !simple_ace_matches(&ace, 1, sid, u32::MAX, false),
                    "B baseline parent already has capability deny"
                );
            }
        }
        unsafe {
            let mut descriptor = std::ptr::null_mut();
            let mut acl = std::ptr::null_mut();
            let code = GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
                &mut descriptor,
            );
            let _descriptor = LocalAllocation(descriptor.cast());
            ensure!(
                code == ERROR_SUCCESS && !acl.is_null(),
                "B retained non-null parent DACL query failed: {code}"
            );
            let entries = roots
                .iter()
                .map(|(_, sid)| EXPLICIT_ACCESS_W {
                    grfAccessPermissions: FILE_DELETE_CHILD,
                    grfAccessMode: DENY_ACCESS,
                    grfInheritance: 0,
                    Trustee: TRUSTEE_W {
                        pMultipleTrustee: std::ptr::null_mut(),
                        MultipleTrusteeOperation: 0,
                        TrusteeForm: TRUSTEE_IS_SID,
                        TrusteeType: TRUSTEE_IS_UNKNOWN,
                        ptstrName: sid.as_ptr().cast_mut().cast(),
                    },
                })
                .collect::<Vec<_>>();
            let mut updated = std::ptr::null_mut();
            let code = SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), acl, &mut updated);
            let _updated = LocalAllocation(updated.cast());
            ensure!(
                code == ERROR_SUCCESS && !updated.is_null(),
                "B exact parent deny construction failed: {code}"
            );
            require_identity(root, path, &file)?;
            let code = SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                updated,
                std::ptr::null_mut(),
            );
            ensure!(
                code == ERROR_SUCCESS,
                "B retained parent deny update failed: {code}"
            );
        }
        require_identity(root, path, &file)?;
        require_exact_deny_delta(&observed, &read_retained_acl(&file)?, roots)?;
        retained_parents.push(file);
    }
    let after = capture_acls(root, objects)?;
    for index in 0..objects.len() {
        if index != 1 && index != 4 {
            ensure!(
                before[index] == after[index],
                "B MAXIMUM_ALLOWED intervention propagated to child/unrelated descriptor"
            );
        }
    }
    eprintln!(
        "LEGACY_CAPDENY intervention parents=2 actual_root_sids=3 mask=64 inheritance=0 owner_and_unrelated_aces_preserved=true child_descriptors_unchanged=true retained_handle_identity=true"
    );
    let retained_parents: [File; 2] = retained_parents
        .try_into()
        .map_err(|_| anyhow!("B retained parent count mismatch"))?;
    Ok(Installed {
        parents: retained_parents,
        paths: parents.clone(),
        acls: after,
    })
}

/// Recreate only the deleted .git object's existing expected setup descriptor.
/// Parent allows/denies and every surviving object are left alone.
pub(crate) fn restore_owned_git_acl(
    root: &Path,
    path: &Path,
    expected: &AclSnapshot,
) -> Result<()> {
    let file = open_owned_access(
        root,
        path,
        windows_sys::Win32::Storage::FileSystem::READ_CONTROL
            | windows_sys::Win32::Storage::FileSystem::WRITE_DAC,
    )?;
    ensure!(
        identity(&file)?.3 & FILE_ATTRIBUTE_DIRECTORY != 0,
        "B restored git target must be directory"
    );
    ensure!(
        std::fs::read_dir(path)?.next().is_none(),
        "B recreated git fixture must be empty; no descendant propagation admitted"
    );
    let _ = ace_bytes(&expected.bytes)?;
    // The captured in-use prefix may be shorter than ACL.AclSize. Supply the
    // entire declared allocation to the native setter, never an undersized Vec.
    let declared = u16::from_le_bytes([expected.bytes[2], expected.bytes[3]]) as usize;
    let mut native_acl = vec![0u8; declared];
    native_acl[..expected.bytes.len()].copy_from_slice(&expected.bytes);
    let protection = if expected.control & SE_DACL_PROTECTED != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    unsafe {
        let code = SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | protection,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            native_acl.as_mut_ptr().cast(),
            std::ptr::null_mut(),
        );
        ensure!(
            code == ERROR_SUCCESS,
            "B owned git descriptor restore failed: {code}"
        );
    }
    require_identity(root, path, &file)?;
    ensure!(
        read_retained_acl(&file)? == *expected,
        "B recreated git descriptor differs from declared baseline setup"
    );
    Ok(())
}
