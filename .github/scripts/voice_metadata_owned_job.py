"""Windows-only private query capture in an owned, non-breakaway Job.

No process lookup or numeric PID termination. The only uncontained process is
our suspended creation handle; assignment and identity checks precede resume.
"""

import ctypes as c
from ctypes import wintypes as w
import hashlib
import os
import re
import stat
import subprocess
import threading
import time


class SI(c.Structure):
    _fields_ = (
        [
            ("cb", w.DWORD),
            ("lpReserved", w.LPWSTR),
            ("lpDesktop", w.LPWSTR),
            ("lpTitle", w.LPWSTR),
        ]
        + [
            (name, w.DWORD)
            for name in (
                "dwX",
                "dwY",
                "dwXSize",
                "dwYSize",
                "dwXCountChars",
                "dwYCountChars",
                "dwFillAttribute",
                "dwFlags",
            )
        ]
        + [
            ("wShowWindow", w.WORD),
            ("cbReserved2", w.WORD),
            ("lpReserved2", c.c_void_p),
            ("hStdInput", w.HANDLE),
            ("hStdOutput", w.HANDLE),
            ("hStdError", w.HANDLE),
        ]
    )


class SIEX(c.Structure):
    _fields_ = [("startup", SI), ("attributes", c.c_void_p)]


class PI(c.Structure):
    _fields_ = [
        ("process", w.HANDLE),
        ("thread", w.HANDLE),
        ("pid", w.DWORD),
        ("tid", w.DWORD),
    ]


class BASIC(c.Structure):
    _fields_ = [
        ("user", c.c_longlong),
        ("job_user", c.c_longlong),
        ("flags", w.DWORD),
        ("min_ws", c.c_size_t),
        ("max_ws", c.c_size_t),
        ("active_limit", w.DWORD),
        ("affinity", c.c_size_t),
        ("priority", w.DWORD),
        ("scheduling", w.DWORD),
    ]


class EXT(c.Structure):
    _fields_ = [
        ("basic", BASIC),
        ("io", c.c_ulonglong * 6),
        ("process_memory", c.c_size_t),
        ("job_memory", c.c_size_t),
        ("peak_process", c.c_size_t),
        ("peak_job", c.c_size_t),
    ]


class ACCOUNT(c.Structure):
    _fields_ = [
        ("times", c.c_longlong * 4),
        ("faults", w.DWORD),
        ("total", w.DWORD),
        ("active", w.DWORD),
        ("terminated", w.DWORD),
    ]


class WinAPI:
    def __init__(self):
        import msvcrt

        self.oshandle = msvcrt.get_osfhandle
        kernel = c.WinDLL("kernel32", use_last_error=True)
        signatures = {
            "CreateJobObjectW": (w.HANDLE, [c.c_void_p, w.LPCWSTR]),
            "SetInformationJobObject": (
                w.BOOL,
                [w.HANDLE, c.c_int, c.c_void_p, w.DWORD],
            ),
            "AssignProcessToJobObject": (w.BOOL, [w.HANDLE, w.HANDLE]),
            "IsProcessInJob": (w.BOOL, [w.HANDLE, w.HANDLE, c.POINTER(w.BOOL)]),
            "CreateProcessW": (
                w.BOOL,
                [
                    w.LPCWSTR,
                    w.LPWSTR,
                    c.c_void_p,
                    c.c_void_p,
                    w.BOOL,
                    w.DWORD,
                    c.c_void_p,
                    w.LPCWSTR,
                    c.POINTER(SIEX),
                    c.POINTER(PI),
                ],
            ),
            "ResumeThread": (w.DWORD, [w.HANDLE]),
            "WaitForSingleObject": (w.DWORD, [w.HANDLE, w.DWORD]),
            "TerminateJobObject": (w.BOOL, [w.HANDLE, w.UINT]),
            "TerminateProcess": (w.BOOL, [w.HANDLE, w.UINT]),
            "CloseHandle": (w.BOOL, [w.HANDLE]),
            "GetExitCodeProcess": (w.BOOL, [w.HANDLE, c.POINTER(w.DWORD)]),
            "GetProcessTimes": (w.BOOL, [w.HANDLE] + [c.POINTER(w.FILETIME)] * 4),
            "QueryInformationJobObject": (
                w.BOOL,
                [w.HANDLE, c.c_int, c.c_void_p, w.DWORD, c.c_void_p],
            ),
            "InitializeProcThreadAttributeList": (
                w.BOOL,
                [c.c_void_p, w.DWORD, w.DWORD, c.POINTER(c.c_size_t)],
            ),
            "UpdateProcThreadAttribute": (
                w.BOOL,
                [
                    c.c_void_p,
                    w.DWORD,
                    c.c_size_t,
                    c.c_void_p,
                    c.c_size_t,
                    c.c_void_p,
                    c.c_void_p,
                ],
            ),
            "DeleteProcThreadAttributeList": (None, [c.c_void_p]),
            "GetCurrentProcess": (w.HANDLE, []),
            "GetCurrentThread": (w.HANDLE, []),
            "DuplicateHandle": (
                w.BOOL,
                [
                    w.HANDLE,
                    w.HANDLE,
                    w.HANDLE,
                    c.POINTER(w.HANDLE),
                    w.DWORD,
                    w.BOOL,
                    w.DWORD,
                ],
            ),
            "CancelSynchronousIo": (w.BOOL, [w.HANDLE]),
        }
        for name, (restype, argtypes) in signatures.items():
            function = getattr(kernel, name)
            function.restype, function.argtypes = restype, argtypes
            setattr(self, name, function)


