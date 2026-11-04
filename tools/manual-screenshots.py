#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Regenerate the manual's window screenshots by driving the FocusForge window over Slint's MCP server.

Every launch runs under its own Xvfb display with a fixed window size, scale factor 1,
fresh scratch config/data/save folders under .scratch/ and a free MCP port. Nothing
touches the user's own config. Needs: a GUI built with
`cargo build -p focusforge-gui --features mcp`, Xvfb, Python 3.11+ and Pillow. On a
machine without a GPU a software Vulkan driver (Mesa lavapipe) is enough; the tool sets
SLINT_WGPU_CPU=1.

Regenerate every screenshot (about 45 minutes with software rendering):

  python3 tools/manual-screenshots.py run
  python3 tools/manual-screenshots.py --selftest      # no display, no GUI: checks the shot lists

Interactive sessions (for exploring a state before writing a shot entry):

  tools/manual-screenshots.py start NAME [--size 1280x800] -- <focusforge-gui args...>
  tools/manual-screenshots.py NAME wait-id   ELEMENT_ID [TIMEOUT_S]
  tools/manual-screenshots.py NAME wait-text TEXT [TIMEOUT_S]          # any element whose label/value contains TEXT
  tools/manual-screenshots.py NAME click-id  ELEMENT_ID [INDEX]
  tools/manual-screenshots.py NAME click-label TEXT [UNDER_ELEMENT_ID] # Button-ish element with that exact label
  tools/manual-screenshots.py NAME double-click-id ELEMENT_ID [INDEX]
  tools/manual-screenshots.py NAME right-click-id ELEMENT_ID [INDEX]
  tools/manual-screenshots.py NAME drag-id   ELEMENT_ID X Y [INDEX]    # drag to window point X,Y (logical px)
  tools/manual-screenshots.py NAME key  KEY                            # Escape Return Tab Up Down Left Right Delete Backspace, or literal text
  tools/manual-screenshots.py NAME type TEXT                           # sends each character
  tools/manual-screenshots.py NAME set-value ELEMENT_ID VALUE [INDEX]  # accessible value (sliders, line edits)
  tools/manual-screenshots.py NAME tree [ELEMENT_ID] [--all]           # ids/types/labels (default: elements with an id or label)
  tools/manual-screenshots.py NAME find TEXT                           # elements whose id/label/value contains TEXT
  tools/manual-screenshots.py NAME props ELEMENT_ID [INDEX]
  tools/manual-screenshots.py NAME windows
  tools/manual-screenshots.py NAME shot OUT.png [--window N] [--crop ELEMENT_ID[:INDEX]] [--pad PX] [--raw]
  tools/manual-screenshots.py NAME pause MS
  tools/manual-screenshots.py stop NAME
  tools/manual-screenshots.py stop-all

Batch mode (regenerates images from shot-list TOML files):

  tools/manual-screenshots.py run [SHOTLIST.toml ...] [--only NAME ...] [--reuse-seed] [--checkout DIR]
                       # no SHOTLIST: every tools/manual-screenshots/*.toml
  tools/manual-screenshots.py seed      # (re)build .scratch/Mantis/mantis-512-fused.ffp: mantis-512 fused, kept, saved

  A shot whose args name .scratch/Mantis/mantis-512-fused.ffp opens an already fused
  project in seconds instead of fusing for a minute or more. `run` rebuilds the seed once at the
  start when any selected shot uses it (--reuse-seed skips that when the seed exists).

Shot list format (TOML):

  [[shot]]
  name  = "main-window"
  args  = ["--open", "fixtures/mantis-512/frames"]   # relative paths resolve in the checkout
  size  = "1280x800"                                  # optional, default 1280x800
  steps = [["wait-id", "FFToolbar::fuse-button"], ["click-label", "Keep"], ["pause", "500"]]
                                                      # any step may end with "--window", "N" (default 0)
  out   = "assets/manual/first-stack/main-window.png" # relative to the checkout's docs/
  crop  = "FFToolbar"                                 # optional element id[:index] to crop to
  pad   = 8                                           # optional padding around the crop
  window = 0                                          # optional window index for the capture
  env   = { FOO = "1" }                               # optional extra environment
  config = "path/to/settings.toml"                    # optional seed copied to <config>/focusforge/settings.toml
  data_seed = "path/to/dir"                           # optional tree copied into the scratch data dir
                                                      # (config/data_seed: relative paths resolve beside the shot list)
  # In env values and step arguments, {checkout} is the app checkout and {run} this shot's scratch folder.
  max_kb = 300                                        # optional budget override

