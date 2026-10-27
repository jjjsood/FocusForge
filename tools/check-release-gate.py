#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Require the latest exact-SHA native gate before a release can publish."""
from __future__ import annotations

import argparse
import datetime
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

EXPECTED_JOBS = {"ubuntu-24.04", "macos-15", "windows-2022"}
WORKFLOW = ".github/workflows/check.yml"



def load(path: Path) -> dict:
    try:
        value = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read {path}: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"{path}: expected JSON object")
    return value


def rows(payload: dict, key: str) -> list[dict] | None:
    pages = payload.get("pages", [payload])
    if not isinstance(pages, list):
        return None
    result = []
    for page in pages:
        if not isinstance(page, dict) or not isinstance(page.get(key), list):
            return None
        if not all(isinstance(row, dict) for row in page[key]):
            return None
        result.extend(page[key])
    if "total_count" in payload:
        count = payload["total_count"]
        if isinstance(count, bool) or not isinstance(count, int) or count != len(result):
            return None
    return result


def latest_run(runs: dict, sha: str) -> dict | None:
    all_runs = rows(runs, "workflow_runs")
    if all_runs is None:
        return None
    candidates = [run for run in all_runs if run.get("path") == WORKFLOW
                  and run.get("event") == "push" and run.get("head_sha") == sha]
    if any(not positive_id(run.get("id")) or not positive_id(run.get("run_attempt"))
           or not timestamp(run.get("created_at"))
           for run in candidates):
        return None
    return max(candidates, key=lambda run: (run.get("created_at", ""), run.get("id", 0)), default=None)


def positive_id(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def timestamp(value: object) -> bool:
    if not isinstance(value, str) or not value:
        return False
    try:
        datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return False
    return True


def approved(runs: dict, jobs: dict, sha: str) -> bool:
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        return False
    run = latest_run(runs, sha)
    if not run or run.get("status") != "completed" or run.get("conclusion") != "success":
        return False
    if not positive_id(run.get("id")) or not positive_id(run.get("run_attempt")):
        return False
    if not timestamp(run.get("created_at")) or run.get("head_sha") != sha:
        return False
    if jobs.get("workflow_run_id") != run.get("id"):
        return False
    if jobs.get("head_sha") != sha or jobs.get("run_attempt") != run.get("run_attempt"):
        return False
    all_jobs = rows(jobs, "jobs")
    if all_jobs is None:
        return False
    expected = {name: [] for name in EXPECTED_JOBS}
    for job in all_jobs:
        if job.get("name") in expected:
            expected[job["name"]].append(job)
    return all(len(group) == 1 and positive_id(group[0].get("run_id")) and group[0].get("run_id") == run["id"]
               and group[0].get("head_sha") == sha
               and positive_id(group[0].get("run_attempt", run.get("run_attempt")))
               and group[0].get("run_attempt", run.get("run_attempt")) == run.get("run_attempt")
               and group[0].get("status") == "completed"
               and group[0].get("conclusion") == "success"
               for group in expected.values())


def retry_delay(error: OSError, attempt: int) -> float | None:
    """Seconds to wait before retrying after `error`, or None when it is permanent.

    Rate limits (403 with an exhausted quota or a Retry-After, 429), server errors
    and network failures are transient; every other HTTP status is not.
    """
    if isinstance(error, urllib.error.HTTPError):
        headers = error.headers or {}
        retry_after, reset = headers.get("Retry-After"), headers.get("X-RateLimit-Reset")
        limited = error.code == 429 or (error.code == 403 and (headers.get("X-RateLimit-Remaining") == "0" or retry_after))
        if not (limited or error.code >= 500):
            return None
        try:
            if retry_after:
                return max(1.0, float(retry_after))
            if reset and limited:
                return max(1.0, float(reset) - time.time() + 1)
        except ValueError:
            pass
    return min(90.0, 5.0 * 2 ** attempt)


def request_pages(url: str, deadline: float, use_token: bool) -> dict:
    pages = []
    seen = set()
    total_count = None
    while url:
        parsed = urllib.parse.urlparse(url)
        if parsed.scheme != "https" or parsed.netloc != "api.github.com" or url in seen:
            raise ValueError("invalid pagination link")
        seen.add(url)
        headers = {"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28"}
        token = (os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")) if use_token else None
        if token:
            headers["Authorization"] = f"Bearer {token}"
        attempt, last_error = 0, None
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ValueError("release gate timed out" + (f" waiting out: {last_error}" if last_error else ""))
            try:
                with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=min(20, remaining)) as response:
                    page = json.load(response)
                    links = response.headers.get("Link", "")
            except OSError as error:
                delay = retry_delay(error, attempt)
                if delay is None:
                    raise
                attempt, last_error = attempt + 1, error
                time.sleep(min(delay, max(0, deadline - time.monotonic())))
                continue
            break
        if not isinstance(page, dict) or not isinstance(page.get("total_count"), int) or isinstance(page.get("total_count"), bool):
            raise ValueError("malformed pagination response")
        if total_count is None:
            total_count = page["total_count"]
        elif total_count != page["total_count"]:
            raise ValueError("inconsistent pagination count")
        pages.append(page)
        url = next((part.split(";", 1)[0].strip(" <>") for part in links.split(",") if 'rel="next"' in part), "")
    return {"pages": pages, "total_count": total_count}


def remote(repository: str, sha: str, wait_seconds: int, use_token: bool) -> tuple[dict, dict]:
    deadline = time.monotonic() + wait_seconds
    while True:
        runs = request_pages(f"https://api.github.com/repos/{repository}/actions/workflows/check.yml/runs?event=push&head_sha={urllib.parse.quote(sha)}&per_page=100", deadline, use_token)
        run = latest_run(runs, sha)
        if not run or run.get("status") == "completed" or time.monotonic() >= deadline:
            if not run:
                return runs, {"workflow_run_id": None, "pages": []}
            if not positive_id(run.get("id")) or not positive_id(run.get("run_attempt")):
                return runs, {"workflow_run_id": None, "pages": []}
            jobs = request_pages(f"https://api.github.com/repos/{repository}/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs?per_page=100", deadline, use_token)
            jobs["workflow_run_id"] = run["id"]
            jobs["head_sha"] = sha
            jobs["run_attempt"] = run["run_attempt"]
            return runs, jobs
        # A 90 s cadence stays far below the 60 requests per hour of an anonymous read.
        time.sleep(min(90, max(0, deadline - time.monotonic())))


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--sha", required=True)
    parser.add_argument("--runs", type=Path)
    parser.add_argument("--jobs", type=Path)
    parser.add_argument("--repository")
    parser.add_argument("--wait-seconds", type=int, default=3600)
    parser.add_argument("--anonymous", action="store_true")
    args = parser.parse_args(argv)
    try:
        if args.runs and args.jobs:
            runs, jobs = load(args.runs), load(args.jobs)
        elif args.repository and not args.runs and not args.jobs:
            runs, jobs = remote(args.repository, args.sha, args.wait_seconds, not args.anonymous)
        else:
            raise ValueError("supply both --runs/--jobs or --repository")
        ok = approved(runs, jobs, args.sha)
    except (ValueError, OSError, urllib.error.URLError, json.JSONDecodeError) as error:
        print(f"release gate refused: {error}", file=sys.stderr)
        return 2
    if not ok:
        print("release gate refused: no exact complete Ubuntu/macOS/Windows check success", file=sys.stderr)
        return 1
    print(f"release gate accepted: {args.sha}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
