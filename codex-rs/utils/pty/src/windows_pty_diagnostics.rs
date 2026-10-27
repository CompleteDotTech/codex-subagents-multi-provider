//! First measurement only: no descendant scan or shell command/version probe.
use std::io;
use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;
use winapi::um::handleapi::DuplicateHandle;
use winapi::um::processthreadsapi::GetCurrentProcess;
use winapi::um::processthreadsapi::GetProcessId;
use winapi::um::processthreadsapi::GetProcessTimes;
use winapi::um::winbase::QueryFullProcessImageNameW;
use winapi::um::winnt::DUPLICATE_SAME_ACCESS;

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // Fields are intentionally consumed by bounded Debug reporting.
pub(crate) struct Summary {
    len: usize,
    class: u8,
}
impl Summary {
    pub(crate) fn new(bytes: &[u8]) -> Self {
        // 1 = exactly Ctrl-C, 2 = contains CR/LF, 0 = other. No input retained.
        Self {
            len: bytes.len(),
            class: if bytes == [3] {
                1
            } else if bytes.iter().any(|b| *b == 10 || *b == 13) {
                2
            } else {
                0
            },
        }
    }
}
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // Fields are intentionally consumed by bounded Debug reporting.
struct Event {
    sequence: u64,
    elapsed_us: u128,
    raw: Summary,
    normalized: Summary,
    write_ok: bool,
    write_os_error: Option<i32>,
    flush_ok: bool,
    flush_os_error: Option<i32>,
}
pub(crate) struct Diagnostics {
    root: Result<OwnedHandle, Option<i32>>,
    enabled: AtomicBool,
    sequence: AtomicU64,
    lost: AtomicU64,
    events: Mutex<[Option<Event>; 32]>,
    start: Instant,
}
impl Diagnostics {
    pub(crate) fn capture(child: &dyn portable_pty::Child) -> Self {
        let root = child.as_raw_handle().ok_or(None).and_then(|raw| {
            let mut duplicate = std::ptr::null_mut();
            // SAFETY: called while exact owned Child is alive, before wait task
            // receives it. Only successful fresh duplicate becomes OwnedHandle.
            let ok = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    raw.cast(),
                    GetCurrentProcess(),
                    &mut duplicate,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            if ok == 0 {
                Err(io::Error::last_os_error().raw_os_error())
            } else {
                Ok(unsafe { OwnedHandle::from_raw_handle(duplicate.cast()) })
            }
        });
        Self {
            root,
            enabled: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            events: Mutex::new([None; 32]),
            start: Instant::now(),
        }
    }
    pub(crate) fn enable(self: &Arc<Self>) -> Report {
        self.enabled.store(true, Ordering::Relaxed);
        Report(Arc::clone(self))
    }
    pub(crate) fn record(
        &self,
        raw: Summary,
        bytes: &[u8],
        write: &io::Result<()>,
        flush: &io::Result<()>,
    ) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let event = Event {
            sequence,
            elapsed_us: self.start.elapsed().as_micros(),
            raw,
            normalized: Summary::new(bytes),
            write_ok: write.is_ok(),
            write_os_error: write.as_ref().err().and_then(io::Error::raw_os_error),
            flush_ok: flush.is_ok(),
            flush_os_error: flush.as_ref().err().and_then(io::Error::raw_os_error),
        };
        // No waiting, allocation, output or native queries on writer path.
        if let Ok(mut events) = self.events.try_lock() {
            if let Ok(index) = usize::try_from(sequence) {
                if let Some(slot) = events.get_mut(index) {
                    *slot = Some(event);
                    return;
                }
            }
        }
        self.lost.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) struct Report(Arc<Diagnostics>);
