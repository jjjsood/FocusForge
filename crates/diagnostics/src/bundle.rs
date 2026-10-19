// SPDX-License-Identifier: GPL-3.0-or-later

//! The diagnostics bundle: one ZIP holding the rotated logs, the crash
//! evidence, a system summary, the graphics adapter record, the effective
//! configuration and version -- nothing in it is sent anywhere.
//!
//! [`manifest`] resolves every entry without touching an output path;
//! [`write()`] stores exactly those entries, redacting text as it goes.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::crash::{crash_dir, read_dir_sorted, rfc3339};
use crate::log_dir;

/// What the home directory and the user name are replaced with.
const HOME_PLACEHOLDER: &str = "[home]";
const USER_PLACEHOLDER: &str = "[user]";

/// The generated entries' names.
const SYSTEM_ENTRY: &str = "system.json";
const ADAPTER_ENTRY: &str = "adapter.json";
const CONFIGURATION_ENTRY: &str = "configuration.json";
const VERSION_ENTRY: &str = "version.json";

/// What a bundle is built from.
#[derive(Debug, Default, Clone)]
pub struct Options {
    /// The data directory holding `logs/` and `crashes/`. `None` collects
    /// nothing, and the bundle holds the generated entries alone.
    pub data_dir: Option<PathBuf>,
    /// The configuration directory the graphical shell's input map lives
    /// in, reported by the effective-configuration entry.
    pub config_dir: Option<PathBuf>,
    /// Whether the minidump binaries themselves are included. They cannot
    /// be redacted.
    pub include_minidumps: bool,
}

/// The text substitutions applied to every redacted entry. Both values are
/// passed in rather than read inside the writer, so a caller -- a test
/// included -- controls exactly what is replaced.
#[derive(Debug, Default, Clone)]
pub struct Redaction {
    /// The home directory path, replaced wherever it occurs.
    pub home: Option<PathBuf>,
    /// The user name, replaced wherever it occurs.
    pub user: Option<String>,
}

impl Redaction {
    /// This process's own home directory and user name.
    pub fn from_env() -> Self {
        Redaction {
            home: directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()),
            user: std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .ok()
                .filter(|name| !name.is_empty()),
        }
    }

    /// `text` with every occurrence of the home path and then of the user
    /// name replaced. The order is load-bearing: the home path contains the
    /// user name, so replacing the name first would leave the path
    /// unmatched and only partly redacted.
    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        if let Some(home) = self.home.as_deref().and_then(Path::to_str)
            && !home.is_empty()
        {
            out = out.replace(home, HOME_PLACEHOLDER);
        }
        if let Some(user) = self.user.as_deref().filter(|u| !u.is_empty()) {
            out = out.replace(user, USER_PLACEHOLDER);
        }
        out
    }
}

/// What an entry holds, which is what the printed list says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A generated summary, built in memory.
    Generated,
    /// A log file.
    Log,
    /// A panic report.
    PanicReport,
    /// A native crash's metadata.
    NativeCrashMetadata,
    /// A minidump binary.
    Minidump,
}

impl EntryKind {
    fn label(self) -> &'static str {
        match self {
            EntryKind::Generated => "generated",
            EntryKind::Log => "log",
            EntryKind::PanicReport => "panic report",
            EntryKind::NativeCrashMetadata => "native crash metadata",
            EntryKind::Minidump => "minidump",
        }
    }
}

/// Where an entry's bytes come from.
#[derive(Debug, Clone)]
pub enum EntrySource {
    /// A file on disk, copied into the archive.
    File(PathBuf),
    /// Text built in memory.
    Generated(String),
}

/// One archive entry.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The name inside the archive, never a path from the host.
    pub name: String,
    /// What the entry holds.
    pub kind: EntryKind,
    /// Where its bytes come from.
    pub source: EntrySource,
    /// Whether the entry's text is redacted on the way in. Only a binary
    /// minidump is not.
    pub redacted: bool,
}

/// Every entry a bundle will hold, in archive order, plus the evidence left
/// out of it.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// The entries, generated ones first and collected ones after, each
    /// group by name.
    pub entries: Vec<Entry>,
    /// Evidence found but not included, by the name it would have had.
    pub excluded: Vec<String>,
}

