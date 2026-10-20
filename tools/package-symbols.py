#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Create and verify pinned native plus Breakpad symbol archives."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import posixpath
import plistlib
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import urllib.request
import uuid
import zipfile
import struct
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:
    tomllib = None

ROOT = Path(__file__).resolve().parent.parent
CONFIG = ROOT / "packaging/symbol-tools.toml"


def digest(path: Path) -> str:
    value = hashlib.sha256()
    if path.is_dir():
        for child in sorted(item for item in path.rglob("*") if item.is_file()):
            value.update(child.relative_to(path).as_posix().encode() + b"\0")
            value.update(digest(child).encode() + b"\n")
        return value.hexdigest()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def check_config(path: Path = CONFIG) -> dict:
    text = path.read_text()
    if tomllib is not None:
        data = tomllib.loads(text)
    else:
        data = parse_symbol_tools(text)
    for name in ("dump_syms", "stackwalk"):
        tool = data.get(name)
        if not isinstance(tool, dict) or not isinstance(tool.get("version"), str):
            raise ValueError(f"missing {name} version")
        if not str(tool.get("manifest_url", "")).startswith("https://") or len(str(tool.get("manifest_sha256", ""))) != 64:
            raise ValueError(f"invalid {name} manifest pin")
        if not isinstance(tool.get("licenses"), list) or not tool["licenses"]:
            raise ValueError(f"missing {name} license declaration")
        seen: set[str] = set()
        for item in tool.get("artifact", []):
            target, url, checksum, executable = (item.get(key) for key in ("target", "url", "sha256", "executable"))
            if not isinstance(target, str) or target in seen or not str(url).startswith("https://") or len(str(checksum)) != 64 or not isinstance(executable, str):
                raise ValueError(f"invalid {name} artifact")
            seen.add(target)
    return data


def parse_symbol_tools(text: str) -> dict:
    """Read this fixed pin file on Ubuntu 22.04's Python 3.10."""
    data: dict = {}
    current: dict | None = None
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if line.startswith("[[") and line.endswith("]]" ):
            tool, field = line[2:-2].split(".", 1)
            current = {}; data.setdefault(tool, {}).setdefault(field, []).append(current)
            continue
        if line.startswith("[") and line.endswith("]"):
            current = data.setdefault(line[1:-1], {})
            continue
        if current is None or "=" not in line:
            raise ValueError("unsupported symbol tool pin format")
        key, value = (part.strip() for part in line.split("=", 1))
        if value.startswith("[") and value.endswith("]"):
            current[key] = [item.strip().strip('"') for item in value[1:-1].split(",") if item.strip()]
        elif value.startswith('"') and value.endswith('"'):
            current[key] = value[1:-1]
        else:
            raise ValueError("unsupported symbol tool pin value")
    return data


def artifact(data: dict, tool: str, target: str) -> dict:
    for item in data[tool]["artifact"]:
        if item["target"] == target:
            return item
    raise ValueError(f"no {tool} artifact for {target}")


def safe_members(names: list[str]) -> None:
    if not names or any(Path(name).is_absolute() or ".." in Path(name).parts for name in names):
        raise ValueError("unsafe tool archive")


def extract_tar(source: tarfile.TarFile, destination: Path) -> None:
    members = source.getmembers()
    names = [member.name for member in members]
    safe_members(names)
    if len(names) != len(set(names)) or any(not (member.isdir() or member.isfile()) for member in members):
        raise ValueError("unsafe tool archive")
    for member in members:
        source.extract(member, destination)


def verified_release_manifest(data: dict, tool: str, target: str, item: dict, directory: Path) -> None:
    """Authenticate the release index before accepting its selected archive."""
    pinned = data[tool]
    manifest = directory / f"{tool}-{pinned['version']}-manifest.json"
    if not manifest.exists():
        with urllib.request.urlopen(pinned["manifest_url"], timeout=60) as response:
            manifest.write_bytes(response.read())
    if digest(manifest) != pinned["manifest_sha256"]:
        raise ValueError(f"{tool} release manifest checksum mismatch")
    try:
        release = json.loads(manifest.read_text())
        published = release["artifacts"][Path(item["url"]).name]
        checksums = published["checksums"]
        targets = published["target_triples"]
    except (KeyError, TypeError, json.JSONDecodeError) as error:
        raise ValueError(f"invalid {tool} release manifest") from error
    if checksums.get("sha256") != item["sha256"] or targets != [target]:
        raise ValueError(f"{tool} release manifest does not match pinned archive")


def install(data: dict, tool: str, target: str, directory: Path) -> Path:
    item = artifact(data, tool, target)
    directory.mkdir(parents=True, exist_ok=True)
    verified_release_manifest(data, tool, target, item, directory)
    archive = directory / Path(item["url"]).name
    if not archive.is_file() or digest(archive) != item["sha256"]:
        with urllib.request.urlopen(item["url"], timeout=60) as response:
            archive.write_bytes(response.read())
    if digest(archive) != item["sha256"]:
        raise ValueError(f"{tool} archive checksum mismatch")
    unpacked = directory / f"{tool}-{target}"
    shutil.rmtree(unpacked, ignore_errors=True)
    unpacked.mkdir()
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as source:
            safe_members(source.namelist())
            infos = source.infolist()
            names = [info.filename for info in infos]
            safe_members(names)
            if len(names) != len(set(names)) or any(((info.external_attr >> 16) & 0o170000) not in (0, stat.S_IFREG, stat.S_IFDIR) for info in infos):
                raise ValueError("unsafe tool archive")
            source.extractall(unpacked)
    else:
        with tarfile.open(archive) as source:
            extract_tar(source, unpacked)
    matches = [path for path in unpacked.rglob(item["executable"]) if path.is_file()]
    if len(matches) != 1:
        raise ValueError(f"{tool} executable is missing or ambiguous")
    matches[0].chmod(matches[0].stat().st_mode | 0o111)
    return matches[0]


def module_header(path: Path) -> tuple[str, str, str, str]:
    head = path.open(encoding="utf-8").readline().split()
    if len(head) != 5 or head[0] != "MODULE" or len(head[3]) < 33 or not all(char in "0123456789ABCDEF" for char in head[3]):
        raise ValueError(f"invalid Breakpad MODULE: {path}")
    body = path.read_text(encoding="utf-8", errors="strict")
    lines = body.splitlines()
    missing = [record for record in ("FILE ", "FUNC ", "STACK CFI ") if not any(line.startswith(record) for line in lines)]
    if missing:
        raise ValueError(f"incomplete Breakpad symbols: {path} lacks {[record.strip() for record in missing]} in {len(lines)} lines")
    for line in body.splitlines():
        if line.startswith("FILE "):
            source = line.split(" ", 2)[-1]
            if source != normalized_source(source):
                raise ValueError("uncanonical source path in symbols")
    return head[1], head[2], head[3], head[4]


def module(path: Path) -> tuple[str, str]:
    _, _, identifier, name = module_header(path)
    return identifier, name


def platform_header(platform: str) -> tuple[str, str, str]:
    known = {
        "x86_64-unknown-linux-gnu": ("Linux", "x86_64", ".debug"),
        "aarch64-apple-darwin": ("Mac", "arm64", ".dSYM"),
        "x86_64-pc-windows-msvc": ("Windows", "x86_64", ".pdb"),
    }
    try:
        return known[platform]
    except KeyError as error:
        raise ValueError("unsupported symbol target") from error


def clipped(value: str) -> str:
    return value if len(value) <= 200 else value[:197] + "..."


def under(path: str, root: str) -> str | None:
    """The part of `path` below `root`, or None; Windows roots compare without case."""
    fold = str.casefold if re.match(r"^[A-Za-z]:", root) else str
    prefix = root.rstrip("/") + "/"
    if fold(path) == fold(root.rstrip("/")):
        return ""
    return path[len(prefix):] if fold(path[:len(prefix)]) == fold(prefix) else None


TOOLCHAIN_ROOTS = re.compile(
    r"^(?:/Applications/Xcode[^/]*\.app/Contents/Developer|/Library/Developer/CommandLineTools"
    r"|[A-Za-z]:/Program Files(?: \(x86\))?/(?:Microsoft Visual Studio|Windows Kits)"
    r"|[A-Za-z]:/[A-Za-z0-9]+/_work/\d+/s)/(.+)$"
)