A shot fails by name when the window never answers, a step names an element that does
not exist, or the converter fails. The previous image at that path is left unchanged,
the GUI is killed, and the run stops.
"""

from __future__ import annotations

import io
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SHOT_LISTS = REPO / "tools" / "manual-screenshots"
DEFAULT_CHECKOUT = REPO
DEFAULT_SIZE = "1280x800"
MAX_WIDTH = 1600
DEFAULT_MAX_KB = 300

KEYS = {
    "Escape": "\u001b", "Esc": "\u001b", "Return": "\n", "Enter": "\n", "Tab": "\t",
    "Backspace": "\u0008", "Delete": "\u007f", "Space": " ",
    "Up": "", "Down": "", "Left": "", "Right": "",
    "Home": "", "End": "", "PageUp": "", "PageDown": "",
}


class ShotError(RuntimeError):
    pass


def sessions_dir(checkout: Path) -> Path:
    d = checkout / ".scratch" / "manual-shots" / "sessions"
    d.mkdir(parents=True, exist_ok=True)
    return d


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def free_display() -> int:
    import random
    candidates = list(range(100, 900))
    random.shuffle(candidates)
    for n in candidates:
        if not Path(f"/tmp/.X11-unix/X{n}").exists() and not Path(f"/tmp/.X{n}-lock").exists():
            return n
    raise ShotError("no free X display number")


# --------------------------------------------------------------------------- MCP client


class Mcp:
    def __init__(self, port: int) -> None:
        self.url = f"http://127.0.0.1:{port}/mcp"
        self.next_id = 1

    def call(self, tool: str, args: dict | None = None, timeout: float = 30.0):
        body = json.dumps({"jsonrpc": "2.0", "id": self.next_id, "method": "tools/call",
                           "params": {"name": tool, "arguments": args or {}}}).encode()
        self.next_id += 1
        req = urllib.request.Request(self.url, data=body, headers={"Content-Type": "application/json",
                                                                    "Accept": "application/json, text/event-stream"})
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(req, timeout=timeout) as resp:
            raw = resp.read().decode()
        if raw.startswith("event:") or raw.startswith("data:"):
            raw = "\n".join(l[5:].strip() for l in raw.splitlines() if l.startswith("data:"))
        msg = json.loads(raw)
        if "error" in msg:
            raise ShotError(f"{tool}: {msg['error'].get('message')}")
        result = msg["result"]
        if result.get("isError"):
            text = " ".join(c.get("text", "") for c in result.get("content", []))
            raise ShotError(f"{tool}: {text}")
        return result

    def json(self, tool: str, args: dict | None = None):
        result = self.call(tool, args)
        for c in result.get("content", []):
            if c.get("type") == "text":
                try:
                    return json.loads(c["text"])
                except json.JSONDecodeError:
                    return c["text"]
        return result

    def alive(self) -> bool:
        try:
            self.json("list_windows")
            return True
        except Exception:
            return False

    # ---- helpers
    def windows(self) -> list:
        r = self.json("list_windows")
        return r.get("windowHandles", r.get("window_handles", [])) if isinstance(r, dict) else []

    def window(self, n: int = 0):
        w = self.windows()
        if n >= len(w):
            raise ShotError(f"window {n} does not exist ({len(w)} open)")
        return w[n]

    def root(self, n: int = 0):
        props = self.json("get_window_properties", {"windowHandle": self.window(n)})
        return props["rootElementHandle"], props

    def find_id(self, element_id: str, n: int = 0) -> list:
        r = self.json("find_elements_by_id", {"windowHandle": self.window(n), "elementsId": element_id})
        return r.get("elementHandles", []) if isinstance(r, dict) else []

    def element(self, element_id: str, index: int = 0, window: int = 0):
        found = self.find_id(element_id, window)
        if len(found) <= index:
            raise ShotError(f"element '{element_id}' #{index} not found ({len(found)} match)")
        return found[index]

    def props(self, handle) -> dict:
        return self.json("get_element_properties", {"elementHandle": handle})

    def tree(self, handle=None, window: int = 0, max_elements: int = 1000) -> list:
        if handle is None:
            handle, _ = self.root(window)
        r = self.json("get_element_tree", {"elementHandle": handle, "maxElements": max_elements})
        return r.get("elements", [])

    def screenshot(self, window: int = 0) -> bytes:
        import base64
        result = self.call("take_screenshot", {"windowHandle": self.window(window), "imageMimeType": "image/png"})
        for c in result.get("content", []):
            if c.get("type") == "image":
                return base64.b64decode(c["data"])
        raise ShotError("take_screenshot returned no image")


def describe(el: dict) -> str:
    ids = el.get("typeNamesAndIds") or []
    names = ",".join(f"{i.get('typeName','')}#{i.get('id','')}" if i.get('id') else i.get('typeName', '') for i in ids)
    label = el.get("accessibleLabel", "")
    value = el.get("accessibleValue", "")
    role = el.get("accessibleRole", "")
    pos = el.get("absolutePosition") or {}
    size = el.get("size") or {}
    geo = f"@{pos.get('x',0):.0f},{pos.get('y',0):.0f} {size.get('width',0):.0f}x{size.get('height',0):.0f}"
    extra = []
    if role and role not in ("None", "none", 0):
        extra.append(f"role={role}")
    if label:
        extra.append(f"label={label!r}")
    if value:
        extra.append(f"value={value!r}")
    if el.get("accessibleCheckable"):
        extra.append(f"checked={el.get('accessibleChecked', False)}")
    if el.get("accessibleEnabled") is False:
        extra.append("disabled")
    return f"{names} {geo} {' '.join(extra)}".strip()


def element_ids(el: dict) -> list[str]:
    return [i.get("id", "") for i in (el.get("typeNamesAndIds") or []) if i.get("id")]


# --------------------------------------------------------------------------- steps


def wait_until(fn, timeout: float, what: str):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            r = fn()
            if r:
                return r
        except ShotError as e:
            last = e
        except OSError as e:
            last = e
        time.sleep(0.25)
    raise ShotError(f"timed out after {timeout:.0f} s waiting for {what}" + (f" ({last})" if last else ""))


def click(mcp: Mcp, handle, double=False, button=0):
    mcp.json("click_element", {"elementHandle": handle, "action": 1 if double else 0, "button": button})


def find_label(mcp: Mcp, text: str, under: str | None, window: int):
    if under:
        root = mcp.element(under, 0, window)
    else:
        root, _ = mcp.root(window)
    els = mcp.tree(root, window)
    exact = [e for e in els if e.get("accessibleLabel", "") == text]
    buttons = [e for e in exact if str(e.get("accessibleRole", "")).lower() in ("button", "2", "checkbox", "tab", "listitem", "menuitem", "switch", "radiobutton", "combobox")]
    pick = (buttons or exact)
    return pick[0] if pick else None


def do_step(mcp: Mcp, step: list[str], out_dir: Path | None = None, window: int = 0) -> str:
    verb, *a = [str(x) for x in step]
    if "--window" in a:
        i = a.index("--window"); window = int(a[i + 1]); del a[i:i + 2]
    if verb == "wait-id":
        timeout = float(a[1]) if len(a) > 1 else 60
        wait_until(lambda: mcp.find_id(a[0], window), timeout, f"element '{a[0]}'")
        return f"found {a[0]}"
    if verb == "wait-gone":
        timeout = float(a[1]) if len(a) > 1 else 60
        wait_until(lambda: not mcp.find_id(a[0], window), timeout, f"element '{a[0]}' to disappear")
        return f"gone {a[0]}"
    if verb == "wait-text":
        timeout = float(a[1]) if len(a) > 1 else 60
        def has_text():
            return [e for e in mcp.tree(window=window) if a[0] in e.get("accessibleLabel", "") or a[0] in e.get("accessibleValue", "")]
        wait_until(has_text, timeout, f"text '{a[0]}'")
        return f"text {a[0]!r} visible"
    if verb == "wait-windows":
        want = int(a[0]); timeout = float(a[1]) if len(a) > 1 else 30
        wait_until(lambda: len(mcp.windows()) >= want, timeout, f"{want} windows")
        return f"{want} windows"
    if verb in ("click-id", "double-click-id", "right-click-id"):
        h = mcp.element(a[0], int(a[1]) if len(a) > 1 else 0, window)
        click(mcp, h, double=verb == "double-click-id", button=1 if verb == "right-click-id" else 0)
        return f"clicked {a[0]}"
    if verb == "click-label":
        el = wait_until(lambda: find_label(mcp, a[0], a[1] if len(a) > 1 else None, window), 30, f"label '{a[0]}'")
        click(mcp, el["handle"])
        return f"clicked label {a[0]!r}"
    if verb == "drag-id":
        h = mcp.element(a[0], int(a[3]) if len(a) > 3 else 0, window)
        mcp.json("drag_element", {"elementHandle": h, "target": {"x": float(a[1]), "y": float(a[2])}, "button": 0})
        return f"dragged {a[0]}"
    if verb == "key":
        text = KEYS.get(a[0], a[0])
        mcp.json("dispatch_key_event", {"windowHandle": mcp.window(window), "text": text, "eventType": 0})
        return f"key {a[0]}"
    if verb == "type":
        for ch in a[0]:
            mcp.json("dispatch_key_event", {"windowHandle": mcp.window(window), "text": ch, "eventType": 0})
        return f"typed {a[0]!r}"
    if verb == "set-value":
        h = mcp.element(a[0], int(a[2]) if len(a) > 2 else 0, window)
        mcp.json("set_element_value", {"elementHandle": h, "value": a[1]})
        return f"set {a[0]}={a[1]}"
    if verb == "pause":
        time.sleep(float(a[0]) / 1000.0)
        return f"paused {a[0]} ms"
    if verb == "window":
        return f"window {a[0]}"
    raise ShotError(f"unknown step verb '{verb}'")


# --------------------------------------------------------------------------- images


def crop_box(mcp: Mcp, spec: str, pad: int, window: int, scale: float):
    head, sep, tail = spec.rpartition(":")
    eid, idx = (head, tail) if sep and tail.isdigit() and not head.endswith(":") else (spec, "0")
    p = mcp.props(mcp.element(eid, int(idx or 0), window))
    pos, size = p.get("absolutePosition") or {}, p.get("size") or {}
    x0 = max(0, int((pos.get("x", 0) - pad) * scale))
    y0 = max(0, int((pos.get("y", 0) - pad) * scale))
    x1 = int((pos.get("x", 0) + size.get("width", 0) + pad) * scale)
    y1 = int((pos.get("y", 0) + size.get("height", 0) + pad) * scale)
    return x0, y0, x1, y1


def web_image(png: bytes, box=None, max_kb: int = DEFAULT_MAX_KB) -> bytes:
    from PIL import Image
    img = Image.open(io.BytesIO(png)).convert("RGB")
    if box:
        x0, y0, x1, y1 = box
        img = img.crop((x0, y0, min(x1, img.width), min(y1, img.height)))
    if img.width > MAX_WIDTH:
        img = img.resize((MAX_WIDTH, round(img.height * MAX_WIDTH / img.width)), Image.LANCZOS)
    best = None
    for colours in (None, 256, 192, 128, 96, 64):
        buf = io.BytesIO()
        out = img if colours is None else img.quantize(colors=colours, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.FLOYDSTEINBERG)
        out.save(buf, format="PNG", optimize=True)
        data = buf.getvalue()
        best = data
        if len(data) <= max_kb * 1024:
            return data
    raise ShotError(f"image stays above {max_kb} KB ({len(best)//1024} KB at 64 colours); crop it or raise max_kb")


# --------------------------------------------------------------------------- launch


def launch(checkout: Path, name: str, gui_args: list[str], size: str = DEFAULT_SIZE, env_extra=None,
           config_seed: str | None = None, data_seed: str | None = None) -> dict:
    gui = checkout / "target" / "debug" / "focusforge-gui"
    if not gui.exists():
        raise ShotError(f"{gui} missing: build it with `cargo build -p focusforge-gui --features mcp`")
    scratch = checkout / ".scratch" / "manual-shots" / "run" / name
    if scratch.exists():
        shutil.rmtree(scratch)
    (scratch / "config").mkdir(parents=True)
    (scratch / "data").mkdir()
    (scratch / "save").mkdir()
    if config_seed:
        dest = scratch / "config" / "focusforge"
        dest.mkdir(parents=True, exist_ok=True)
        shutil.copy(resolve(checkout, config_seed), dest / "settings.toml")
    if data_seed:
        shutil.copytree(resolve(checkout, data_seed), scratch / "data", dirs_exist_ok=True)
    w, h = (int(v) for v in size.lower().split("x"))
    display = free_display()
    xvfb = subprocess.Popen(["Xvfb", f":{display}", "-screen", "0", f"{max(w, 1920)}x{max(h, 1200)}x24", "-nolisten", "tcp"],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    try:
        wait_until(lambda: xvfb.poll() is None and Path(f"/tmp/.X11-unix/X{display}").exists(), 10, "Xvfb")
    except ShotError:
        xvfb.kill()
        raise
    port = free_port()
    env = dict(os.environ)
    env.update({
        "DISPLAY": f":{display}", "SLINT_MCP_PORT": str(port), "SLINT_SCALE_FACTOR": "1",
        "FOCUSFORGE_DATA_DIR": str(scratch / "data"), "WAYLAND_DISPLAY": "",
        "XDG_CONFIG_HOME": str(scratch / "xdg-config"), "XDG_DATA_HOME": str(scratch / "xdg-data"),
        "XDG_CACHE_HOME": str(scratch / "xdg-cache"), "XDG_STATE_HOME": str(scratch / "xdg-state"),
        "HOME": str(scratch / "home"), "LIBGL_ALWAYS_SOFTWARE": "1",
        # No GPU in a headless box: let Slint's WGPU renderer use the software Vulkan adapter.
        "SLINT_WGPU_CPU": "1",
    })
    env.pop("WAYLAND_DISPLAY", None)
    for k in ("xdg-config", "xdg-data", "xdg-cache", "xdg-state", "home"):
        (scratch / k).mkdir(exist_ok=True)
    env.update(env_extra or {})
    args = [str(resolve(checkout, a)) if a.startswith(("fixtures/", ".scratch/")) else a for a in gui_args]
    full = [str(gui), *args, "--size", f"{w}x{h}", "--config-dir", str(scratch / "config"),
            "--save-folder", str(scratch / "save")]
    log = open(scratch / "gui.log", "w")
    proc = subprocess.Popen(full, cwd=checkout, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    info = {"name": name, "port": port, "display": display, "gui_pid": proc.pid, "xvfb_pid": xvfb.pid,
            "scratch": str(scratch), "size": [w, h], "argv": full}
    mcp = Mcp(port)
    try:
        def ready():
            if proc.poll() is not None:
                raise RuntimeError(f"GUI exited with {proc.returncode}; see {scratch/'gui.log'}")
            return mcp.alive() and mcp.windows()
        wait_until(ready, 120, "the GUI's MCP server")
        # The window is resized 50 ms after it is shown; wait until the size has landed.
        def sized():
            _, props = mcp.root(0)
            s = props.get("size") or {}
            return s.get("width") == w and s.get("height") == h
        try:
            wait_until(sized, 15, f"window size {w}x{h}")
        except ShotError:
            _, props = mcp.root(0)
            raise ShotError(f"window did not reach {w}x{h} (is {props.get('size')})")
    except Exception:
        teardown(info)
        raise
    return info


def teardown(info: dict) -> None:
    for key in ("gui_pid", "xvfb_pid"):
        pid = info.get(key)
        if not pid:
            continue
        try:
            os.killpg(pid, signal.SIGTERM)
        except ProcessLookupError:
            continue
        for _ in range(40):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            try:
                os.waitpid(pid, os.WNOHANG)
            except ChildProcessError:
                pass
            time.sleep(0.1)
        else:
            try:
                os.killpg(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def resolve(checkout: Path, p: str) -> Path:
    path = Path(p)
    return path if path.is_absolute() else checkout / path


# --------------------------------------------------------------------------- commands


def capture(mcp: Mcp, info: dict, out: Path, window=0, crop=None, pad=0, raw=False, max_kb=DEFAULT_MAX_KB) -> str:
    png = mcp.screenshot(window)
    from PIL import Image
    shot_w = Image.open(io.BytesIO(png)).width
    _, props = mcp.root(window)
    logical_w = (props.get("size") or {}).get("width") or shot_w
    scale = shot_w / logical_w if logical_w else 1.0
    box = crop_box(mcp, crop, pad, window, scale) if crop else None
    data = png if raw else web_image(png, box, max_kb)
    out.parent.mkdir(parents=True, exist_ok=True)
    tmp = out.with_name(out.name + ".tmp")
    tmp.write_bytes(data)
    tmp.replace(out)
    img = Image.open(out)
    return f"wrote {out} {img.width}x{img.height} {len(data)//1024} KB"


SEED_REL = ".scratch/Mantis/mantis-512-fused.ffp"


def make_seed(checkout: Path) -> Path:
    """Fuse fixtures/mantis-512/frames, answer Keep and save: a fused project shots can reopen in seconds."""
    seed = checkout / SEED_REL
    info = launch(checkout, "seed", ["--open", "fixtures/mantis-512/frames", "--fuse-tabs", "0", "--save-active-project"])
    try:
        mcp = Mcp(info["port"])
        do_step(mcp, ["wait-id", "AppWindow::divergence-warning", "900"])
        do_step(mcp, ["click-label", "Keep", "AppWindow::divergence-warning"])
        saved = Path(info["scratch"]) / "save" / "frames.ffp"
        wait_until(lambda: saved.exists() and saved.stat().st_size > 0, 120, "the saved seed project")
        time.sleep(2)
        seed.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(saved, seed)
    finally:
        teardown(info)
    print(f"[seed] {seed}")
    return seed


def run_shotlists(paths: list[Path], only: set[str], checkout: Path, reuse_seed: bool = False) -> int:
    shots = []
    for p in paths:
        data = tomllib.loads(p.read_text())
        for s in data.get("shot", []):
            s["_list"] = str(p)
            shots.append(s)
    names = [s["name"] for s in shots]
    dupes = {n for n in names if names.count(n) > 1}
    if dupes:
        print(f"duplicate shot names: {sorted(dupes)}", file=sys.stderr)
        return 2
    if only:
        missing = only - set(names)
        if missing:
            print(f"unknown shot names: {sorted(missing)}", file=sys.stderr)
            return 2
        shots = [s for s in shots if s["name"] in only]
    if any(SEED_REL in a for s in shots for a in s.get("args", [])) and not (reuse_seed and (checkout / SEED_REL).exists()):
        try:
            make_seed(checkout)
        except Exception as e:  # noqa: BLE001
            print(f"[FAIL] seed project: {e}", file=sys.stderr)
            return 1
    for s in shots:
        name = s["name"]
        out = checkout / "docs" / s["out"]
        t0 = time.time()
        info = None
        try:
            base = Path(s["_list"]).resolve().parent
            run_dir = checkout / ".scratch" / "manual-shots" / "run" / f"shot-{name}"
            subst = lambda v: str(v).replace("{checkout}", str(checkout)).replace("{run}", str(run_dir))
            rel = lambda v: str(base / v) if v and not Path(v).is_absolute() else v
            env = {k: subst(v) for k, v in (s.get("env") or {}).items()}
            s["steps"] = [[subst(x) for x in step] for step in s.get("steps", [])]
            info = launch(checkout, f"shot-{name}", s.get("args", []), s.get("size", DEFAULT_SIZE), env,
                          rel(s.get("config")), rel(s.get("data_seed")))
            mcp = Mcp(info["port"])
            window = int(s.get("window", 0))
            for step in s.get("steps", []):
                do_step(mcp, step)
            msg = capture(mcp, info, out, window, s.get("crop"), int(s.get("pad", 0)), False, int(s.get("max_kb", DEFAULT_MAX_KB)))
            print(f"[ok] {name}: {msg} ({time.time()-t0:.0f} s)")
        except Exception as e:  # noqa: BLE001 - every failure stops the run by name
            print(f"[FAIL] {name} ({s['_list']}): {e}", file=sys.stderr)
            if info:
                print(f"       log: {info['scratch']}/gui.log", file=sys.stderr)
            return 1
        finally:
            if info:
                teardown(info)
    return 0


VERBS = {"wait-id", "wait-gone", "wait-text", "wait-windows", "click-id", "double-click-id", "right-click-id",
         "click-label", "drag-id", "key", "type", "set-value", "pause", "window"}


def selftest(checkout: Path) -> int:
    """Check every shot list without a display: names, verbs, seeds, outputs and that the book uses each image."""
    problems: list[str] = []
    names: set[str] = set()
    outs: set[str] = set()
    lists = sorted(SHOT_LISTS.glob("*.toml"))
    pages = "\n".join(p.read_text(encoding="utf-8") for p in (checkout / "docs").rglob("*.md"))
    count = 0
    for lst in lists:
        try:
            data = tomllib.loads(lst.read_text(encoding="utf-8"))
        except tomllib.TOMLDecodeError as e:
            problems.append(f"{lst.name}: {e}")
            continue
        for s in data.get("shot", []):
            count += 1
            name = s.get("name", "?")
            where = f"{lst.name}:{name}"
            if name in names:
                problems.append(f"{where}: duplicate shot name")
            names.add(name)
            out = s.get("out", "")
            if not out.startswith("assets/manual/") or not out.endswith(".png"):
                problems.append(f"{where}: out must be a .png under docs/assets/manual/ (is {out!r})")
            if out in outs:
                problems.append(f"{where}: output {out} written by two shots")
            outs.add(out)
            if Path(out).name not in pages:
                problems.append(f"{where}: no page in docs/ references {Path(out).name}")
            for key in ("config", "data_seed"):
                if s.get(key) and not (lst.parent / s[key]).exists():
                    problems.append(f"{where}: {key} {s[key]} missing beside the shot list")
            for a in s.get("args", []):
                if a.startswith("fixtures/") and not (checkout / a).exists():
                    problems.append(f"{where}: input {a} missing")
            for step in s.get("steps", []):
                if not step or step[0] not in VERBS:
                    problems.append(f"{where}: unknown step {step!r}")
            try:
                w, h = (int(v) for v in s.get("size", DEFAULT_SIZE).lower().split("x"))
                if not (320 <= w <= 3840 and 240 <= h <= 2160):
                    raise ValueError
            except ValueError:
                problems.append(f"{where}: bad size {s.get('size')!r}")
    # The image budget holds for what is committed.
    try:
        from PIL import Image
        for out in sorted(outs):
            path = checkout / "docs" / out
            if not path.exists():
                problems.append(f"{out}: not produced yet (run the tool)")
                continue
            with Image.open(path) as img:
                if img.width > MAX_WIDTH or path.stat().st_size > DEFAULT_MAX_KB * 1024:
                    problems.append(f"{out}: {img.width} px / {path.stat().st_size // 1024} KB is over budget")
    except ImportError:
        print("Pillow missing: image budget not checked", file=sys.stderr)
    for line in problems:
        print(line, file=sys.stderr)
    if problems:
        return 1
    print(f"OK: {count} shots in {len(lists)} lists, every image referenced by the book")
    return 0


def main(argv: list[str]) -> int:
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 0
    if argv[0] == "--selftest":
        return selftest(DEFAULT_CHECKOUT)
    checkout = DEFAULT_CHECKOUT
    if "--checkout" in argv:
        i = argv.index("--checkout")
        checkout = Path(argv[i + 1]).resolve()
        del argv[i:i + 2]
    cmd = argv[0]
    try:
        if cmd == "run":
            only: set[str] = set()
            lists: list[Path] = []
            rest = argv[1:]
            reuse = "--reuse-seed" in rest
            rest = [r for r in rest if r != "--reuse-seed"]
            while rest:
                a = rest.pop(0)
                if a == "--only":
                    while rest and not rest[0].startswith("--"):
                        only.add(rest.pop(0))
                else:
                    lists.append(Path(a))
            return run_shotlists(lists or sorted(SHOT_LISTS.glob("*.toml")), only, checkout, reuse)
        if cmd == "start":
            name = argv[1]
            rest = argv[2:]
            size = DEFAULT_SIZE
            config = None
            if "--size" in rest[: rest.index("--") if "--" in rest else len(rest)]:
                i = rest.index("--size"); size = rest[i + 1]; del rest[i:i + 2]
            if "--config" in rest[: rest.index("--") if "--" in rest else len(rest)]:
                i = rest.index("--config"); config = rest[i + 1]; del rest[i:i + 2]
            gui_args = rest[rest.index("--") + 1:] if "--" in rest else rest
            state = sessions_dir(checkout) / f"{name}.json"
            if state.exists():
                teardown(json.loads(state.read_text()))
            info = launch(checkout, f"session-{name}", gui_args, size, None, config)
            state.write_text(json.dumps(info))
            print(f"session {name}: port {info['port']} display :{info['display']} log {info['scratch']}/gui.log")
            return 0
        if cmd == "seed":
            make_seed(checkout)
            return 0
        if cmd == "stop":
            state = sessions_dir(checkout) / f"{argv[1]}.json"
            if state.exists():
                teardown(json.loads(state.read_text()))
                state.unlink()
            print(f"stopped {argv[1]}")
            return 0
        if cmd == "stop-all":
            for state in sessions_dir(checkout).glob("*.json"):
                teardown(json.loads(state.read_text()))
                state.unlink()
            print("stopped all sessions")
            return 0
        # session verbs
        state = sessions_dir(checkout) / f"{cmd}.json"
        if not state.exists():
            print(f"no session '{cmd}' (start one with: tools/manual-screenshots.py start {cmd} -- <gui args>)", file=sys.stderr)
            return 2
        info = json.loads(state.read_text())
        mcp = Mcp(info["port"])
        verb, *a = argv[1:]
        window = 0
        if "--window" in a:
            i = a.index("--window"); window = int(a[i + 1]); del a[i:i + 2]
        if verb == "windows":
            for n, w in enumerate(mcp.windows()):
                _, props = mcp.root(n)
                print(n, props.get("size"), props.get("position"))
            return 0
        if verb == "tree":
            show_all = "--all" in a
            a = [x for x in a if x != "--all"]
            root = mcp.element(a[0], int(a[1]) if len(a) > 1 else 0, window) if a else None
            for el in mcp.tree(root, window):
                if show_all or element_ids(el) or el.get("accessibleLabel") or el.get("accessibleValue"):
                    if not show_all and not element_ids(el) and not (el.get("accessibleLabel") or el.get("accessibleValue")):
                        continue
                    print(describe(el))
            return 0
        if verb == "find":
            needle = a[0].lower()
            for el in mcp.tree(window=window):
                hay = " ".join(element_ids(el) + [el.get("accessibleLabel", ""), el.get("accessibleValue", "")]).lower()
                if needle in hay:
                    print(describe(el))
            return 0
        if verb == "props":
            print(json.dumps(mcp.props(mcp.element(a[0], int(a[1]) if len(a) > 1 else 0, window)), indent=1))
            return 0
        if verb == "shot":
            out = Path(a[0]).resolve()
            crop = pad = None
            raw = "--raw" in a
            if "--crop" in a:
                crop = a[a.index("--crop") + 1]
            pad = int(a[a.index("--pad") + 1]) if "--pad" in a else 0
            print(capture(mcp, info, out, window, crop, pad, raw))
            return 0
        print(do_step(mcp, [verb, *a], window=window))
        return 0
    except ShotError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    sys.exit(main(sys.argv[1:]))