impl Drop for Report {
    fn drop(&mut self) {
        self.0.enabled.store(false, Ordering::Relaxed);
        // Copy bounded data, release mutex before querying native state/output.
        let events = self.0.events.try_lock().ok().map(|events| *events);
        let identity = self.0.root.as_ref().map_err(|error| *error).map(|root| {
            let raw = root.as_raw_handle().cast();
            // SAFETY: root is our still-owned duplicate; buffers are fixed and valid.
            let pid = unsafe { GetProcessId(raw) };
            let pid = if pid == 0 {
                Err(io::Error::last_os_error().raw_os_error())
            } else {
                Ok(pid)
            };
            let mut creation = unsafe { std::mem::zeroed() };
            let mut exit = unsafe { std::mem::zeroed() };
            let mut kernel = unsafe { std::mem::zeroed() };
            let mut user = unsafe { std::mem::zeroed() };
            let birth = if unsafe {
                GetProcessTimes(raw, &mut creation, &mut exit, &mut kernel, &mut user)
            } == 0
            {
                Err(io::Error::last_os_error().raw_os_error())
            } else {
                Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
            };
            let mut image = [0u16; 4096];
            let mut len = image.len() as u32;
            let image =
                if unsafe { QueryFullProcessImageNameW(raw, 0, image.as_mut_ptr(), &mut len) } == 0
                {
                    Err(io::Error::last_os_error().raw_os_error())
                } else {
                    Ok(String::from_utf16_lossy(&image[..len as usize]))
                };
            (pid, birth, image)
        });
        // Debug escapes image control characters. Fixed 4096 UTF-16 image cap,
        // 32 fixed numeric events, no arbitrary error text or stdin/output payload.
        // Reporting must not panic on stderr failure during test unwinding.
        let _ = writeln!(
            io::stderr().lock(),
            "PTY_DIAGNOSTIC_V1 identity={identity:?} events={events:?} attempted={} lost={} shell_semantic_version=unmeasured",
            self.0.sequence.load(Ordering::Relaxed),
            self.0.lost.load(Ordering::Relaxed)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostics() -> Diagnostics {
        Diagnostics {
            root: Err(None),
            enabled: AtomicBool::new(true),
            sequence: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            events: Mutex::new([None; 32]),
            start: Instant::now(),
        }
    }

    #[test]
    fn summaries_classify_without_retaining_payload() {
        for (bytes, class) in [
            (b"\x03".as_slice(), 1),
            (b"\r".as_slice(), 2),
            (b"\n".as_slice(), 2),
            (b"private\r\n".as_slice(), 2),
            (b"private".as_slice(), 0),
            (b"\x03x".as_slice(), 0),
            (b"".as_slice(), 0),
        ] {
            let summary = Summary::new(bytes);
            assert_eq!(summary.len, bytes.len());
            assert_eq!(summary.class, class);
        }
        // Equal lengths/classes reveal no distinction between different payloads.
        assert_eq!(
            format!("{:?}", Summary::new(b"secret-a")),
            format!("{:?}", Summary::new(b"secret-b"))
        );
    }

    #[test]
    fn events_keep_first_32_and_count_overflow_without_payload() {
        let diagnostics = diagnostics();
        for _ in 0..35 {
            diagnostics.record(
                Summary::new(b"private\n"),
                b"private\r",
                &Ok(()),
                &Err(io::Error::from_raw_os_error(5)),
            );
        }
        assert_eq!(diagnostics.sequence.load(Ordering::Relaxed), 35);
        assert_eq!(diagnostics.lost.load(Ordering::Relaxed), 3);
        let events = diagnostics.events.lock().unwrap();
        for (index, event) in events.iter().enumerate() {
            let event = event.unwrap();
            assert_eq!(event.sequence, index as u64);
            assert_eq!(event.raw.len, 8);
            assert_eq!(event.raw.class, 2);
            assert_eq!(event.normalized.len, 8);
            assert_eq!(event.normalized.class, 2);
            assert!(event.write_ok);
            assert_eq!(event.write_os_error, None);
            assert!(!event.flush_ok);
            assert_eq!(event.flush_os_error, Some(5));
        }
        assert!(!format!("{events:?}").contains("private"));
    }

    #[test]
    fn contention_drops_an_event_and_disabled_recording_is_inert() {
        let diagnostics = diagnostics();
        let guard = diagnostics.events.lock().unwrap();
        diagnostics.record(Summary::new(b"\x03"), b"\x03", &Ok(()), &Ok(()));
        assert_eq!(diagnostics.lost.load(Ordering::Relaxed), 1);
        assert!(guard.iter().all(Option::is_none));
        drop(guard);
        diagnostics.enabled.store(false, Ordering::Relaxed);
        diagnostics.record(Summary::new(b"\x03"), b"\x03", &Ok(()), &Ok(()));
        assert_eq!(diagnostics.sequence.load(Ordering::Relaxed), 1);
        assert_eq!(diagnostics.lost.load(Ordering::Relaxed), 1);
    }
}
