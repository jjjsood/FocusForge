#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Retrying HTTPS download for pinned tools -- stdlib only."""
from __future__ import annotations

import contextlib
import http.client
import io
import json
import os
import re
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

ATTEMPTS = 8
TIMEOUT = 30
RETRY_STATUS = frozenset({408, 429, 500, 502, 503, 504})
RELEASE_URL = re.compile(r"https://github\.com/([^/]+)/([^/]+)/releases/download/([^/]+)/([^/]+)")


def transient(error: BaseException) -> bool:
    """True when retrying may succeed: a throttling or server status, a body cut short, or a network failure."""
    if isinstance(error, urllib.error.HTTPError):
        return error.code in RETRY_STATUS
    return isinstance(error, (http.client.IncompleteRead, OSError))


def _read(request: urllib.request.Request) -> bytes:
    with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
        return response.read()


def _release_asset(url: str, token: str) -> bytes:
    """Fetch a github.com release asset through the API, sending `token` to api.github.com only."""
    owner, repo, tag, name = (urllib.parse.unquote(part) for part in RELEASE_URL.fullmatch(url).groups())

    def get(target: str, accept: str) -> bytes:
        request = urllib.request.Request(target, headers={"Accept": accept, "X-GitHub-Api-Version": "2022-11-28"})
        # An unredirected header is not copied to the host a redirect points at.
        request.add_unredirected_header("Authorization", f"Bearer {token}")
        return _read(request)

    release = f"https://api.github.com/repos/{owner}/{repo}/releases/tags/{urllib.parse.quote(tag, safe='')}"
    assets = [asset for asset in json.loads(get(release, "application/vnd.github+json")).get("assets", [])
              if asset.get("name") == name]
    if len(assets) != 1:
        raise ValueError(f"release {tag} of {owner}/{repo} has {len(assets)} assets named {name}")
    asset_url = urllib.parse.urlsplit(str(assets[0].get("url")))
    if asset_url.scheme != "https" or asset_url.netloc != "api.github.com":
        raise ValueError(f"asset {name} of release {tag} is not served from api.github.com")
    return get(asset_url.geturl(), "application/octet-stream")


def download(url: str, token: str | None = None) -> bytes:
    """Read `url`, retrying transient failures with growing pauses and failing at once on anything else.

    With a `token`, a github.com release asset is fetched through the API under the job's quota.
    Both API requests share the attempt budget. An empty body is an error.
    """
    for attempt in range(ATTEMPTS):
        try:
            if token and RELEASE_URL.fullmatch(url):
                body = _release_asset(url, token)
            else:
                body = _read(urllib.request.Request(url))
        except (OSError, http.client.HTTPException) as error:
            if not transient(error):
                raise
            if attempt == ATTEMPTS - 1:
                raise OSError(f"download of {url} failed after {ATTEMPTS} attempts: {error}") from error
            delay = min(90, 5 * 2**attempt)
            print(f"download of {url} failed ({error}); retrying in {delay:g}s", file=sys.stderr)
            time.sleep(delay)
        else:
            if not body:
                raise ValueError(f"download of {url} returned an empty body")
            return body
    raise AssertionError("unreachable: the last attempt returns or raises")


