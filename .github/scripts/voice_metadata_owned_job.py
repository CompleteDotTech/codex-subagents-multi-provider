"""Windows-only private query capture in an owned, non-breakaway Job.

No process lookup or numeric PID termination. The only uncontained process is
our suspended creation handle; assignment and identity checks precede resume.
"""

import ctypes as c
from ctypes import wintypes as w
import hashlib
import os
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
        }
        for name, (restype, argtypes) in signatures.items():
            function = getattr(kernel, name)
            function.restype, function.argtypes = restype, argtypes
            setattr(self, name, function)


def check(ok):
    if not ok:
        # Fixed message: no process command/environment appears in diagnostics.
        raise OSError("Owned metadata process operation failed")


def capture_owned(command, destination, cap, timeout, *, api=None):
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
                0x4 | 0x08000000 | 0x80000,
                None,
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