def check(ok):
    if not ok:
        # Fixed message: no process command/environment appears in diagnostics.
        raise OSError("Owned metadata process operation failed")


def environment_block(environment):
    """Encode an explicit Windows child environment without logging input.

    UTF-16LE is independent of ctypes wchar_t width in pure encoding tests.
    Strict encoding rejects unpaired surrogates while preserving astral values.
    """
    entries, names = [], set()
    for name, value in environment.items():
        if (
            not isinstance(name, str)
            or not isinstance(value, str)
            or not re.fullmatch(r"[A-Za-z_][A-Za-z_0-9()]*", name)
            or name.upper() in names
            or "\0" in value
        ):
            raise ValueError("Invalid explicit child environment")
        names.add(name.upper())
        entries.append((name, value))
    entries.sort(key=lambda item: item[0].upper())
    try:
        return (
            "\0".join(name + "=" + value for name, value in entries) + "\0\0"
        ).encode("utf-16-le")
    except UnicodeError:
        raise ValueError("Invalid explicit child environment") from None


def capture_owned(
    command, destination, cap, timeout, *, api=None, env=None, stderr_destination=None
):
    if stderr_destination is not None:
        return capture_split_owned(
            command, destination, stderr_destination, cap, timeout, api=api, env=env
        )
    return capture_merged_owned(command, destination, cap, timeout, api=api, env=env)