def _selftest() -> int:
    class Reply(io.BytesIO):
        def __enter__(self) -> "Reply":
            return self

        def __exit__(self, *args: object) -> None:
            self.close()

    class Truncated(Reply):
        def read(self, *args: object) -> bytes:
            raise http.client.IncompleteRead(b"par", 10)

    def failure(code: int) -> urllib.error.HTTPError:
        return urllib.error.HTTPError("https://example.invalid/x", code, "stub", {}, None)

    class Run:
        """Scripted opener: each outcome is raised (an exception) or served (bytes or a reply)."""

        def __init__(self, *outcomes: object) -> None:
            self.script = list(outcomes)
            self.requests: list[urllib.request.Request] = []
            self.timeouts: list[float] = []
            self.sleeps: list[float] = []
            self.log = io.StringIO()

        def call(self, *args: object, **kwargs: object) -> object:
            saved = urllib.request.urlopen, time.sleep
            urllib.request.urlopen, time.sleep = self.urlopen, self.sleeps.append
            try:
                with contextlib.redirect_stderr(self.log):
                    try:
                        return download(*args, **kwargs)
                    except (OSError, ValueError) as error:
                        return error
            finally:
                urllib.request.urlopen, time.sleep = saved

        def urlopen(self, request: urllib.request.Request, timeout: float) -> Reply:
            self.requests.append(request)
            self.timeouts.append(timeout)
            outcome = self.script.pop(0)
            if isinstance(outcome, BaseException):
                raise outcome
            return outcome if isinstance(outcome, Reply) else Reply(outcome)

        def urls(self) -> list[str]:
            return [request.full_url for request in self.requests]

    url = "https://example.invalid/tool.tar.gz"

    run = Run(failure(503), TimeoutError("slow"), urllib.error.URLError("offline"), b"payload")
    assert run.call(url) == b"payload"
    assert run.urls() == [url] * 4 and run.sleeps == [5, 10, 20] and set(run.timeouts) == {30}, (run.urls(), run.sleeps)
    lines = run.log.getvalue().splitlines()
    assert len(lines) == 3 and all(f"download of {url} failed (" in line and "; retrying in " in line for line in lines), lines
    assert lines[0].endswith("retrying in 5s") and "503" in lines[0], lines

    for code in (408, 429, 500, 502, 504):
        run = Run(failure(code), b"x")
        assert run.call(url) == b"x" and run.sleeps == [5], (code, run.sleeps)
    run = Run(Truncated(b""), ConnectionResetError("reset"), b"whole")
    assert run.call(url) == b"whole" and run.sleeps == [5, 10], run.sleeps

    for code in (400, 401, 403, 404, 410, 501):
        run = Run(failure(code), b"never served")
        result = run.call(url)
        assert isinstance(result, urllib.error.HTTPError) and result.code == code, (code, result)
        assert len(run.requests) == 1 and not run.sleeps and not run.log.getvalue(), code
    run = Run(b"", b"never served")
    result = run.call(url)
    assert isinstance(result, ValueError) and url in str(result), result
    assert len(run.requests) == 1 and not run.sleeps

    run = Run(*[failure(504)] * 9)
    result = run.call(url)
    assert type(result) is OSError and isinstance(result.__cause__, urllib.error.HTTPError), result
    assert url in str(result) and "504" in str(result) and "8 attempts" in str(result), result
    assert len(run.requests) == 8 and run.sleeps == [5, 10, 20, 40, 80, 90, 90] and sum(run.sleeps) == 335, run.sleeps
    assert len(run.log.getvalue().splitlines()) == 7

    token = "ghs_secretvalue123"
    release = {"assets": [{"name": "other.zip", "url": "https://api.github.com/repos/o/r/releases/assets/1"},
                          {"name": "tool.tar.gz", "url": "https://api.github.com/repos/o/r/releases/assets/22"}]}
    listing = json.dumps(release).encode()

    def api(run: Run) -> None:
        for request in run.requests:
            assert request.unredirected_hdrs.get("Authorization") == f"Bearer {token}", request.unredirected_hdrs
            assert "Authorization" not in request.headers, request.headers

    for tag, name in (("v0.5.4", "tool.tar.gz"), ("cargo-nextest-0.9.144", "tool.tar.gz")):
        release["assets"][1]["name"] = name
        run = Run(failure(502), json.dumps(release).encode(), b"bytes of " + tag.encode())
        result = run.call(f"https://github.com/o/r/releases/download/{tag}/{name}", token)
        assert result == b"bytes of " + tag.encode(), result
        assert run.urls() == [f"https://api.github.com/repos/o/r/releases/tags/{tag}",
                              f"https://api.github.com/repos/o/r/releases/tags/{tag}",
                              "https://api.github.com/repos/o/r/releases/assets/22"], run.urls()
        assert run.sleeps == [5], run.sleeps
        api(run)
        assert run.requests[1].get_header("Accept") == "application/vnd.github+json"
        assert run.requests[1].get_header("X-github-api-version") == "2022-11-28"
        assert run.requests[2].get_header("Accept") == "application/octet-stream"
        assert token not in run.log.getvalue() and "Bearer" not in run.log.getvalue(), run.log.getvalue()
    run = Run(b"{}", b"never served")
    assert isinstance(run.call("https://github.com/o/r/releases/download/a%2Fb/t.zip", token), ValueError)
    assert run.urls() == ["https://api.github.com/repos/o/r/releases/tags/a%2Fb"], run.urls()

    release["assets"][1]["name"] = "tool.tar.gz"
    for label, body in (("missing", json.dumps({"assets": []}).encode()),
                        ("duplicated", json.dumps({"assets": release["assets"][1:] * 2}).encode()),
                        ("off host", json.dumps({"assets": [{"name": "tool.tar.gz", "url": "https://evil.invalid/a"}]}).encode()),
                        ("http", json.dumps({"assets": [{"name": "tool.tar.gz", "url": "http://api.github.com/a"}]}).encode())):
        run = Run(body, b"never served")
        result = run.call("https://github.com/o/r/releases/download/v1/tool.tar.gz", token)
        assert isinstance(result, ValueError), (label, result)
        assert len(run.requests) == 1 and not run.sleeps, label
        assert label in ("off host", "http") or "tool.tar.gz" in str(result), (label, result)
        assert token not in str(result)

    run = Run(b"plain")
    assert run.call("https://github.com/o/r/releases/download/v1/tool.tar.gz") == b"plain"
    assert run.urls() == ["https://github.com/o/r/releases/download/v1/tool.tar.gz"]
    assert not run.requests[0].headers.get("Authorization") and not run.requests[0].unredirected_hdrs
    for other in ("https://example.invalid/tool.tar.gz", "https://github.com/o/r/archive/v1.tar.gz"):
        run = Run(b"plain")
        assert run.call(other, token) == b"plain" and run.urls() == [other]
        assert not run.requests[0].unredirected_hdrs and not run.requests[0].headers

    request = urllib.request.Request("https://api.github.com/repos/o/r/releases/assets/22")
    request.add_unredirected_header("Authorization", f"Bearer {token}")
    request.add_header("Accept", "application/octet-stream")
    followed = urllib.request.HTTPRedirectHandler().redirect_request(
        request, io.BytesIO(), 302, "Found", {}, "https://release-assets.example.invalid/blob")
    assert followed is not None and followed.get_header("Authorization") is None, followed.headers
    assert followed.get_header("Accept") == "application/octet-stream"

    def cli(*argv: str, env: dict[str, str] | None = None, outcomes: tuple = ()) -> tuple[int, str, Run, Path]:
        run = Run(*outcomes)
        saved_env = {key: os.environ.pop(key, None) for key in ("GH_TOKEN", "GITHUB_TOKEN")}
        os.environ.update(env or {})
        saved = urllib.request.urlopen, time.sleep
        urllib.request.urlopen, time.sleep = run.urlopen, run.sleeps.append
        try:
            with tempfile.TemporaryDirectory() as directory:
                out = Path(directory) / "out.bin"
                with contextlib.redirect_stderr(run.log):
                    code = main(["download.py", *[arg.replace("OUT", str(out)) for arg in argv]])
                return code, run.log.getvalue(), run, (out.read_bytes() if out.exists() else None)
        finally:
            urllib.request.urlopen, time.sleep = saved
            for key in ("GH_TOKEN", "GITHUB_TOKEN"):
                os.environ.pop(key, None)
                if saved_env[key] is not None:
                    os.environ[key] = saved_env[key]

    code, log, run, written = cli(url, "OUT", outcomes=(b"file",))
    assert (code, written, log) == (0, b"file", "") and not run.requests[0].unredirected_hdrs
    code, log, run, written = cli(url, "OUT", outcomes=(failure(404),))
    assert code == 1 and written is None and len(log.splitlines()) == 1 and url in log and "404" in log, (code, log)
    code, log, run, written = cli(url, "OUT", outcomes=(b"",))
    assert code == 1 and len(log.splitlines()) == 1 and url in log, (code, log)
    code, log, run, written = cli(url, "OUT", outcomes=[failure(504)] * 8)
    assert code == 1 and len(log.splitlines()) == 8 and "8 attempts" in log.splitlines()[-1], log
    for argv in ((), (url,), (url, "OUT", "extra"), ("--bogus", "OUT")):
        code, log, run, written = cli(*argv)
        assert code == 2 and not run.requests and "usage" in log, (argv, code, log)
    release_url = "https://github.com/o/r/releases/download/v1/tool.tar.gz"
    for env, expected in (({"GITHUB_TOKEN": "second"}, "second"), ({"GH_TOKEN": "first", "GITHUB_TOKEN": "second"}, "first")):
        code, log, run, written = cli(release_url, "OUT", env=env, outcomes=(listing, b"asset"))
        assert code == 0 and written == b"asset", (code, log)
        assert {r.unredirected_hdrs["Authorization"] for r in run.requests} == {f"Bearer {expected}"}
        assert "first" not in log and "second" not in log

    print("OK: download retries transient failures only, within a fixed bound, and keeps the token off redirects")
    return 0


def main(argv: list[str]) -> int:
    if len(argv) == 2 and argv[1] == "--selftest":
        return _selftest()
    if len(argv) != 3 or argv[1].startswith("-"):
        print("usage: download.py URL OUT | --selftest", file=sys.stderr)
        return 2
    url, out = argv[1:]
    try:
        Path(out).write_bytes(download(url, os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")))
    except (OSError, ValueError, http.client.HTTPException) as error:
        print(error if url in str(error) else f"download of {url} failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