impl Manifest {
    /// The bundle's contents as one line per entry -- its name, what it
    /// holds and whether it is redacted -- followed by one line per piece
    /// of excluded evidence.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .entries
            .iter()
            .map(|entry| {
                let redaction = if entry.redacted {
                    "redacted"
                } else {
                    "not redacted"
                };
                format!("{} ({}, {redaction})", entry.name, entry.kind.label())
            })
            .collect();
        lines.extend(
            self.excluded
                .iter()
                .map(|name| format!("excluded: {name} (minidump, cannot be redacted)")),
        );
        lines
    }

    /// Whether any log file was collected.
    pub fn has_logs(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.kind == EntryKind::Log)
    }

    /// Whether any crash evidence was collected, or found and left out.
    pub fn has_crash_evidence(&self) -> bool {
        !self.excluded.is_empty()
            || self.entries.iter().any(|entry| {
                matches!(
                    entry.kind,
                    EntryKind::PanicReport | EntryKind::NativeCrashMetadata | EntryKind::Minidump
                )
            })
    }
}

/// What a finished bundle is, and what went into it.
#[derive(Debug, Clone)]
pub struct Written {
    /// The archive's path.
    pub path: PathBuf,
    /// Every collected source file the archive holds.
    pub sources: Vec<PathBuf>,
}

/// Why a bundle could not be written.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// The output path exists and overwriting was not asked for.
    #[error("{} exists", .0.display())]
    OutputExists(PathBuf),
    /// A file could not be read, created or renamed.
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The archive itself refused an entry.
    #[error("{}: {source}", path.display())]
    Archive {
        path: PathBuf,
        source: zip::result::ZipError,
    },
}

/// Resolves every entry the bundle will hold. Reads the data directory and
/// the registry; writes nothing.
pub fn manifest(options: &Options) -> Manifest {
    let (collected, excluded) = collect(options);
    let logs: Vec<&Path> = collected
        .iter()
        .filter(|entry| entry.kind == EntryKind::Log)
        .filter_map(|entry| match &entry.source {
            EntrySource::File(path) => Some(path.as_path()),
            EntrySource::Generated(_) => None,
        })
        .collect();

    let mut entries = vec![
        generated(SYSTEM_ENTRY, system_summary()),
        generated(ADAPTER_ENTRY, adapter_record(&logs)),
        generated(
            CONFIGURATION_ENTRY,
            configuration(options.config_dir.as_deref()),
        ),
        generated(VERSION_ENTRY, version(&excluded)),
    ];
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries.extend(collected);
    Manifest { entries, excluded }
}

fn generated(name: &str, body: serde_json::Value) -> Entry {
    Entry {
        name: name.to_string(),
        kind: EntryKind::Generated,
        source: EntrySource::Generated(
            serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()),
        ),
        redacted: true,
    }
}