def capture_merged_owned(command, destination, cap, timeout, *, api=None, env=None):
    # Validate before creating a Job, pipes or a process. Keep this byte buffer
    # alive through CreateProcessW; callers never mutate their parent environment.
    block = None if env is None else environment_block(env)
    child_block = None if block is None else c.create_string_buffer(block, len(block))
    api = WinAPI() if api is None else api
    job = api.CreateJobObjectW(None, None)
    check(job)
    pi, si, ext = PI(), SIEX(), EXT()
    ext.basic.flags = 0x2000  # KILL_ON_JOB_CLOSE; no breakaway permission.
    fds, reader, attributes = [], None, None
    assigned = False
    errors, stopped = [], threading.Event()
    result = {
        "bytes_observed": 0,
        "retained_bytes": 0,
        "oversize": False,
        "timeout": False,
        "assigned_before_resume": False,
    }
    digest = hashlib.sha256()
    deadline = time.monotonic() + timeout
    try:
        check(api.SetInformationJobObject(job, 9, c.byref(ext), c.sizeof(ext)))
        read_fd, write_fd = os.pipe()
        fds.extend((read_fd, write_fd))
        null_fd = os.open(os.devnull, os.O_RDONLY)
        fds.append(null_fd)
        for fd in (write_fd, null_fd):
            os.set_inheritable(fd, True)
        handles = (w.HANDLE * 2)(api.oshandle(write_fd), api.oshandle(null_fd))
        size = c.c_size_t()
        api.InitializeProcThreadAttributeList(None, 1, 0, c.byref(size))
        check(size.value > 0)
        attributes = c.create_string_buffer(size.value)
        check(api.InitializeProcThreadAttributeList(attributes, 1, 0, c.byref(size)))
        si.attributes = c.cast(attributes, c.c_void_p)
        # Explicit HANDLE_LIST prevents inheritance of unrelated session handles.
        check(
            api.UpdateProcThreadAttribute(
                attributes, 0, 0x20002, handles, c.sizeof(handles), None, None
            )
        )
        si.startup.cb, si.startup.dwFlags = c.sizeof(si), 0x101
        si.startup.hStdInput = handles[1]
        si.startup.hStdOutput = si.startup.hStdError = handles[0]
        command_line = c.create_unicode_buffer(subprocess.list2cmdline(command))
        check(
            api.CreateProcessW(
                command[0],
                command_line,
                None,
                None,
                True,
                0x4 | 0x08000000 | 0x80000 | (0x400 if child_block is not None else 0),
                None if child_block is None else c.cast(child_block, c.c_void_p),
                None,
                c.byref(si),
                c.byref(pi),
            )
        )
        birth = [w.FILETIME() for _ in range(4)]
        check(api.GetProcessTimes(pi.process, *[c.byref(t) for t in birth]))
        result["root_identity"] = {
            "pid": pi.pid,
            "creation_filetime": birth[0].dwLowDateTime
            + (birth[0].dwHighDateTime << 32),
        }
        check(api.AssignProcessToJobObject(job, pi.process))
        assigned = True
        member = w.BOOL()
        check(api.IsProcessInJob(pi.process, job, c.byref(member)))
        check(member.value)
        result["assigned_before_resume"] = True
        for fd in (write_fd, null_fd):
            os.close(fd)
            fds.remove(fd)

        def drain():
            try:
                with destination.open("xb") as output:
                    while chunk := os.read(read_fd, 65536):
                        result["bytes_observed"] += len(chunk)
                        if result["retained_bytes"] + len(chunk) > cap:
                            result["oversize"] = True
                            stopped.set()
                            # Continue draining until owned Job cleanup closes writers.
                            continue
                        if not result["oversize"]:
                            output.write(chunk)
                            digest.update(chunk)
                            result["retained_bytes"] += len(chunk)
            except Exception:
                errors.append("Private reader failed")
                stopped.set()

        reader = threading.Thread(target=drain)
        reader.start()
        check(api.ResumeThread(pi.thread) != 0xFFFFFFFF)
        while True:
            status = api.WaitForSingleObject(pi.process, 25)
            check(status in (0, 258))
            if status == 0:
                code = w.DWORD()
                check(api.GetExitCodeProcess(pi.process, c.byref(code)))
                result["exit_code"] = code.value
                break
            if stopped.is_set() or time.monotonic() >= deadline:
                result["timeout"] = not stopped.is_set()
                result["exit_code"] = None
                break
    finally:
        if pi.process:
            # Handles remain retained through termination and zero-active proof.
            if assigned:
                if not api.TerminateJobObject(job, 0x62):
                    errors.append("Owned Job termination failed")
            elif not api.TerminateProcess(pi.process, 0x62):
                errors.append("Suspended root termination failed")
            if api.WaitForSingleObject(pi.process, 5000) != 0:
                errors.append("Owned root did not exit")
        if assigned:
            cleanup_deadline = time.monotonic() + 5
            while True:
                account = ACCOUNT()
                if not api.QueryInformationJobObject(
                    job, 1, c.byref(account), c.sizeof(account), None
                ):
                    errors.append("Owned Job accounting failed")
                    break
                result["active_job_processes_after_cleanup"] = account.active
                if account.active == 0:
                    break
                if time.monotonic() >= cleanup_deadline:
                    errors.append("Owned Job remained active")
                    break
                time.sleep(0.01)
        # Close Job before joining readers: kill-on-close is a final safeguard.
        if not api.CloseHandle(job):
            errors.append("Owned Job close failed")
        if reader is not None:
            reader.join(5)
            if reader.is_alive():
                errors.append("Private reader remained active")
        for handle in (pi.thread, pi.process):
            if handle and not api.CloseHandle(handle):
                errors.append("Owned handle close failed")
        if attributes is not None and si.attributes:
            api.DeleteProcThreadAttributeList(attributes)
        for fd in fds:
            os.close(fd)
    check(not errors)
    result["reader_closed"] = reader is not None and not reader.is_alive()
    result["retained_sha256"] = digest.hexdigest()
    return result


# Only failed split captures can enter quarantine. Each daemon reader owns its
# descriptors until it exits; a parent must never close/reuse a live reader FD.
_QUARANTINED_READERS = []


def plain_ancestors(path, *, missing_leaf=False):
    """Inspect the lexical chain before resolve; caller trusts its owned parents."""
    if not path.is_absolute():
        raise ValueError("Private capture path must be absolute")
    for item in [*reversed(path.parents), path]:
        try:
            info = item.lstat()
        except FileNotFoundError:
            if missing_leaf and item == path:
                continue
            raise
        if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
            raise ValueError("Private capture reparse path rejected")


def quarantine_reader(state, api):
    _QUARANTINED_READERS.append(state)

    def reap():
        state["thread"].join()
        if state["thread_handle"]:
            api.CloseHandle(state["thread_handle"])
        _QUARANTINED_READERS.remove(state)

    threading.Thread(target=reap, daemon=True).start()