def builder_location(value: str) -> str:
    """Map the build host's checkout, cargo home, toolchain and SDK onto the container layout."""
    # Windows canonical paths carry the extended-length prefix: "\\?\D:\a\..." is "D:\a\...".
    path = posixpath.normpath(re.sub(r"^//\?/(?=[A-Za-z]:/)", "", value.replace("\\", "/")))
    relative = under(path, ROOT.as_posix())
    if relative is not None:
        if relative.startswith("target/"):
            return "/out/target/" + relative[len("target/"):]
        return "/work/" + relative if relative else "/work"
    cargo_home = os.environ.get("CARGO_HOME", "").replace("\\", "/")
    relative = under(path, cargo_home) if cargo_home else None
    if relative and relative.startswith(("registry/", "git/checkouts/")):
        return "/root/.cargo/" + relative
    for pattern, template in (
        (r"/\.cargo/((?:registry/src|git/checkouts)/.+)$", "/root/.cargo/{}"),
        (r"/lib/rustlib/(?:src|rustc-src)/rust/(library/.+)$", "/rustc/toolchain/{}"),
        # The debugger visualizers a PDB lists beside its sources.
        (r"/lib/rustlib/(etc/[^/]+\.natvis)$", "/rustc/toolchain/{}"),
        (r"^[A-Za-z]:(/rustc/.+)$", "{}"),
    ):
        match = re.search(pattern, path)
        if match:
            return template.format(match.group(1))
    match = TOOLCHAIN_ROOTS.match(path)
    return "/usr/include/" + match.group(1) if match else path


def native_source(path: str) -> str:
    """A path read from native symbols; builder locations are mapped only on a public hosted runner."""
    path = path.replace("\\", "/")
    return builder_location(path) if os.environ.get("GITHUB_ACTIONS") == "true" else path


def reject_private_native(paths) -> None:
    rejected = sorted({path.replace("\\", "/") for path in paths if not safe_native_source(path)})
    if rejected:
        raise ValueError(f"private source path in native symbols: {clipped(rejected[0])}" + (f" (+{len(rejected) - 1} more)" if len(rejected) > 1 else ""))


def absolute_native(value: str) -> bool:
    value = value.replace("\\", "/")
    return value.startswith("/") or re.match(r"^[A-Za-z]:/", value) is not None


def normalized_source(value: str) -> str:
    """Map product paths and allow standard dependency records without escapes."""
    source = value.replace("\\", "/")
    if ".." in Path(source).parts:
        raise ValueError(f"source path escapes its root: {clipped(value)}")
    source = builder_location(source)
    for prefix in ("crates/", "/work/crates/", "C:/work/crates/"):
        if source.startswith(prefix):
            return "crates/" + source.split("crates/", 1)[1]
    if source.startswith("dependency/"):
        return source
    if source.startswith(("/rustc/", "/rust/deps/", "/root/.cargo/registry/", "/root/.cargo/git/checkouts/", "/out/target/", "/usr/include/")) or re.match(r"^/usr/lib/gcc/[^/]+/[^/]+/include/.+", source):
        return "dependency/" + source.rsplit("/", 1)[-1]
    raise ValueError(f"private or unsupported source path in symbols: {clipped(value)}")


def canonicalize_symbol_sources(path: Path) -> None:
    lines = []
    rejected: list[str] = []
    for line in path.read_text(encoding="utf-8", errors="strict").splitlines(keepends=True):
        if line.startswith("FILE "):
            parts = line.rstrip("\n").split(" ", 2)
            if len(parts) != 3:
                raise ValueError("invalid Breakpad FILE record")
            try:
                line = f"{parts[0]} {parts[1]} {normalized_source(parts[2])}\n"
            except ValueError:
                if parts[2] not in rejected: rejected.append(parts[2])
        lines.append(line)
    if rejected:
        raise ValueError("private or unsupported source path in symbols: " + "; ".join(clipped(value) for value in rejected[:5]) + (f" (+{len(rejected) - 5} more)" if len(rejected) > 5 else ""))
    path.write_text("".join(lines), encoding="utf-8")


SOURCE_INSPECTION_SCHEMA = 1


def resolved_native_source(directory: str, value: str) -> str:
    """Resolve one decoded DWARF source record without accepting escapes."""
    if not isinstance(directory, str) or not isinstance(value, str) or not directory or not value:
        raise ValueError("empty native source record")
    value = value.replace("\\", "/")
    directory = directory.replace("\\", "/")
    if value.startswith(("/", "//")) or re.match(r"^[A-Za-z]:/", value):
        return posixpath.normpath(value)
    return posixpath.normpath(posixpath.join(directory, value))


def safe_native_source(path: str) -> bool:
    canonical = posixpath.normpath(path.replace("\\", "/"))
    return canonical == "/work" or canonical.startswith(("/work/crates/", "C:/work/crates/", "/work/.cargo/registry/", "C:/work/.cargo/registry/", "/root/.cargo/registry/", "/root/.cargo/git/checkouts/", "/rustc/", "/rust/deps/", "/out/target/", "/usr/", "/lib/", "/lib64/", "/bin/", "/etc/", "/dev/", "/proc/", "/sys/"))


def safe_source_less_cu(name: str) -> bool:
    """Accept only Rust codegen-unit labels with no source table."""
    raw = name.replace("\\", "/")
    if ".." in Path(raw).parts:
        return False
    canonical = posixpath.normpath(raw)
    source, marker, unit = canonical.partition("/@/")
    if not marker or not source or not unit:
        return False
    return source.startswith("crates/") or safe_native_source(source)