/// The allowlist: `logs/focusforge.jsonl` and its rotated siblings, every
/// `crashes/*.json`, and `crashes/*.dmp` only when asked for. Anything else
/// under the data directory is skipped, which is what keeps image data out
/// of a bundle no matter what has been dropped there.
fn collect(options: &Options) -> (Vec<Entry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut excluded = Vec::new();
    let Some(data_dir) = options.data_dir.as_deref() else {
        return (entries, excluded);
    };

    for (path, name) in read_dir_sorted(&log_dir(data_dir)) {
        if is_log_name(&name) {
            entries.push(Entry {
                name: format!("logs/{name}"),
                kind: EntryKind::Log,
                source: EntrySource::File(path),
                redacted: true,
            });
        }
    }

    for (path, name) in read_dir_sorted(&crash_dir(data_dir)) {
        let entry_name = format!("crashes/{name}");
        if name.ends_with(".json") {
            entries.push(Entry {
                name: entry_name,
                kind: crash_kind(&path),
                source: EntrySource::File(path),
                redacted: true,
            });
        } else if name.ends_with(".dmp") {
            if options.include_minidumps {
                entries.push(Entry {
                    name: entry_name,
                    kind: EntryKind::Minidump,
                    source: EntrySource::File(path),
                    redacted: false,
                });
            } else {
                excluded.push(entry_name);
            }
        }
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    excluded.sort();
    (entries, excluded)
}

/// `focusforge.jsonl` and `focusforge.<n>.jsonl`, and nothing else.
fn is_log_name(name: &str) -> bool {
    match name.strip_prefix("focusforge.").and_then(|rest| {
        rest.strip_suffix(".jsonl")
            .or_else(|| (rest == "jsonl").then_some(""))
    }) {
        Some("") => true,
        Some(number) => !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// A crash `.json` naming a minidump is that dump's metadata; anything else
/// is a panic report, including a file that does not parse -- a truncated
/// report is still evidence.
fn crash_kind(path: &Path) -> EntryKind {
    let names_a_dump = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("minidump").cloned())
        .is_some_and(|dump| !dump.is_null());
    if names_a_dump {
        EntryKind::NativeCrashMetadata
    } else {
        EntryKind::PanicReport
    }
}

/// The host's memory figures, core count and operating system. The host
/// name is not read.
fn system_summary() -> serde_json::Value {
    let facts = focusforge_core::host::snapshot();
    let os = focusforge_core::host::os_description();
    serde_json::json!({
        "total_memory": facts.total_memory(),
        "available_memory": facts.available_memory(),
        "cgroup_limit": facts.cgroup_limit(),
        "usable_memory": facts.usable_memory(),
        "usable_available": facts.usable_available(),
        "logical_cores": facts.logical_cores(),
        "facts_origin": format!("{:?}", facts.origin()),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "os_name": os.name,
        "os_version": os.os_version,
        "kernel_version": os.kernel_version,
    })
}

/// The messages the graphics device raises when it selects an adapter.
const ADAPTER_SELECTED_MESSAGE: &str = "graphics adapter selected";
const ADAPTER_LIMITS_MESSAGE: &str = "graphics adapter and device limits";

/// The adapter record recovered from the collected logs: the last selection
/// line and the last limits line. No device is initialised here.
fn adapter_record(logs: &[&Path]) -> serde_json::Value {
    let mut selected = None;
    let mut limits = None;
    for path in logs {
        let Ok(file) = File::open(path) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            match value.get("message").and_then(|m| m.as_str()) {
                Some(ADAPTER_SELECTED_MESSAGE) => selected = Some(value),
                Some(ADAPTER_LIMITS_MESSAGE) => limits = Some(value),
                _ => {}
            }
        }
    }
    match selected {
        Some(selected) => serde_json::json!({ "selected": selected, "limits": limits }),
        None => serde_json::json!({
            "selected": serde_json::Value::Null,
            "limits": limits,
            "note": "the collected logs hold no graphics adapter record",
        }),
    }
}

/// What a run started now would resolve every registered algorithm's
/// parameters to, with each value's provenance, plus the graphical shell's
/// input map and whether it exists. No stack context, so a stack `auto`
/// hook is reported as present and unevaluated while a host-derived default
/// is evaluated.
fn configuration(config_dir: Option<&Path>) -> serde_json::Value {
    let facts = focusforge_core::host::snapshot();
    let explicit = focusforge_core::params::ParamValues::default();
    let algorithms: Vec<serde_json::Value> = focusforge_core::registry::algorithms()
        .iter()
        .map(|entry| {
            let params = match focusforge_core::params::resolve_in(
                entry.params(),
                &explicit,
                None,
                &facts,
            ) {
                Ok(resolved) => entry
                    .params()
                    .iter()
                    .map(|desc| {
                        let r = &resolved[&desc.id];
                        serde_json::json!({
                            "id": desc.id,
                            "value": r.value,
                            "provenance": r.provenance,
                            "auto": desc.auto.is_some(),
                        })
                    })
                    .collect(),
                Err(error) => vec![serde_json::json!({ "error": error.to_string() })],
            };
            serde_json::json!({
                "id": entry.id(),
                "display_name": entry.display_name(),
                "family": entry.family().to_string(),
                "params": params,
            })
        })
        .collect();

    let input_map = config_dir.map(|dir| dir.join("settings.toml"));
    serde_json::json!({
        "algorithms": algorithms,
        "input_map": {
            "path": input_map.as_ref().map(|p| p.display().to_string()),
            "exists": input_map.as_deref().is_some_and(Path::is_file),
        },
    })
}

/// The build's own version and commit hash, when this bundle was made, and
/// what was left out of it.
fn version(excluded: &[String]) -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "commit": env!("FOCUSFORGE_COMMIT_HASH"),
        "created": rfc3339(focusforge_core::project::seal::now_unix()),
        "excluded": excluded,
    })
}