def capture_split_owned(
    command, destination, stderr_destination, cap, timeout, *, api=None, env=None
):
    """One Job, two pipes, one byte cap/deadline and one cleanup reservation.

    No per-stream path/hash is returned. Failure results are never evidence that
    streams are complete; callers must require operation_failed=False and both
    reader_closed and zero active owned processes before opening either file.
    """
    deadline = time.monotonic() + timeout
    plain_ancestors(destination, missing_leaf=True)
    plain_ancestors(stderr_destination, missing_leaf=True)
    check(
        destination.parent == stderr_destination.parent
        and destination != stderr_destination
    )
    block = None if env is None else environment_block(env)
    child_block = None if block is None else c.create_string_buffer(block, len(block))
    api = WinAPI() if api is None else api
    result = {
        "capture_format": "separate-v1",
        "bytes_observed": 0,
        "retained_bytes": 0,
        "oversize": False,
        "timeout": False,
        "assigned_before_resume": False,
        "exit_code": None,
        "reader_closed": False,
        "operation_failed": True,
    }
    lock, stopped = threading.Lock(), threading.Event()
    errors, states, owned_fds = [], [], set()
    pi, si, job, attributes, assigned = PI(), SIEX(), None, None, False
    reserved = 0

    def drain(state):
        nonlocal reserved
        try:
            thread_handle = w.HANDLE()
            process = api.GetCurrentProcess()
            # Duplicate this exact current thread's pseudo-handle. Never reopen
            # a numeric thread ID that can be recycled between check and use.
            check(
                api.DuplicateHandle(
                    process,
                    api.GetCurrentThread(),
                    process,
                    c.byref(thread_handle),
                    0x0001,
                    False,
                    0,
                )
            )
            check(thread_handle.value)
            state["thread_handle"] = thread_handle.value
            state["ready"].set()
            while chunk := os.read(state["read_fd"], 65536):
                with lock:
                    result["bytes_observed"] += len(chunk)
                    retain = not result["oversize"] and reserved + len(chunk) <= cap
                    if retain:
                        reserved += len(chunk)
                    else:
                        result["oversize"] = True
                        stopped.set()
                if retain:
                    written = state["output"].write(chunk)
                    with lock:
                        result["retained_bytes"] += written
                    check(written == len(chunk))
        except Exception:
            errors.append("Private split reader failed")
            stopped.set()
        finally:
            state["ready"].set()
            try:
                state["output"].close()
            except Exception:
                errors.append("Private split output close failed")
            try:
                os.close(state["read_fd"])
            except OSError:
                errors.append("Private split pipe close failed")

    try:
        for path in (destination, stderr_destination):
            output = path.open("xb", buffering=0)
            state = {
                "output": output,
                "read_fd": None,
                "write_fd": None,
                "thread": None,
                "thread_handle": None,
                "ready": threading.Event(),
            }
            states.append(state)
            state["read_fd"], state["write_fd"] = os.pipe()
            owned_fds.update((state["read_fd"], state["write_fd"]))
        null_fd = os.open(os.devnull, os.O_RDONLY)
        owned_fds.add(null_fd)
        inherited = [states[0]["write_fd"], states[1]["write_fd"], null_fd]
        for fd in inherited:
            os.set_inheritable(fd, True)
        handles = (w.HANDLE * 3)(*(api.oshandle(fd) for fd in inherited))
        job = api.CreateJobObjectW(None, None)
        check(job)
        ext = EXT()
        ext.basic.flags = 0x2000
        check(api.SetInformationJobObject(job, 9, c.byref(ext), c.sizeof(ext)))
        size = c.c_size_t()
        api.InitializeProcThreadAttributeList(None, 1, 0, c.byref(size))
        check(size.value > 0)
        attributes = c.create_string_buffer(size.value)
        check(api.InitializeProcThreadAttributeList(attributes, 1, 0, c.byref(size)))
        si.attributes = c.cast(attributes, c.c_void_p)
        check(
            api.UpdateProcThreadAttribute(
                attributes, 0, 0x20002, handles, c.sizeof(handles), None, None
            )
        )
        si.startup.cb, si.startup.dwFlags = c.sizeof(si), 0x101
        si.startup.hStdOutput, si.startup.hStdError, si.startup.hStdInput = handles
        command_line = c.create_unicode_buffer(subprocess.list2cmdline(command))
        check(
            api.CreateProcessW(
                command[0],
                command_line,
                None,
                None,
                True,
                0x4 | 0x08000000 | 0x80000 | (0x400 if child_block is not None else 0),
                None if child_block is None else c.cast(child_block, c.c_void_p),
                None,
                c.byref(si),
                c.byref(pi),
            )
        )
        birth = [w.FILETIME() for _ in range(4)]
        check(api.GetProcessTimes(pi.process, *[c.byref(t) for t in birth]))
        result["root_identity"] = {
            "pid": pi.pid,
            "creation_filetime": birth[0].dwLowDateTime
            + (birth[0].dwHighDateTime << 32),
        }
        check(api.AssignProcessToJobObject(job, pi.process))
        assigned = True
        member = w.BOOL()
        check(api.IsProcessInJob(pi.process, job, c.byref(member)))
        check(member.value)
        result["assigned_before_resume"] = True
        for fd in inherited:
            os.close(fd)
            owned_fds.remove(fd)
        for state in states:
            state["thread"] = threading.Thread(target=drain, args=(state,), daemon=True)
            state["thread"].start()
            owned_fds.remove(state["read_fd"])
        for state in states:
            check(state["ready"].wait(max(0, deadline - time.monotonic())))
            check(state["thread_handle"] and not stopped.is_set())
        check(time.monotonic() < deadline)
        check(api.ResumeThread(pi.thread) != 0xFFFFFFFF)
        while True:
            status = api.WaitForSingleObject(pi.process, 25)
            check(status in (0, 258))
            if status == 0:
                code = w.DWORD()
                check(api.GetExitCodeProcess(pi.process, c.byref(code)))
                result["exit_code"] = code.value
                break
            if stopped.is_set() or time.monotonic() >= deadline:
                result["timeout"] = not stopped.is_set()
                break
    except Exception:
        errors.append("Owned split capture operation failed")
    finally:
        # Exactly the existing 15s reservation, shared by root/accounting/readers.
        cleanup_start = time.monotonic()
        cleanup_deadline = cleanup_start + 15
        if pi.process:
            if assigned:
                if not api.TerminateJobObject(job, 0x62):
                    errors.append("Owned Job termination failed")
            elif not api.TerminateProcess(pi.process, 0x62):
                errors.append("Suspended root termination failed")
            if api.WaitForSingleObject(pi.process, 5000) != 0:
                errors.append("Owned root did not exit")
        if assigned:
            while True:
                account = ACCOUNT()
                if not api.QueryInformationJobObject(
                    job, 1, c.byref(account), c.sizeof(account), None
                ):
                    errors.append("Owned Job accounting failed")
                    break
                result["active_job_processes_after_cleanup"] = account.active
                if account.active == 0:
                    break
                if time.monotonic() >= min(cleanup_start + 10, cleanup_deadline):
                    errors.append("Owned Job remained active")
                    break
                time.sleep(0.01)
        if job and not api.CloseHandle(job):
            errors.append("Owned Job close failed")
        for state in states:
            thread = state["thread"]
            if thread is not None and thread.ident is not None:
                thread.join(max(0, cleanup_deadline - 1 - time.monotonic()))
        for state in states:
            thread = state["thread"]
            if thread is not None and thread.ident is not None:
                if thread.is_alive() and state["thread_handle"]:
                    # Cancellation is asynchronous (Microsoft ioapiset docs).
                    # This retained exact-thread handle stays valid through join.
                    try:
                        api.CancelSynchronousIo(state["thread_handle"])
                    except Exception:
                        errors.append("Private reader cancellation failed")
                thread.join(max(0, cleanup_deadline - time.monotonic()))
                if thread.is_alive():
                    errors.append("Private split reader remained active")
                    try:
                        quarantine_reader(state, api)
                    except Exception:
                        # State was retained before attempting the daemon reaper.
                        # Never close a live FD even if reaper startup fails.
                        errors.append("Private split quarantine failed")
                elif state["thread_handle"]:
                    if not api.CloseHandle(state["thread_handle"]):
                        errors.append("Private reader handle close failed")
            else:
                try:
                    state["output"].close()
                except Exception:
                    errors.append("Unstarted output close failed")
        for handle in (pi.thread, pi.process):
            if handle and not api.CloseHandle(handle):
                errors.append("Owned handle close failed")
        if attributes is not None and si.attributes:
            api.DeleteProcThreadAttributeList(attributes)
        for fd in owned_fds:
            os.close(fd)
    result["reader_closed"] = len(states) == 2 and all(
        state["thread"] is not None
        and state["thread"].ident is not None
        and not state["thread"].is_alive()
        for state in states
    )
    result["operation_failed"] = bool(errors)
    return result