def readelf_source_records(path: Path) -> tuple[list[str], int, int, int, int]:
    """Stream GNU readelf's typed CU and line-table records, never raw opcodes."""
    environment = {**os.environ, "LC_ALL": "C", "LANG": "C", "DEBUGINFOD_URLS": ""}
    def lines(option: str):
        process = subprocess.Popen(["readelf", "--wide", f"--debug-dump={option}", "--debug-dump=no-follow-links", str(path)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="strict", env=environment)
        assert process.stdout is not None
        complete = False
        try:
            for line in process.stdout:
                yield line.rstrip("\n")
            complete = True
        finally:
            process.stdout.close()
            if not complete and process.poll() is None:
                process.terminate()
            stderr = process.stderr.read() if process.stderr else ""
            status = process.wait()
            harmless = "readelf: Error: Unable to find program interpreter name"
            if complete and (status or stderr.strip() not in ("", harmless)):
                raise ValueError(f"readelf {option} failed: {stderr.strip()}")
    units: dict[int, dict[str, str]] = {}
    all_units: list[dict[str, str]] = []
    current: dict[str, str] | None = None
    current_is_cu = False
    for line in lines("info"):
        if "DW_TAG_compile_unit" in line and re.match(r"\s*<0><[0-9a-f]+>", line):
            if current is not None:
                all_units.append(current)
            current = {}; current_is_cu = True; continue
        if re.match(r"\s*<[1-9][0-9]*><[0-9a-f]+>", line):
            current_is_cu = False
            continue
        if current is None or not current_is_cu:
            continue
        attribute = re.search(r"DW_AT_(name|comp_dir|stmt_list)\s*:\s*(.*)$", line)
        if not attribute:
            continue
        kind, text = attribute.groups()
        value = text.rsplit("): ", 1)[-1] if "): " in text else text.rsplit(": ", 1)[-1]
        if kind == "stmt_list":
            current[kind] = value
            offset = re.search(r"(?:0x[0-9a-fA-F]+|\d+)\s*$", value)
            if not offset:
                raise ValueError("invalid DWARF stmt_list")
            units[int(offset.group(), 0)] = current
        else:
            current[kind] = value
    if current is not None:
        all_units.append(current)
    if not all_units:
        raise ValueError("missing DWARF compilation units")
    paths: set[str] = set()
    source_less_count = no_line_count = 0
    for unit in all_units:
        if "name" not in unit and "comp_dir" not in unit:
            continue
        if set(unit) not in ({"name", "comp_dir"}, {"name", "comp_dir", "stmt_list"}):
            raise ValueError("incomplete DWARF compilation unit")
        base, name = unit["comp_dir"], unit["name"]
        source = posixpath.normpath(name.replace("\\", "/"))
        if "stmt_list" not in unit:
            if safe_source_less_cu(name):
                source = source.partition("/@/")[0]
                source_less_count += 1
            else:
                no_line_count += 1
        resolved = resolved_native_source(base, source)
        reject_private_native((posixpath.normpath(base.replace("\\", "/")), resolved))
        paths.update((posixpath.normpath(base.replace("\\", "/")), resolved))
    if not units:
        raise ValueError("missing DWARF line tables")
    table_count = 0
    offset: int | None = None
    directories: dict[int, str] = {}
    files: list[tuple[int, str]] = []
    table = ""
    seen: set[int] = set()
    def finish() -> None:
        nonlocal table_count, source_less_count
        if offset is None:
            return
        unit = units.get(offset)
        if unit is not None:
            seen.add(offset)
            if set(unit) != {"name", "comp_dir", "stmt_list"}:
                raise ValueError("incomplete DWARF compilation unit")
            base = unit["comp_dir"]
            paths.add(resolved_native_source(base, unit["name"]))
            paths.add(posixpath.normpath(base.replace("\\", "/")))
            resolved_directories = {index: resolved_native_source(base, value) for index, value in directories.items()}
        else:
            base = None
            resolved_directories = {}
            for index, value in directories.items():
                canonical = value.replace("\\", "/")
                if not canonical.startswith(("/", "//")) and not re.match(r"^[A-Za-z]:/", canonical):
                    raise ValueError("unresolved DWARF line table basis")
                resolved_directories[index] = resolved_native_source("/", canonical)
        paths.update(resolved_directories.values())
        if not files and unit is not None:
            if safe_source_less_cu(unit["name"]):
                source_less_count += 1
            else:
                raise ValueError("incomplete DWARF line table")
        for index, name in files:
            if index and index not in resolved_directories:
                raise ValueError("invalid DWARF line directory index")
            if index:
                directory = resolved_directories[index]
            elif base is not None:
                directory = base
            else:
                value = name.replace("\\", "/")
                if not value.startswith(("/", "//")) and not re.match(r"^[A-Za-z]:/", value):
                    raise ValueError("unresolved DWARF line table basis")
                directory = "/"
            paths.add(resolved_native_source(directory, name))
        table_count += 1
    for line in lines("rawline"):
        match = re.match(r"\s*Offset:\s*(0x[0-9a-fA-F]+|\d+)", line)
        if match:
            finish(); offset = int(match.group(1), 0); directories = {}; files = []; table = ""; continue
        if "The Directory Table" in line: table = "directory"; continue
        if "The File Name Table" in line: table = "file"; continue
        if not line.strip():
            if table in {"directory", "file"}: table = ""
            continue
        if table == "directory":
            row = re.match(r"\s*(\d+)\s+(.+?)\s*$", line)
            if row: directories[int(row.group(1))] = row.group(2)
        elif table == "file":
            row = re.match(r"\s*(\d+)\s+(\d+)\s+\d+\s+\d+\s+(.+?)\s*$", line)
            if row: files.append((int(row.group(2)), row.group(3)))
    finish()
    if seen != set(units) or not table_count:
        raise ValueError("missing DWARF line table")
    return sorted(paths), len(all_units), table_count, source_less_count, no_line_count


def _inspect_linux_native(path: Path) -> dict:
    paths, units, tables, source_less, no_line = readelf_source_records(path)
    reject_private_native(paths)
    return {"schema": SOURCE_INSPECTION_SCHEMA, "format": "elf-dwarf", "native_inspection_sha256": digest(path), "coverage": {"compile_units": units, "line_tables": tables, "source_less_compile_units": source_less, "no_line_compile_units": no_line}, "source_paths": paths}


def valid_source_inspection(platform: str, path: Path, record: object) -> dict:
    if not isinstance(record, dict) or record.get("schema") != SOURCE_INSPECTION_SCHEMA or record.get("native_inspection_sha256") != digest(path):
        raise ValueError("invalid native source inspection")
    expected_format = {"x86_64-unknown-linux-gnu": "elf-dwarf", "aarch64-apple-darwin": "macho-dwarf", "x86_64-pc-windows-msvc": "pdb-dbi"}[platform]
    coverage, paths = record.get("coverage"), record.get("source_paths")
    if record.get("format") != expected_format or not isinstance(coverage, dict) or not isinstance(paths, list) or not paths or any(not isinstance(item, str) or not safe_native_source(item) for item in paths):
        raise ValueError("invalid native source inspection")
    if expected_format == "elf-dwarf":
        if not all(type(coverage.get(key)) is int and coverage[key] > 0 for key in ("compile_units", "line_tables")) or not all(type(coverage.get(key)) is int and coverage[key] >= 0 for key in ("source_less_compile_units", "no_line_compile_units")):
            raise ValueError("incomplete ELF source inspection")
    if expected_format == "macho-dwarf" and (coverage.get("slices") != ["arm64"] or type(coverage.get("auxiliary_source_records")) is not int or coverage["auxiliary_source_records"] < 0):
        raise ValueError("incomplete Mach-O source inspection")
    if expected_format == "pdb-dbi" and (coverage.get("dbi_stream") != 3 or type(coverage.get("file_records")) is not int or coverage["file_records"] < 1 or not all(type(coverage.get(key)) is int and coverage[key] >= 0 for key in ("module_c13_records", "environment_records"))):
        raise ValueError("incomplete PDB source inspection")
    return record


def macos_auxiliary_source_paths(path: Path, uuids: set[str]) -> tuple[set[str], int]:
    candidates = [path / "Contents/Info.plist"] + [item for item in (path / "Contents/Resources").glob("*.plist") if item.stem.replace("-", "").upper() in uuids]
    paths: set[str] = set(); records = 0
    for plist in candidates:
        try: value = plistlib.loads(plist.read_bytes())
        except (OSError, plistlib.InvalidFileException, ValueError) as error: raise ValueError("invalid dSYM auxiliary plist") from error
        if not isinstance(value, dict): raise ValueError("invalid dSYM auxiliary plist")
        build, source = value.get("DBGBuildSourcePath"), value.get("DBGSourcePath")
        if (build is None) != (source is None) or (build is not None and (not isinstance(build, str) or not build or not isinstance(source, str) or not source)): raise ValueError("invalid dSYM source path pair")
        if build is not None: paths.update((build, source)); records += 1
        remapping = value.get("DBGSourcePathRemapping")
        if remapping is not None:
            if not isinstance(remapping, dict): raise ValueError("invalid dSYM source path remapping")
            for left, right in remapping.items():
                if not isinstance(left, str) or not left or not isinstance(right, str) or not right: raise ValueError("invalid dSYM source path remapping")
                paths.update((left, right)); records += 1
    return paths, records


def _inspect_macos_native(path: Path, logical_name: str) -> dict:
    dwarf = path / "Contents/Resources/DWARF" / logical_name
    if not dwarf.is_file():
        raise ValueError("missing dSYM DWARF payload")
    paths: set[str] = set()
    slices: list[str] = []
    for arch in ("arm64",):
        result = subprocess.run(["dwarfdump", "--arch", arch, "--debug-info", "--debug-line", str(dwarf)], text=True, encoding="utf-8", errors="strict", capture_output=True, check=False)
        if result.returncode or result.stderr:
            raise ValueError(f"dwarfdump failed for {arch}: {result.stderr.strip()}")
        units: dict[int, tuple[str | None, str | None, int | None]] = {}
        unit_offset: int | None = None; directory = name = None; statement_list: int | None = None; unit_active = False
        line_tables: dict[int, tuple[dict[int, str], list[tuple[int, str]]]] = {}
        table_offset: int | None = None; line_directories: dict[int, str] = {}; line_files: list[tuple[int, str]] = []; file_name: str | None = None; file_directory: int | None = None
        def finish_file() -> None:
            nonlocal file_name, file_directory
            if file_name is not None:
                if file_directory is None: raise ValueError(f"incomplete dSYM line file for {arch}")
                line_files.append((file_directory, file_name))
            file_name = file_directory = None
        def finish_unit() -> None:
            nonlocal directory, name, statement_list, unit_offset
            if unit_offset is None: return
            units[unit_offset] = (directory, name, statement_list)
            directory = name = None; statement_list = unit_offset = None
        def finish_table() -> None:
            nonlocal line_directories, line_files, table_offset
            finish_file()
            if table_offset is not None:
                line_tables[table_offset] = (line_directories, line_files)
            line_directories = {}; line_files = []; table_offset = None
        for line in result.stdout.splitlines():
            unit = re.search(r"^(?:0x)?([0-9A-Fa-f]+):.*DW_TAG_compile_unit", line)
            table = re.search(r"(?:debug_line|line table)\[(0x[0-9A-Fa-f]+)\]", line)
            if unit:
                finish_unit(); unit_active = True; unit_offset = int(unit.group(1), 16)
            elif "DW_TAG_" in line:
                unit_active = False
            value = re.search(r"DW_AT_(name|comp_dir)\s*\((.*)\)", line)
            if value and unit_active:
                kind, raw = value.groups(); raw = raw.strip('"')
                if kind == "name": name = raw
                else: directory = raw
            statement = re.search(r"DW_AT_stmt_list\s*\((0x[0-9A-Fa-f]+)\)", line)
            if statement and unit_active: statement_list = int(statement.group(1), 16)
            if table:
                finish_table(); table_offset = int(table.group(1), 16)
            directory_row = re.search(r"include_directories\[\s*(\d+)\]\s*=\s*[\"'](.*)[\"']\s*$", line)
            if directory_row and table_offset is not None:
                line_directories[int(directory_row.group(1))] = directory_row.group(2)
            if table_offset is not None and re.search(r"file_names\[\s*\d+\]", line): finish_file()
            inline_file = re.search(r"file_names\[\s*\d+\].*name:\s*[\"'](.*)[\"']", line)
            if inline_file and table_offset is not None: file_name = inline_file.group(1)
            name_row = re.search(r"\bname:\s*[\"'](.*)[\"']", line)
            if name_row and not inline_file and table_offset is not None: file_name = name_row.group(1)
            index_row = re.search(r"dir_index:\s*(\d+)", line)
            if index_row and table_offset is not None: file_directory = int(index_row.group(1))
        finish_unit(); finish_table()
        unit_paths: set[str] = set()
        for directory, name, statement_list in units.values():
            if directory is None and name is None: continue
            if not directory or not name: raise ValueError(f"incomplete dSYM source records for {arch}")
            unit_paths.add(posixpath.normpath(directory)); unit_paths.add(resolved_native_source(directory, name))
            if statement_list is None: continue
            table_records = line_tables.pop(statement_list, None)
            if table_records is None: raise ValueError(f"missing dSYM line table for {arch}")
            directories, files = table_records
            resolved_directories = {index: resolved_native_source(directory, value) for index, value in directories.items()}
            unit_paths.update(resolved_directories.values())
            for index, value in files:
                if index and index not in resolved_directories: raise ValueError(f"invalid dSYM line directory for {arch}: index {index}, {len(resolved_directories)} directories (indexes {sorted(resolved_directories)[:8]}), file {clipped(value)!r}")
                unit_paths.add(resolved_native_source(resolved_directories.get(index, directory), value))
        # dwarfdump also prints empty tables no unit points at; they name no source.
        line_tables = {offset: records for offset, records in line_tables.items() if records[0] or records[1]}
        if line_tables:
            unbound = "; ".join(f"table 0x{offset:x} ({len(dirs)} dirs, {len(files)} files, first {clipped(files[0][1])!r})" if files else f"table 0x{offset:x} ({len(dirs)} dirs, no files)" for offset, (dirs, files) in sorted(line_tables.items())[:5])
            referenced = sorted({statement_list for _, _, statement_list in units.values() if statement_list is not None})
            raise ValueError(f"unbound dSYM line table for {arch}: {len(line_tables)} unreferenced ({unbound}); {len(units)} units, {len(referenced)} reference a table {[hex(offset) for offset in referenced[:8]]}")
        if not unit_paths:
            raise ValueError(f"missing dSYM source records for {arch}")
        paths.update(unit_paths); slices.append(arch)
    auxiliary, auxiliary_records = macos_auxiliary_source_paths(path, {value.hex().upper() for value in macho_uuids(dwarf).values()})
    paths = {native_source(item) for item in paths | auxiliary}
    reject_private_native(paths)
    return {"schema": SOURCE_INSPECTION_SCHEMA, "format": "macho-dwarf", "native_inspection_sha256": digest(path), "coverage": {"slices": sorted(slices), "auxiliary_source_records": auxiliary_records}, "source_paths": sorted(paths)}


def pdb_streams(path: Path) -> tuple[list[bytes | None], int]:
    data = path.read_bytes()
    signature = b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0"
    if data[:len(signature)] != signature:
        raise ValueError(f"invalid PDB MSF: {path}")
    page_size, fpm, page_count, directory_size, _, block_map = struct.unpack_from("<IIIIII", data, 32)
    if page_size < 512 or page_size & (page_size - 1) or fpm not in (1, 2) or page_count < 3 or len(data) != page_size * page_count:
        raise ValueError(f"invalid PDB pages: {path}")
    directory_pages = (directory_size + page_size - 1) // page_size; block_map_pages = (directory_pages * 4 + page_size - 1) // page_size
    if not directory_pages or block_map + block_map_pages > page_count:
        raise ValueError(f"invalid PDB block map: {path}")
    reserved = {0}
    for interval in range(0, page_count, page_size): reserved.update(block for block in (interval + 1, interval + 2) if block < page_count)
    claimed: set[int] = set()
    def claim(block: int, label: str) -> None:
        if block >= page_count or block in reserved or block in claimed: raise ValueError(f"invalid PDB {label}: {path}")
        claimed.add(block)
    for block in range(block_map, block_map + block_map_pages): claim(block, "block map")
    map_data = b"".join(data[block * page_size:(block + 1) * page_size] for block in range(block_map, block_map + block_map_pages))
    blocks = struct.unpack_from(f"<{directory_pages}I", map_data)
    for block in blocks: claim(block, "directory block")
    directory = b"".join(data[block * page_size:(block + 1) * page_size] for block in blocks)[:directory_size]
    if len(directory) < 4: raise ValueError(f"invalid PDB directory: {path}")
    stream_count = struct.unpack_from("<I", directory)[0]
    if stream_count > (len(directory) - 4) // 4: raise ValueError(f"invalid PDB stream directory: {path}")
    sizes = struct.unpack_from(f"<{stream_count}I", directory, 4); cursor = 4 + 4 * stream_count; streams: list[bytes | None] = []
    for size in sizes:
        if size == 0xFFFFFFFF: streams.append(None); continue
        count = (size + page_size - 1) // page_size
        if cursor + 4 * count > len(directory): raise ValueError(f"invalid PDB stream directory: {path}")
        pages = struct.unpack_from(f"<{count}I", directory, cursor); cursor += 4 * count
        for block in pages: claim(block, "stream block")
        streams.append(b"".join(data[block * page_size:(block + 1) * page_size] for block in pages)[:size])
    return streams, page_size


def pdb_names_table(streams: list[bytes | None]) -> bytes:
    """The PDB-wide /names string buffer, which linked modules' file checksums index into."""
    info = streams[1] if len(streams) > 1 else None
    if info is None or len(info) < 32: raise ValueError("missing PDB info stream")
    size = struct.unpack_from("<I", info, 28)[0]; buffer = info[32:32 + size]; cursor = 32 + size
    if len(buffer) != size or cursor + 8 > len(info): raise ValueError("invalid PDB named stream map")
    count = struct.unpack_from("<I", info, cursor)[0]; cursor += 8
    for _ in range(2):
        if cursor + 4 > len(info): raise ValueError("invalid PDB named stream map")
        cursor += 4 + 4 * struct.unpack_from("<I", info, cursor)[0]
    if cursor + 8 * count > len(info): raise ValueError("invalid PDB named stream map")
    for index in range(count):
        key, stream = struct.unpack_from("<II", info, cursor + 8 * index)
        if key < size and buffer[key:buffer.find(b"\0", key)] == b"/names":
            names = streams[stream] if stream < len(streams) else None
            if names is None or len(names) < 12: raise ValueError("invalid PDB /names stream")
            signature, _, length = struct.unpack_from("<III", names)
            if signature != 0xEFFEEFFE or 12 + length > len(names): raise ValueError("invalid PDB /names stream")
            return names[12:12 + length]
    raise ValueError("PDB has no /names stream")


def _inspect_windows_native(path: Path) -> dict:
    streams, _ = pdb_streams(path)
    if len(streams) < 4 or streams[3] is None or len(streams[3]) < 64:
        raise ValueError("missing PDB DBI source stream")
    dbi = streams[3]; assert dbi is not None
    mod_size, contribution_size, map_size, file_size = struct.unpack_from("<IIII", dbi, 24)
    start = 64 + mod_size + contribution_size + map_size
    if start + file_size > len(dbi) or file_size < 4: raise ValueError("invalid PDB DBI source stream")
    source = dbi[start:start + file_size]; modules, _ = struct.unpack_from("<HH", source)
    indices_offset = 4
    counts_offset = indices_offset + modules * 2
    if not modules or counts_offset + modules * 2 > len(source): raise ValueError("invalid PDB source records")
    counts = struct.unpack_from(f"<{modules}H", source, counts_offset)
    files = sum(counts)
    positions_offset = counts_offset + modules * 2
    offset = positions_offset + files * 4
    if not files or offset > len(source): raise ValueError("missing PDB source records")
    positions = struct.unpack_from(f"<{files}I", source, positions_offset)
    names = source[offset:]; file_paths: list[str] = []; paths: list[str] = []; compilation_directories: list[str] = []
    for position in positions:
        if position >= len(names): raise ValueError("invalid PDB source offset")
        end = names.find(b"\0", position)
        if end < 0: raise ValueError("unterminated PDB source record")
        file_paths.append(names[position:end].decode("utf-8", "strict"))
    modules_data = dbi[64:64 + mod_size]
    cursor = c13_records = environment_records = 0
    while cursor < len(modules_data):
        if cursor + 64 > len(modules_data): raise ValueError("truncated PDB module record")
        stream_index = struct.unpack_from("<H", modules_data, cursor + 34)[0]
        symbol_size, c11_size, c13_size = struct.unpack_from("<III", modules_data, cursor + 36)
        module_end = modules_data.find(b"\0", cursor + 64)
        object_end = modules_data.find(b"\0", module_end + 1) if module_end >= 0 else -1
        if object_end < 0: raise ValueError("unterminated PDB module record")
        next_cursor = (object_end + 4) & ~3
        if next_cursor > len(modules_data): raise ValueError("invalid PDB module record")
        if stream_index != 0xFFFF:
            if stream_index >= len(streams) or streams[stream_index] is None or c11_size: raise ValueError("unsupported PDB module source records")
            module = streams[stream_index]; assert module is not None
            if symbol_size < 4 or symbol_size + c13_size > len(module): raise ValueError("truncated PDB module stream")
            symbols = module[4:symbol_size]; record = 0
            module_directories: list[str] = []
            while record < len(symbols):
                if record + 4 > len(symbols): raise ValueError("truncated CodeView record")
                length, kind = struct.unpack_from("<HH", symbols, record); end = record + 2 + length
                if length < 2 or end > len(symbols): raise ValueError("invalid CodeView record")
                if kind == 0x113D:
                    payload = symbols[record + 5:end]
                    if not payload.endswith(b"\0\0"):
                        raise ValueError("invalid CodeView environment record")
                    fields = payload.rstrip(b"\0").split(b"\0")
                    if not fields or len(fields) % 2:
                        raise ValueError("invalid CodeView environment record")
                    for key, raw in zip(fields[::2], fields[1::2]):
                        if key.lower() in {b"cwd", b"src", b"source"}:
                            value = raw.decode("utf-8", "strict")
                            if key.lower() == b"cwd":
                                compilation_directories.append(value)
                                module_directories.append(value)
                            else: paths.append(resolved_native_source(compilation_directories[-1], value) if not absolute_native(value) and compilation_directories else value)
                            environment_records += 1
                record = end
            c13 = module[symbol_size:symbol_size + c13_size]; cursor13 = 0; strings = b""; offsets: list[int] = []
            while cursor13 < len(c13):
                if cursor13 + 8 > len(c13): raise ValueError("truncated C13 subsection")
                kind, size = struct.unpack_from("<II", c13, cursor13); body = cursor13 + 8; end = body + size
                if end > len(c13): raise ValueError("invalid C13 subsection")
                if kind == 0xF3: strings = c13[body:end]
                elif kind == 0xF4:
                    item = body
                    while item < end:
                        if item + 6 > end: raise ValueError("truncated C13 file checksum")
                        offsets.append(struct.unpack_from("<I", c13, item)[0]); item = (item + 6 + c13[item + 4] + 3) & ~3
                    if item != end: raise ValueError("invalid C13 file checksum")
                cursor13 = (end + 3) & ~3
            if c13_size:
                # A module may carry only line or inlinee subsections and no source files.
                if offsets and not strings: strings = pdb_names_table(streams)
                for position in offsets:
                    if position >= len(strings): raise ValueError("invalid C13 source offset")
                    end = strings.find(b"\0", position)
                    if end < 0: raise ValueError("unterminated C13 source record")
                    value = strings[position:end].decode("utf-8", "strict")
                    if not absolute_native(value):
                        if len(set(module_directories)) != 1:
                            raise ValueError("unresolved relative C13 source path")
                        value = resolved_native_source(module_directories[0], value)
                    paths.append(value)
                c13_records += len(offsets)
        cursor = next_cursor
    if any(not absolute_native(value) for value in file_paths) and len(set(compilation_directories)) != 1: raise ValueError("unresolved relative PDB source path")
    paths.extend(compilation_directories)
    paths.extend(resolved_native_source(compilation_directories[0], value) if not absolute_native(value) else value for value in file_paths)
    paths = [native_source(item) for item in paths]
    reject_private_native(paths)
    return {"schema": SOURCE_INSPECTION_SCHEMA, "format": "pdb-dbi", "native_inspection_sha256": digest(path), "coverage": {"dbi_stream": 3, "file_records": len(file_paths), "module_c13_records": c13_records, "environment_records": environment_records}, "source_paths": sorted(set(paths))}


def stable_native_inspection(path: Path, inspect) -> dict:
    before = digest(path)
    record = inspect()
    after = digest(path)
    if before != after or record.get("native_inspection_sha256") != before:
        raise ValueError("native changed during source inspection")
    return record


def inspect_linux_native(path: Path) -> dict:
    return stable_native_inspection(path, lambda: _inspect_linux_native(path))


def inspect_macos_native(path: Path, logical_name: str) -> dict:
    return stable_native_inspection(path, lambda: _inspect_macos_native(path, logical_name))


def inspect_windows_native(path: Path) -> dict:
    return stable_native_inspection(path, lambda: _inspect_windows_native(path))


def inspect_native(platform: str, path: Path, logical_name: str) -> dict:
    if platform == "x86_64-unknown-linux-gnu": return inspect_linux_native(path)
    if platform.endswith("apple-darwin"): return inspect_macos_native(path, logical_name)
    if platform == "x86_64-pc-windows-msvc": return inspect_windows_native(path)
    raise ValueError("unsupported source inspection platform")


def macho_uuids(path: Path) -> dict[str, bytes]:
    """The UUID of a thin arm64 Mach-O, keyed by its architecture."""
    data = path.read_bytes()
    if len(data) < 32:
        raise ValueError(f"invalid Mach-O: {path}")
    magic, cpu, _, _, commands, command_bytes, _, _ = struct.unpack_from("<IIIIIIII", data)
    if magic != 0xFEEDFACF or cpu != 0x100000C or command_bytes < 24 or 32 + command_bytes > len(data):
        raise ValueError(f"invalid arm64 Mach-O header: {path}")
    cursor, end, identifier = 32, 32 + command_bytes, None
    for _ in range(commands):
        if cursor + 8 > end:
            break
        command, length = struct.unpack_from("<II", data, cursor)
        if length < 8 or cursor + length > end:
            break
        if command == 0x1B and length == 24:
            identifier = data[cursor + 8:cursor + 24]
        cursor += length
    if identifier is None:
        raise ValueError(f"missing Mach-O UUID: {path}")
    return {"arm64": identifier}


def native_module_id(platform: str, native: Path, logical_name: str) -> str:
    if platform == "x86_64-unknown-linux-gnu":
        if elf_machine(native) != 62:
            raise ValueError(f"unexpected ELF machine: {native}")
        return elf_module_id(native)
    if platform == "x86_64-pc-windows-msvc":
        guid, age = pdb_info(native)
        return uuid.UUID(bytes_le=guid).hex.upper() + f"{age:X}"
    dwarf = native / "Contents/Resources/DWARF" / logical_name
    if not dwarf.is_file():
        raise ValueError(f"missing dSYM DWARF file: {native}")
    uuid_by_arch = macho_uuids(dwarf)
    return uuid_by_arch["arm64"].hex().upper() + "0"


def assert_safe_tree(root: Path, allowed_files: set[str]) -> None:
    actual: set[str] = set()
    for directory, names, files in os.walk(root, followlinks=False):
        current = Path(directory)
        if current.is_symlink() or any((current / name).is_symlink() for name in names + files):
            raise ValueError("symbol tree contains symlink")
        if not names and not files and current != root:
            raise ValueError("symbol tree contains empty directory")
        actual.update((current / name).relative_to(root).as_posix() for name in files)
    if actual != allowed_files:
        raise ValueError(f"symbol tree contains stale or missing files: stale {sorted(actual - allowed_files)}, missing {sorted(allowed_files - actual)}")


def write_inventory(root: Path, platform: str, version: str, native: dict[str, Path], symbols_by_name: dict[str, list[Path]] | None = None, inspections: dict[str, dict] | None = None) -> Path:
    store = root / "store"
    entries = []
    for name, path in sorted(native.items()):
        if inspections is None or name not in inspections:
            raise ValueError("missing native source inspection")
        inspection = valid_source_inspection(platform, path, inspections[name])
        matches = sorted(symbols_by_name[name] if symbols_by_name else (store / name).glob("*/*.sym"))
        if not matches:
            raise ValueError(f"missing .sym for {name}")
        symbols = []
        for match in matches:
            module_id, module_name = module(match)
            # dump_syms files a PDB's symbols as `<name>.pdb/<id>/<name>.sym`, the Breakpad convention.
            file_names = {f"{module_name}.sym", f"{module_name[:-4]}.sym" if module_name.lower().endswith(".pdb") else ""}
            if match.parent.name != module_id or match.name not in file_names or match.parent.parent.name != module_name:
                raise ValueError(f"invalid store layout for {name}")
            symbols.append({"path": match.relative_to(root).as_posix(), "sha256": digest(match), "module_id": module_id, "module_name": module_name})
        if len({item["module_id"] for item in symbols}) != len(symbols):
            raise ValueError(f"duplicate Breakpad module ID for {name}")
        entry = {"name": name, "native": path.relative_to(root).as_posix(), "native_sha256": digest(path), "native_id": native_module_id(platform, path, name), "native_source_inspection": inspection, "symbols": symbols}
        if platform == "x86_64-unknown-linux-gnu":
            entry["native_build_id"] = elf_build_id(path)
        if platform.endswith("apple-darwin"):
            dwarf = path / "Contents/Resources/DWARF" / name
            entry["native_uuids"] = {arch: value.hex().upper() for arch, value in macho_uuids(dwarf).items()}
        entries.append(entry)
    manifest = root / "symbol-inventory.json"
    manifest.write_text(json.dumps({"platform": platform, "version": version, "entries": entries}, indent=2, sort_keys=True) + "\n")
    return manifest


def verify_inventory(root: Path, manifest: Path | None = None) -> dict:
    data = json.loads((manifest or root / "symbol-inventory.json").read_text())
    platform = data.get("platform")
    if not isinstance(platform, str) or not isinstance(data.get("version"), str) or not data["version"]:
        raise ValueError("invalid symbol inventory metadata")
    os_name, cpu, suffix = platform_header(platform)
    entries = data.get("entries")
    if not isinstance(entries, list) or {item.get("name") for item in entries if isinstance(item, dict)} != {"focusforge-cli", "focusforge-gui"}:
        raise ValueError("symbol inventory must contain GUI and CLI exactly once")
    names: set[str] = set()
    for item in entries:
        name = item.get("name")
        if not isinstance(name, str) or name in names:
            raise ValueError("duplicate symbol inventory name")
        names.add(name)
        native_path = Path(item.get("native", ""))
        if native_path.is_absolute() or ".." in native_path.parts or native_path.parts[:1] != ("native",):
            raise ValueError("unsafe native inventory path")
        native = root / native_path
        symbols = item.get("symbols")
        if not native.exists() or native.is_symlink() or native.suffix != suffix or digest(native) != item["native_sha256"] or not isinstance(symbols, list) or len(symbols) != 1:
            raise ValueError(f"symbol inventory hash mismatch for {name}")
        native_id = native_module_id(platform, native, name)
        if item.get("native_id") != native_id:
            raise ValueError(f"native identity mismatch for {name}")
        valid_source_inspection(platform, native, item.get("native_source_inspection"))
        if platform == "x86_64-unknown-linux-gnu" and item.get("native_build_id") != elf_build_id(native):
            raise ValueError(f"native full build ID mismatch for {name}")
        if platform.endswith("apple-darwin"):
            dwarf = native / "Contents/Resources/DWARF" / name
            actual_uuids = {arch: value.hex().upper() for arch, value in macho_uuids(dwarf).items()}
            required_files = {"Contents/Resources/DWARF/" + name}
            standard_files = required_files | {"Contents/Info.plist"}
            actual_files = {path.relative_to(native).as_posix() for path in native.rglob("*") if path.is_file()}
            auxiliary = {f"Contents/Resources/{value}.plist" for value in actual_uuids.values()} | {f"Contents/Resources/{value[:8]}-{value[8:12]}-{value[12:16]}-{value[16:20]}-{value[20:]}.plist" for value in actual_uuids.values()}
            if actual_files not in (required_files, standard_files) and not (standard_files.issubset(actual_files) and all(path in standard_files or path in auxiliary or path.startswith("Contents/Resources/Relocations/") for path in actual_files)) or item.get("native_uuids") != actual_uuids:
                raise ValueError(f"invalid dSYM payload for {name}")
        for symbol_data in symbols:
            symbol_path = Path(symbol_data.get("path", ""))
            if symbol_path.is_absolute() or ".." in symbol_path.parts or symbol_path.parts[:1] != ("store",):
                raise ValueError("unsafe symbol inventory path")
            symbol = root / symbol_path
            if not symbol.is_file() or digest(symbol) != symbol_data["sha256"]:
                raise ValueError(f"symbol inventory hash mismatch for {name}")
            module_os, module_cpu, module_id, module_name = module_header(symbol)
            expected_module = native.name if platform == "x86_64-pc-windows-msvc" else name
            # Breakpad producers disagree on the case of the OS token ("windows" and "Windows").
            checks = {"os": module_os.lower() == os_name.lower(), "cpu": module_cpu == cpu, "name": module_name == expected_module, "recorded name": module_name == symbol_data.get("module_name"), "id": module_id == native_id, "recorded id": module_id == symbol_data["module_id"], "id directory": symbol.parent.name == module_id, "name directory": symbol.parent.parent.name == module_name}
            failed = [f"{label}" for label, holds in checks.items() if not holds]
            if failed:
                raise ValueError(f"symbol inventory module mismatch for {name}: {', '.join(failed)} (header {module_os} {module_cpu} {module_id} {module_name}; expected {os_name} {cpu} {native_id} {expected_module})")
    expected = {"symbol-inventory.json"}
    for item in entries:
        native = root / item["native"]
        if native.is_dir():
            expected.update(path.relative_to(root).as_posix() for path in native.rglob("*") if path.is_file())
        else:
            expected.add(item["native"])
        expected.update(symbol["path"] for symbol in item["symbols"])
    archive = root / f"FocusForge-{data['version']}-{platform}-symbols.tar.gz"
    if archive.is_file():
        expected.add(archive.name)
        if (root / "symbols.sha256").is_file():
            expected.add("symbols.sha256")
    assert_safe_tree(root, expected)
    return data


def verify_archive(root: Path, archive: Path) -> dict:
    """Require archive bytes to be the exact safe inventory payload."""
    inventory = verify_inventory(root)
    expected = {"symbol-inventory.json"}
    for entry in inventory["entries"]:
        native = root / entry["native"]
        if native.is_dir():
            expected.update(path.relative_to(root).as_posix() for path in native.rglob("*") if path.is_file())
        else:
            expected.add(entry["native"])
        expected.update(symbol["path"] for symbol in entry["symbols"])
    with tarfile.open(archive, "r:gz") as source:
        members = source.getmembers()
        names = [member.name for member in members]
        if len(names) != len(set(names)) or any(Path(name).is_absolute() or ".." in Path(name).parts or not member.isfile() for name, member in zip(names, members)):
            raise ValueError("unsafe symbol archive")
        if set(names) != expected:
            raise ValueError("symbol archive payload mismatch")
        for member in members:
            extracted = source.extractfile(member)
            if extracted is None or hashlib.sha256(extracted.read()).hexdigest() != digest(root / member.name):
                raise ValueError("symbol archive content mismatch")
    return inventory


def elf_build_id(path: Path) -> str:
    data = path.read_bytes()
    if data[:5] != b"\x7fELF\x02" or data[5] != 1:
        raise ValueError(f"invalid ELF: {path}")
    section_offset = struct.unpack_from("<Q", data, 40)[0]
    entry_size, count, names_index = struct.unpack_from("<HHH", data, 58)
    if entry_size < 64 or not count or names_index >= count or section_offset + entry_size * count > len(data):
        raise ValueError(f"invalid ELF sections: {path}")
    def section(index: int) -> tuple[int, int, int, int]:
        offset = section_offset + index * entry_size
        name, kind = struct.unpack_from("<II", data, offset)
        start, size = struct.unpack_from("<QQ", data, offset + 24)
        return name, kind, start, size
    _, _, strings_start, strings_size = section(names_index)
    strings = data[strings_start:strings_start + strings_size]
    for index in range(count):
        name, kind, start, size = section(index)
        label = strings[name:strings.find(b"\0", name)].decode("ascii", "ignore") if name < len(strings) else ""
        if label != ".note.gnu.build-id" or kind != 7 or start + size > len(data):
            continue
        cursor, end = start, start + size
        while cursor + 12 <= end:
            names, description, kind = struct.unpack_from("<III", data, cursor); cursor += 12
            owner = data[cursor:cursor + names].rstrip(b"\0"); cursor += (names + 3) & ~3
            value = data[cursor:cursor + description]; cursor += (description + 3) & ~3
            if owner == b"GNU" and kind == 3 and value:
                return value.hex()
    raise ValueError(f"missing ELF build ID: {path}")


def elf_machine(path: Path) -> int:
    data = path.read_bytes()
    if data[:5] != b"\x7fELF\x02" or data[5] != 1 or len(data) < 20:
        raise ValueError(f"invalid ELF: {path}")
    return struct.unpack_from("<H", data, 18)[0]


def elf_module_id(path: Path) -> str:
    value = elf_build_id(path)
    if len(value) < 32:
        raise ValueError(f"short ELF build ID: {path}")
    raw = bytes.fromhex(value[:32])
    return (raw[0:4][::-1] + raw[4:6][::-1] + raw[6:8][::-1] + raw[8:16]).hex().upper() + "0"


def verify_linux(root: Path, pairs: list[str]) -> None:
    inventory = verify_inventory(root)
    expected = {item["name"]: {symbol["module_id"] for symbol in item["symbols"]} for item in inventory["entries"]}
    seen: set[str] = set()
    for pair in pairs:
        name, value = pair.split("=", 1)
        paths = value.split(",")
        if len(paths) != 3 or name not in expected or name in seen:
            raise ValueError("invalid Linux identity input")
        seen.add(name)
        build_ids = {elf_build_id(Path(path)) for path in paths}
        module_ids = {elf_module_id(Path(path)) for path in paths}
        if len(build_ids) != 1 or len(module_ids) != 1 or module_ids.pop() not in expected[name]:
            raise ValueError(f"Linux native/Breakpad identity mismatch for {name}")
    if seen != {"focusforge-cli", "focusforge-gui"}:
        raise ValueError("Linux identity inputs must name GUI and CLI exactly once")


def pe_rsds_record(path: Path) -> tuple[bytes, int, str]:
    data = path.read_bytes()
    if data[:2] != b"MZ" or len(data) < 0x40:
        raise ValueError(f"invalid PE: {path}")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError(f"missing PE header: {path}")
    sections = struct.unpack_from("<H", data, pe + 6)[0]
    optional_size = struct.unpack_from("<H", data, pe + 20)[0]
    optional = pe + 24
    magic = struct.unpack_from("<H", data, optional)[0]
    directory_offset = optional + (96 if magic == 0x10B else 112 if magic == 0x20B else 0)
    if not directory_offset or struct.unpack_from("<I", data, directory_offset - 4)[0] < 7:
        raise ValueError(f"missing PE debug directory: {path}")
    debug_rva, debug_size = struct.unpack_from("<II", data, directory_offset + 8 * 6)
    section_offset = optional + optional_size
    def raw_offset(rva: int) -> int:
        for index in range(sections):
            entry = section_offset + index * 40
            virtual_size, virtual_address, raw_size, raw = struct.unpack_from("<IIII", data, entry + 8)
            if virtual_address <= rva < virtual_address + max(virtual_size, raw_size):
                return raw + rva - virtual_address
        raise ValueError(f"unmapped PE RVA: {path}")
    for offset in range(raw_offset(debug_rva), raw_offset(debug_rva) + debug_size, 28):
        kind, size = struct.unpack_from("<II", data, offset + 12)
        pointer = struct.unpack_from("<I", data, offset + 24)[0]
        if kind == 2 and data[pointer:pointer + 4] == b"RSDS" and size >= 24:
            end = data.find(b"\0", pointer + 24)
            if end < 0:
                raise ValueError(f"unterminated PE PDB path: {path}")
            return data[pointer + 4:pointer + 20], struct.unpack_from("<I", data, pointer + 20)[0], data[pointer + 24:end].decode("utf-8")
    raise ValueError(f"missing PE RSDS record: {path}")


def pe_rsds(path: Path) -> tuple[bytes, int]:
    guid, age, _ = pe_rsds_record(path)
    return guid, age


def pdb_info(path: Path) -> tuple[bytes, int]:
    data = path.read_bytes()
    signature = b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0"
    if data[:len(signature)] != signature:
        raise ValueError(f"invalid PDB MSF: {path}")
    page_size, fpm, page_count, directory_size, unknown, block_map = struct.unpack_from("<IIIIII", data, 32)
    if page_size < 512 or page_size & (page_size - 1) or fpm not in (1, 2) or page_count < 3 or len(data) != page_size * page_count:
        raise ValueError(f"invalid PDB pages: {path}")
    directory_pages = (directory_size + page_size - 1) // page_size
    block_map_pages = (directory_pages * 4 + page_size - 1) // page_size
    if not directory_pages or block_map + block_map_pages > page_count:
        raise ValueError(f"invalid PDB block map: {path}")
    reserved = {0}
    for interval in range(0, page_count, page_size):
        reserved.update(block for block in (interval + 1, interval + 2) if block < page_count)
    claimed: set[int] = set()
    def claim(block: int, label: str) -> None:
        if block >= page_count or block in reserved or block in claimed:
            raise ValueError(f"invalid PDB {label}: {path}")
        claimed.add(block)
    for block in range(block_map, block_map + block_map_pages):
        claim(block, "block map")
    map_data = b"".join(data[block * page_size:(block + 1) * page_size] for block in range(block_map, block_map + block_map_pages))
    blocks = struct.unpack_from(f"<{directory_pages}I", map_data)
    for block in blocks:
        claim(block, "directory block")
    directory = b"".join(data[block * page_size:(block + 1) * page_size] for block in blocks)[:directory_size]
    if len(directory) < 4:
        raise ValueError(f"invalid PDB directory: {path}")
    stream_count = struct.unpack_from("<I", directory)[0]
    if stream_count > (len(directory) - 4) // 4:
        raise ValueError(f"invalid PDB stream directory: {path}")
    sizes = struct.unpack_from(f"<{stream_count}I", directory, 4)
    cursor = 4 + 4 * stream_count
    stream_one: bytes | None = None
    for index, size in enumerate(sizes):
        if size == 0xFFFFFFFF:
            continue
        count = (size + page_size - 1) // page_size
        if cursor + 4 * count > len(directory):
            raise ValueError(f"invalid PDB stream directory: {path}")
        pages = struct.unpack_from(f"<{count}I", directory, cursor); cursor += 4 * count
        for block in pages:
            claim(block, "stream block")
        if index == 1:
            stream_one = b"".join(data[block * page_size:(block + 1) * page_size] for block in pages)[:size]
    if stream_count < 2 or stream_one is None or len(stream_one) < 28:
        raise ValueError(f"missing PDB info stream: {path}")
    info = stream_one
    return info[12:28], struct.unpack_from("<I", info, 8)[0]


def verify_windows(pairs: list[str]) -> None:
    for pair in pairs:
        name, value = pair.split("=", 1); pe, pdb = map(Path, value.split(","))
        if not name.startswith("focusforge-") or pe_rsds(pe) != pdb_info(pdb):
            raise ValueError(f"PE/PDB GUID+age mismatch for {name}")


def dsym_dwarf_file(native: Path) -> Path | None:
    if not native.is_dir(): return None
    files = [item for item in (native / "Contents/Resources/DWARF").glob("*") if item.is_file()]
    if len(files) != 1: raise ValueError(f"dSYM bundle without exactly one DWARF file: {native}")
    return files[0]


def add_binary_cfi(dump_syms: str, arch: str | None, binary: str, symbol: Path, scratch: Path) -> None:
    """dsymutil copies no unwind tables, so take the STACK records from the linked binary itself."""
    lines = symbol.read_text(encoding="utf-8").splitlines(keepends=True)
    if any(line.startswith("STACK CFI ") for line in lines): return
    shutil.rmtree(scratch, ignore_errors=True)
    subprocess.run([dump_syms, "--store", str(scratch), *(["--arch", arch] if arch else []), binary], check=True)
    found = list(scratch.glob("*/*/*.sym"))
    if len(found) != 1: raise ValueError(f"expected one Breakpad file from {binary}, found {len(found)}")
    other = found[0].read_text(encoding="utf-8").splitlines(keepends=True)
    if other[0].split()[:4] != lines[0].split()[:4]: raise ValueError(f"binary and dSYM disagree on the module: {other[0].strip()!r} vs {lines[0].strip()!r}")
    symbol.write_text("".join(lines + [line for line in other if line.startswith("STACK ")]), encoding="utf-8")
    shutil.rmtree(scratch, ignore_errors=True)


def build(args: argparse.Namespace) -> None:
    output = Path(args.output).resolve()
    output_root = Path(args.output_root).resolve()
    protected = (ROOT, Path.cwd().resolve(), *ROOT.parents)
    if output == output_root or output_root == ROOT or not output.is_relative_to(output_root) or any(path.is_relative_to(output) for path in protected):
        raise ValueError("symbol output must be a child of its explicit scratch output root")
    shutil.rmtree(output, ignore_errors=True)
    (output / "native").mkdir(parents=True)
    native: dict[str, Path] = {}
    for pair in args.native:
        name, value = pair.split("=", 1); source = Path(value).resolve(); destination = output / "native" / source.name
        if not source.exists() or name in native:
            raise ValueError("invalid native input")
        if source.is_dir(): shutil.copytree(source, destination)
        else: shutil.copyfile(source, destination)
        native[name] = destination
    symbols_by_name: dict[str, list[Path]] = {name: [] for name in native}
    for pair in args.binary:
        name, value = pair.split("=", 1)
        if name not in native: raise ValueError("binary lacks native symbol input")
        for arch in args.arch or [None]:
            before = set((output / "store").glob("*/*/*.sym"))
            command = [args.dump_syms, "--inlines", "--store", str(output / "store")]
            if arch: command.extend(["--arch", arch])
            dwarf = dsym_dwarf_file(native[name])
            # A linked Mach-O keeps only a debug map; the line tables live in the dSYM.
            subprocess.run([*command, str(dwarf or value)], check=True)
            after = set((output / "store").glob("*/*/*.sym"))
            if dwarf:
                for symbol in sorted(after - before): add_binary_cfi(args.dump_syms, arch, value, symbol, output / "cfi-store")
            symbols_by_name[name].extend(sorted(after - before))
    for symbols in symbols_by_name.values():
        for symbol in symbols:
            canonicalize_symbol_sources(symbol)
    inspections = {name: inspect_native(args.platform, path, name) for name, path in native.items()}
    manifest = write_inventory(output, args.platform, args.version, native, symbols_by_name, inspections)
    inventory = verify_inventory(output, manifest)
    if any(len(item["symbols"]) != args.symbols_per_native for item in inventory["entries"]):
        raise ValueError("unexpected number of Breakpad symbols per native input")
    archive = output / f"FocusForge-{args.version}-{args.platform}-symbols.tar.gz"
    with tarfile.open(archive, "w:gz", format=tarfile.PAX_FORMAT) as result:
        for path in sorted([* (output / "native").rglob("*"), *(output / "store").rglob("*"), manifest]):
            if path.is_file(): result.add(path, arcname=path.relative_to(output), recursive=False)
    verify_archive(output, archive)
    (output / "symbols.sha256").write_text(f"{digest(archive)}  {archive.name}\n")


def main() -> None:
    parser = argparse.ArgumentParser(); command = parser.add_subparsers(dest="command", required=True)
    command.add_parser("check-config")
    verify_parser = command.add_parser("verify"); verify_parser.add_argument("root", type=Path)
    archive_parser = command.add_parser("verify-archive"); archive_parser.add_argument("root", type=Path); archive_parser.add_argument("archive", type=Path)
    linux_parser = command.add_parser("verify-linux"); linux_parser.add_argument("--root", required=True, type=Path); linux_parser.add_argument("--identity", action="append", required=True)
    windows_parser = command.add_parser("verify-windows"); windows_parser.add_argument("--identity", action="append", required=True)
    pdb_parser = command.add_parser("windows-pdb"); pdb_parser.add_argument("pe", type=Path)
    install_parser = command.add_parser("install"); install_parser.add_argument("tool", choices=("dump_syms", "stackwalk")); install_parser.add_argument("target"); install_parser.add_argument("directory")
    build_parser = command.add_parser("build"); build_parser.add_argument("--platform", required=True); build_parser.add_argument("--version", required=True); build_parser.add_argument("--dump-syms", required=True); build_parser.add_argument("--output", required=True); build_parser.add_argument("--output-root", default=ROOT / ".scratch"); build_parser.add_argument("--native", action="append", required=True); build_parser.add_argument("--binary", action="append", required=True); build_parser.add_argument("--arch", action="append"); build_parser.add_argument("--symbols-per-native", type=int, default=1)
    args = parser.parse_args(); data = check_config()
    if args.command == "install": print(install(data, args.tool, args.target, Path(args.directory)))
    elif args.command == "build": build(args)
    elif args.command == "verify": verify_inventory(args.root)
    elif args.command == "verify-archive": verify_archive(args.root, args.archive)
    elif args.command == "verify-linux": verify_linux(args.root, args.identity)
    elif args.command == "verify-windows": verify_windows(args.identity)
    elif args.command == "windows-pdb": print(Path(pe_rsds_record(args.pe)[2]).name)


if __name__ == "__main__":
    main()