/// The default file name for a bundle made now: colon-free, so it is legal
/// on every target filesystem.
pub fn default_file_name() -> String {
    format!(
        "focusforge-diagnostics-{}.zip",
        crate::crash::utc_stamp(focusforge_core::project::seal::now_unix())
    )
}

/// Writes exactly `manifest`'s entries to `path`, applying `redaction` to
/// every entry marked redacted as its bytes are produced. Refuses an
/// existing `path` unless `force`, and any `path` without a file name; a
/// failure leaves nothing behind at `path`.
pub fn write(
    manifest: &Manifest,
    path: &Path,
    force: bool,
    redaction: &Redaction,
) -> Result<Written, BundleError> {
    if path.file_name().is_none() || (path.exists() && !force) {
        return Err(BundleError::OutputExists(path.to_path_buf()));
    }
    let partial = focusforge_core::project::container::PartialFile::new(path);
    let file = File::create(partial.path()).map_err(|source| BundleError::Io {
        path: partial.path().to_path_buf(),
        source,
    })?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut sources = Vec::new();

    for entry in &manifest.entries {
        zip.start_file(&entry.name, options)
            .map_err(|source| BundleError::Archive {
                path: partial.path().to_path_buf(),
                source,
            })?;
        match &entry.source {
            EntrySource::Generated(text) => {
                let body = redaction.apply(text);
                zip.write_all(body.as_bytes())
                    .map_err(|source| BundleError::Io {
                        path: partial.path().to_path_buf(),
                        source,
                    })?;
            }
            EntrySource::File(source_path) => {
                if entry.redacted {
                    copy_redacted(source_path, &mut zip, redaction, partial.path())?;
                } else {
                    let mut source = File::open(source_path).map_err(|source| BundleError::Io {
                        path: source_path.clone(),
                        source,
                    })?;
                    std::io::copy(&mut source, &mut zip).map_err(|source| BundleError::Io {
                        path: source_path.clone(),
                        source,
                    })?;
                }
                sources.push(source_path.clone());
            }
        }
    }

    let file = zip.finish().map_err(|source| BundleError::Archive {
        path: partial.path().to_path_buf(),
        source,
    })?;
    file.sync_all().map_err(|source| BundleError::Io {
        path: partial.path().to_path_buf(),
        source,
    })?;
    drop(file);
    let partial_at = partial.path().to_path_buf();
    partial.commit().map_err(|source| BundleError::Io {
        path: partial_at,
        source,
    })?;
    Ok(Written {
        path: path.to_path_buf(),
        sources,
    })
}

