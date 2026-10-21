#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Run a command while holding one portable advisory file lock."""

import os
import math
import shutil
import subprocess
import sys
import time
from pathlib import Path


def lock_file(handle):
    if os.name == "nt":
        import msvcrt

        handle.seek(0, os.SEEK_END)
        if handle.tell() == 0:
            handle.write(b"\0")
            handle.flush()
        handle.seek(0)
        msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
    else:
        import fcntl

        fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)


def unlock_file(handle):
    if os.name == "nt":
        import msvcrt

        handle.seek(0)
        msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
    else:
        import fcntl

        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def run_windows_command(command):
    import ctypes
    import msvcrt
    from ctypes import wintypes

    class BasicLimitInformation(ctypes.Structure):
        _fields_ = [
            ("PerProcessUserTimeLimit", ctypes.c_longlong),
            ("PerJobUserTimeLimit", ctypes.c_longlong),
            ("LimitFlags", wintypes.DWORD),
            ("MinimumWorkingSetSize", ctypes.c_size_t),
            ("MaximumWorkingSetSize", ctypes.c_size_t),
            ("ActiveProcessLimit", wintypes.DWORD),
            ("Affinity", ctypes.c_size_t),
            ("PriorityClass", wintypes.DWORD),
            ("SchedulingClass", wintypes.DWORD),
        ]

    class IoCounters(ctypes.Structure):
        _fields_ = [(name, ctypes.c_ulonglong) for name in (
            "ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
            "ReadTransferCount", "WriteTransferCount", "OtherTransferCount",
        )]

    class ExtendedLimitInformation(ctypes.Structure):
        _fields_ = [
            ("BasicLimitInformation", BasicLimitInformation),
            ("IoInfo", IoCounters),
            ("ProcessMemoryLimit", ctypes.c_size_t),
            ("JobMemoryLimit", ctypes.c_size_t),
            ("PeakProcessMemoryUsed", ctypes.c_size_t),
            ("PeakJobMemoryUsed", ctypes.c_size_t),
        ]

    class StartupInfo(ctypes.Structure):
        _fields_ = [
            ("cb", wintypes.DWORD), ("lpReserved", wintypes.LPWSTR),
            ("lpDesktop", wintypes.LPWSTR), ("lpTitle", wintypes.LPWSTR),
            ("dwX", wintypes.DWORD), ("dwY", wintypes.DWORD),
            ("dwXSize", wintypes.DWORD), ("dwYSize", wintypes.DWORD),
            ("dwXCountChars", wintypes.DWORD), ("dwYCountChars", wintypes.DWORD),
            ("dwFillAttribute", wintypes.DWORD), ("dwFlags", wintypes.DWORD),
            ("wShowWindow", wintypes.WORD), ("cbReserved2", wintypes.WORD),
            ("lpReserved2", ctypes.POINTER(ctypes.c_byte)),
            ("hStdInput", wintypes.HANDLE), ("hStdOutput", wintypes.HANDLE),
            ("hStdError", wintypes.HANDLE),
        ]

    class ProcessInformation(ctypes.Structure):
        _fields_ = [
            ("hProcess", wintypes.HANDLE), ("hThread", wintypes.HANDLE),
            ("dwProcessId", wintypes.DWORD), ("dwThreadId", wintypes.DWORD),
        ]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateJobObjectW.argtypes = (wintypes.LPVOID, wintypes.LPCWSTR)
    kernel32.CreateJobObjectW.restype = wintypes.HANDLE
    kernel32.SetInformationJobObject.argtypes = (wintypes.HANDLE, wintypes.INT, wintypes.LPVOID, wintypes.DWORD)
    kernel32.SetInformationJobObject.restype = wintypes.BOOL
    kernel32.AssignProcessToJobObject.argtypes = (wintypes.HANDLE, wintypes.HANDLE)
    kernel32.AssignProcessToJobObject.restype = wintypes.BOOL
    kernel32.CreateProcessW.argtypes = (
        wintypes.LPCWSTR, wintypes.LPWSTR, wintypes.LPVOID, wintypes.LPVOID,
        wintypes.BOOL, wintypes.DWORD, wintypes.LPVOID, wintypes.LPCWSTR,
        ctypes.POINTER(StartupInfo), ctypes.POINTER(ProcessInformation),
    )
    kernel32.CreateProcessW.restype = wintypes.BOOL
    kernel32.ResumeThread.argtypes = (wintypes.HANDLE,)
    kernel32.ResumeThread.restype = wintypes.DWORD
    kernel32.TerminateProcess.argtypes = (wintypes.HANDLE, wintypes.UINT)
    kernel32.TerminateProcess.restype = wintypes.BOOL
    kernel32.WaitForSingleObject.argtypes = (wintypes.HANDLE, wintypes.DWORD)
    kernel32.WaitForSingleObject.restype = wintypes.DWORD
    kernel32.GetExitCodeProcess.argtypes = (wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD))
    kernel32.GetExitCodeProcess.restype = wintypes.BOOL
    kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)
    kernel32.CloseHandle.restype = wintypes.BOOL
    job = kernel32.CreateJobObjectW(None, None)
    if not job:
        raise ctypes.WinError(ctypes.get_last_error())
    child = ProcessInformation()
    try:
        limits = ExtendedLimitInformation()
        limits.BasicLimitInformation.LimitFlags = 0x00002000  # KILL_ON_JOB_CLOSE
        if not kernel32.SetInformationJobObject(job, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
            raise ctypes.WinError(ctypes.get_last_error())
        shell = os.environ.get("FF_LOCK_SHELL") or shutil.which("bash.exe") or shutil.which("bash")
        if not shell:
            raise FileNotFoundError("with_lock: cannot find Bash for native lock command")
        command_line = ctypes.create_unicode_buffer(subprocess.list2cmdline([shell, "-c", 'exec "$@"', "native-lock", *command]))
        startup = StartupInfo()
        startup.cb = ctypes.sizeof(startup)
        startup.dwFlags = 0x00000100  # STARTF_USESTDHANDLES
        handles = [msvcrt.get_osfhandle(stream.fileno()) for stream in (sys.stdin, sys.stdout, sys.stderr)]
        startup.hStdInput, startup.hStdOutput, startup.hStdError = handles
        inherited = [os.get_handle_inheritable(handle) for handle in handles]
        for handle, is_inheritable in zip(handles, inherited):
            if not is_inheritable:
                os.set_handle_inheritable(handle, True)
        created = kernel32.CreateProcessW(shell, command_line, None, None, True, 0x00000004, None, None, ctypes.byref(startup), ctypes.byref(child))
        error = ctypes.get_last_error()
        for handle, is_inheritable in zip(handles, inherited):
            if not is_inheritable:
                os.set_handle_inheritable(handle, False)
        if not created:
            raise ctypes.WinError(error)
        if not kernel32.AssignProcessToJobObject(job, child.hProcess):
            error = ctypes.get_last_error()
            kernel32.TerminateProcess(child.hProcess, 1)
            kernel32.WaitForSingleObject(child.hProcess, 0xFFFFFFFF)
            raise ctypes.WinError(error)
        if kernel32.ResumeThread(child.hThread) == 0xFFFFFFFF:
            error = ctypes.get_last_error()
            kernel32.TerminateProcess(child.hProcess, 1)
            kernel32.WaitForSingleObject(child.hProcess, 0xFFFFFFFF)
            raise ctypes.WinError(error)
        kernel32.WaitForSingleObject(child.hProcess, 0xFFFFFFFF)
        status = wintypes.DWORD()
        if not kernel32.GetExitCodeProcess(child.hProcess, ctypes.byref(status)):
            raise ctypes.WinError(ctypes.get_last_error())
        return status.value
    finally:
        if child.hThread:
            kernel32.CloseHandle(child.hThread)
        if child.hProcess:
            kernel32.CloseHandle(child.hProcess)
        kernel32.CloseHandle(job)


def parse_timeout(timeout_text):
    try:
        value = float(timeout_text)
    except ValueError:
        print(f"with_lock: invalid timeout {timeout_text!r}", file=sys.stderr)
        return None
    if not math.isfinite(value) or value < 0:
        print(f"with_lock: invalid timeout {timeout_text!r}", file=sys.stderr)
        return None
    return value


def acquire_lock(handle, path, timeout, timeout_text):
    started = time.monotonic()
    waiting = False
    while True:
        try:
            lock_file(handle)
            return True
        except OSError:
            if not waiting:
                print(f">>> waiting for lock ({path}) -- another process holds it", file=sys.stderr)
                waiting = True
            if time.monotonic() - started >= timeout:
                print(f"with_lock: timed out after {timeout_text}s waiting for lock ({path})", file=sys.stderr)
                return False
            time.sleep(0.05)


def cleanup_owner(argv):
    if len(argv) != 5:
        return 2
    _mode, path, timeout_text, owner, token = argv
    timeout = parse_timeout(timeout_text)
    if timeout is None:
        return 2
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "a+b") as handle:
        if not acquire_lock(handle, path, timeout, timeout_text):
            return 4
        try:
            try:
                owner_text = Path(owner).read_text(encoding="utf-8")
            except FileNotFoundError:
                return 0
            except OSError as error:
                print(f"with_lock: cannot read owner marker {owner}: {error}", file=sys.stderr)
                return 2
            try:
                _pid, owner_token = owner_text.split(maxsplit=1)
            except ValueError:
                return 0
            if owner_token.strip() == token:
                try:
                    Path(owner).unlink()
                except FileNotFoundError:
                    pass
                except OSError as error:
                    print(f"with_lock: cannot remove owner marker {owner}: {error}", file=sys.stderr)
                    return 2
        finally:
            unlock_file(handle)
    return 0


def main(argv):
    if len(argv) > 1 and argv[1] == "--cleanup":
        return cleanup_owner(argv[1:])
    if len(argv) < 5 or argv[3] != "--":
        return 2
    path, timeout_text = argv[1:3]
    timeout = parse_timeout(timeout_text)
    if timeout is None:
        return 2
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "a+b") as handle:
        if not acquire_lock(handle, path, timeout, timeout_text):
            return 4
        owner = os.environ.get("FF_LOCK_OWNER")
        if owner:
            token = os.environ.get("FF_LOCK_OWNER_TOKEN", "")
            Path(owner).write_text(f"{os.getpid()} {token}\n", encoding="utf-8")
        try:
            if os.name == "nt":
                return run_windows_command(argv[4:])
            os.set_inheritable(handle.fileno(), True)
            os.execvpe(argv[4], argv[4:], os.environ)
        finally:
            unlock_file(handle)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