/// Reads a text file line by line, redacting each line into the archive, so
/// a log of any size is never held whole in memory. Bytes that are not
/// UTF-8 are replaced rather than failing the bundle.
fn copy_redacted(
    source_path: &Path,
    zip: &mut ZipWriter<File>,
    redaction: &Redaction,
    partial: &Path,
) -> Result<(), BundleError> {
    let file = File::open(source_path).map_err(|source| BundleError::Io {
        path: source_path.to_path_buf(),
        source,
    })?;
    let mut reader = BufReader::new(file);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        let read = reader
            .read_until(b'\n', &mut raw)
            .map_err(|source| BundleError::Io {
                path: source_path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&raw);
        zip.write_all(redaction.apply(&line).as_bytes())
            .map_err(|source| BundleError::Io {
                path: partial.to_path_buf(),
                source,
            })?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn redaction(home: &str, user: &str) -> Redaction {
        Redaction {
            home: Some(PathBuf::from(home)),
            user: Some(user.to_string()),
        }
    }

    /// A data directory holding a live log, a rotated log numbered above
    /// it, a panic report with a sibling dump, a native crash metadata and
    /// dump pair, and files no bundle may collect.
    fn seeded_data_dir(root: &Path) -> PathBuf {
        let data = root.join("focusforge");
        std::fs::create_dir_all(data.join("logs")).unwrap();
        std::fs::create_dir_all(data.join("crashes")).unwrap();
        std::fs::write(
            data.join("logs/focusforge.jsonl"),
            "{\"message\":\"live\"}\n",
        )
        .unwrap();
        std::fs::write(
            data.join("logs/focusforge.2.jsonl"),
            "{\"message\":\"rotated\"}\n",
        )
        .unwrap();
        std::fs::write(data.join("logs/notes.txt"), "stray").unwrap();
        std::fs::write(data.join("logs/preview.tif"), b"II*\0").unwrap();
        std::fs::write(
            data.join("crashes/20260918T010203Z-focusforge-cli-7.json"),
            "{\"format\":1,\"payload\":\"boom\"}",
        )
        .unwrap();
        std::fs::write(
            data.join("crashes/20260918T010203Z-focusforge-cli-7.dmp"),
            b"MDMP",
        )
        .unwrap();
        std::fs::write(
            data.join("crashes/20260918T010204Z-focusforge-cli-8.json"),
            "{\"format\":1,\"minidump\":\"20260918T010204Z-focusforge-cli-8.dmp\"}",
        )
        .unwrap();
        std::fs::write(
            data.join("crashes/20260918T010204Z-focusforge-cli-8.dmp"),
            b"MDMP",
        )
        .unwrap();
        std::fs::write(data.join("crashes/truncated.json"), "{\"format\":1,").unwrap();
        std::fs::write(data.join("crashes/frame.tif"), b"II*\0").unwrap();
        std::fs::write(data.join("session.ffproj"), b"PK\x03\x04").unwrap();
        std::fs::write(data.join("stray.log"), "stray").unwrap();
        data
    }

    fn entry_names(manifest: &Manifest) -> Vec<String> {
        manifest.entries.iter().map(|e| e.name.clone()).collect()
    }

    fn archive_names(path: &Path) -> Vec<String> {
        let file = File::open(path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().unwrap().to_string())
            .collect()
    }

    fn archive_text(path: &Path, name: &str) -> String {
        let file = File::open(path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut entry = archive.by_name(name).unwrap();
        let mut text = String::new();
        entry.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn the_home_path_is_redacted_before_the_user_name() {
        let redaction = redaction("/home/ann", "ann");
        assert_eq!(
            redaction.apply("opened /home/ann/pictures for ann"),
            "opened [home]/pictures for [user]"
        );
    }

    #[test]
    fn a_user_name_inside_a_longer_word_is_redacted_too() {
        assert_eq!(
            redaction("/home/ann", "ann").apply("annotation"),
            "[user]otation"
        );
    }

    #[test]
    fn the_allowlist_collects_logs_and_crash_json_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let data = seeded_data_dir(root.path());
        let manifest = manifest(&Options {
            data_dir: Some(data),
            ..Options::default()
        });

        assert_eq!(
            entry_names(&manifest),
            vec![
                "adapter.json",
                "configuration.json",
                "system.json",
                "version.json",
                "crashes/20260918T010203Z-focusforge-cli-7.json",
                "crashes/20260918T010204Z-focusforge-cli-8.json",
                "crashes/truncated.json",
                "logs/focusforge.2.jsonl",
                "logs/focusforge.jsonl",
            ]
        );
    }

    #[test]
    fn a_crash_report_is_classified_by_its_content_not_by_a_sibling_dump() {
        let root = tempfile::tempdir().unwrap();
        let data = seeded_data_dir(root.path());
        let manifest = manifest(&Options {
            data_dir: Some(data),
            ..Options::default()
        });
        let kind = |name: &str| {
            manifest
                .entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("{name} must be collected"))
                .kind
        };

        // Both have a sibling `.dmp`; only the one naming it is metadata.
        assert_eq!(
            kind("crashes/20260918T010203Z-focusforge-cli-7.json"),
            EntryKind::PanicReport
        );
        assert_eq!(
            kind("crashes/20260918T010204Z-focusforge-cli-8.json"),
            EntryKind::NativeCrashMetadata
        );
        assert_eq!(kind("crashes/truncated.json"), EntryKind::PanicReport);
    }

    #[test]
    fn a_minidump_is_excluded_by_default_and_included_on_request() {
        let root = tempfile::tempdir().unwrap();
        let data = seeded_data_dir(root.path());

        let default = manifest(&Options {
            data_dir: Some(data.clone()),
            ..Options::default()
        });
        assert_eq!(
            default.excluded,
            vec![
                "crashes/20260918T010203Z-focusforge-cli-7.dmp",
                "crashes/20260918T010204Z-focusforge-cli-8.dmp",
            ]
        );
        assert!(!entry_names(&default).iter().any(|n| n.ends_with(".dmp")));

        let opted_in = manifest(&Options {
            data_dir: Some(data),
            include_minidumps: true,
            ..Options::default()
        });
        assert!(opted_in.excluded.is_empty());
        let dumps: Vec<&Entry> = opted_in
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::Minidump)
            .collect();
        assert_eq!(dumps.len(), 2);
        assert!(dumps.iter().all(|e| !e.redacted));
        assert!(
            opted_in
                .lines()
                .iter()
                .filter(|l| l.contains(".dmp"))
                .all(|l| l.contains("not redacted"))
        );
    }

    #[test]
    fn the_archive_holds_exactly_the_manifest_entries_and_names_its_sources() {
        let root = tempfile::tempdir().unwrap();
        let data = seeded_data_dir(root.path());
        let manifest = manifest(&Options {
            data_dir: Some(data.clone()),
            include_minidumps: true,
            ..Options::default()
        });
        let out = root.path().join("bundle.zip");

        let written = write(&manifest, &out, false, &Redaction::default()).unwrap();

        assert_eq!(archive_names(&out), entry_names(&manifest));
        assert_eq!(written.path, out);
        assert!(
            written
                .sources
                .contains(&data.join("logs/focusforge.jsonl"))
        );
        assert!(
            written
                .sources
                .contains(&data.join("crashes/20260918T010203Z-focusforge-cli-7.dmp"))
        );
        assert_eq!(written.sources.len(), 7);
        assert!(!out.with_file_name("bundle.zip.partial").exists());
    }

    #[test]
    fn every_text_entry_is_redacted_on_the_way_in() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("focusforge");
        std::fs::create_dir_all(data.join("logs")).unwrap();
        std::fs::write(
            data.join("logs/focusforge.jsonl"),
            "{\"message\":\"read /home/ann/frames\",\"user\":\"ann\"}\n",
        )
        .unwrap();
        let manifest = manifest(&Options {
            data_dir: Some(data),
            ..Options::default()
        });
        let out = root.path().join("bundle.zip");

        write(&manifest, &out, false, &redaction("/home/ann", "ann")).unwrap();

        let log = archive_text(&out, "logs/focusforge.jsonl");
        assert_eq!(
            log,
            "{\"message\":\"read [home]/frames\",\"user\":\"[user]\"}\n"
        );
    }

    #[test]
    fn an_existing_output_is_refused_without_force_and_overwritten_with_it() {
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("bundle.zip");
        std::fs::write(&out, "not a bundle").unwrap();
        let manifest = manifest(&Options::default());

        let refused = write(&manifest, &out, false, &Redaction::default());

        assert!(matches!(refused, Err(BundleError::OutputExists(_))));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "not a bundle");
        assert!(!out.with_file_name("bundle.zip.partial").exists());

        write(&manifest, &out, true, &Redaction::default()).unwrap();
        assert_eq!(archive_names(&out).len(), 4);
    }

    #[test]
    fn an_output_path_without_a_file_name_is_refused_and_touches_no_partial() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("data")).unwrap();
        let unrelated = root.path().join(".partial");
        std::fs::write(&unrelated, "unrelated").unwrap();
        let manifest = manifest(&Options::default());

        let refused = write(
            &manifest,
            &root.path().join("data").join(".."),
            true,
            &Redaction::default(),
        );

        assert!(matches!(refused, Err(BundleError::OutputExists(_))));
        assert_eq!(std::fs::read_to_string(&unrelated).unwrap(), "unrelated");
    }

    #[test]
    fn a_missing_data_directory_still_yields_the_generated_entries() {
        let root = tempfile::tempdir().unwrap();
        let manifest = manifest(&Options {
            data_dir: Some(root.path().join("absent")),
            ..Options::default()
        });

        assert_eq!(
            entry_names(&manifest),
            vec![
                "adapter.json",
                "configuration.json",
                "system.json",
                "version.json"
            ]
        );
        assert!(!manifest.has_logs());
        assert!(!manifest.has_crash_evidence());
    }

    #[test]
    fn the_adapter_entry_says_so_when_the_logs_hold_no_record() {
        let root = tempfile::tempdir().unwrap();
        let data = seeded_data_dir(root.path());
        let manifest = manifest(&Options {
            data_dir: Some(data),
            ..Options::default()
        });
        let out = root.path().join("bundle.zip");
        write(&manifest, &out, false, &Redaction::default()).unwrap();

        let adapter: serde_json::Value =
            serde_json::from_str(&archive_text(&out, "adapter.json")).unwrap();
        assert!(adapter["selected"].is_null());
        assert!(adapter["note"].as_str().unwrap().contains("no graphics"));
    }

    #[test]
    fn the_adapter_entry_carries_the_last_record_the_logs_hold() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("focusforge");
        std::fs::create_dir_all(data.join("logs")).unwrap();
        std::fs::write(
            data.join("logs/focusforge.jsonl"),
            "{\"message\":\"graphics adapter selected\",\"adapter\":\"first\"}\n\
             {\"message\":\"graphics adapter selected\",\"adapter\":\"second\"}\n\
             {\"message\":\"graphics adapter and device limits\",\"device_limits\":\"...\"}\n",
        )
        .unwrap();
        let manifest = manifest(&Options {
            data_dir: Some(data),
            ..Options::default()
        });
        let out = root.path().join("bundle.zip");
        write(&manifest, &out, false, &Redaction::default()).unwrap();

        let adapter: serde_json::Value =
            serde_json::from_str(&archive_text(&out, "adapter.json")).unwrap();
        assert_eq!(adapter["selected"]["adapter"], "second");
        assert_eq!(adapter["limits"]["device_limits"], "...");
        assert!(adapter.get("note").is_none());
    }

    #[test]
    fn the_version_entry_carries_the_package_version_and_a_commit_hash() {
        let root = tempfile::tempdir().unwrap();
        let manifest = manifest(&Options::default());
        let out = root.path().join("bundle.zip");
        write(&manifest, &out, false, &Redaction::default()).unwrap();

        let version: serde_json::Value =
            serde_json::from_str(&archive_text(&out, "version.json")).unwrap();
        assert_eq!(version["version"], env!("CARGO_PKG_VERSION"));
        assert!(!version["commit"].as_str().unwrap().is_empty());
        assert!(version["created"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn the_configuration_entry_names_the_input_map_and_whether_it_exists() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let out = root.path().join("bundle.zip");

        let absent = manifest(&Options {
            config_dir: Some(config.clone()),
            ..Options::default()
        });
        write(&absent, &out, true, &Redaction::default()).unwrap();
        let entry: serde_json::Value =
            serde_json::from_str(&archive_text(&out, "configuration.json")).unwrap();
        assert_eq!(entry["input_map"]["exists"], false);
        assert!(
            entry["input_map"]["path"]
                .as_str()
                .unwrap()
                .ends_with("settings.toml")
        );

        std::fs::write(config.join("settings.toml"), "").unwrap();
        let present = manifest(&Options {
            config_dir: Some(config),
            ..Options::default()
        });
        write(&present, &out, true, &Redaction::default()).unwrap();
        let entry: serde_json::Value =
            serde_json::from_str(&archive_text(&out, "configuration.json")).unwrap();
        assert_eq!(entry["input_map"]["exists"], true);
    }

    #[test]
    fn the_system_entry_carries_this_host_and_never_its_name() {
        let root = tempfile::tempdir().unwrap();
        let manifest = manifest(&Options::default());
        let out = root.path().join("bundle.zip");
        write(&manifest, &out, false, &Redaction::default()).unwrap();

        let system: serde_json::Value =
            serde_json::from_str(&archive_text(&out, "system.json")).unwrap();
        assert!(system["total_memory"].as_u64().unwrap() > 0);
        assert!(system["logical_cores"].as_u64().unwrap() >= 1);
        assert_eq!(system["facts_origin"], "Probed");
        assert_eq!(system["os"], std::env::consts::OS);
        assert!(system.get("host_name").is_none());
    }

    #[test]
    fn the_default_file_name_carries_no_colon() {
        let name = default_file_name();
        assert!(name.starts_with("focusforge-diagnostics-"), "{name}");
        assert!(name.ends_with(".zip"), "{name}");
        assert!(!name.contains(':'), "{name}");
    }
}
