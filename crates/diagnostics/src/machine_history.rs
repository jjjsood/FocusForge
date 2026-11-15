// SPDX-License-Identifier: GPL-3.0-or-later

//! The per-user file the duration estimate learns into, shared by
//! `focusforge-cli` and `focusforge-gui`.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use focusforge_core::gpu::TileBackend;
use focusforge_core::job::machine::{MachineHistory, MachineKey, MachineTerm, RunOutcome};
use focusforge_core::job::prediction::{RawSource, predict_run};
use focusforge_core::job::{ResumeError, write_atomic};
use focusforge_core::pipeline::{FuseReport, RunPlanFeatures};

/// The file inside the data directory. Deleting it resets what the estimate
/// learned.
pub const FILE_NAME: &str = "machine-history.json";

/// Why the history could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// The path exists but cannot be read: a directory, no permission.
    #[error("{}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Write(#[from] ResumeError),
}

/// The history in `data_dir`. A missing, empty or unparsable file, or one of
/// another revision, is the empty history; any other read error is refused so
/// the caller can leave the path alone.
pub fn load(data_dir: &Path) -> Result<MachineHistory, HistoryError> {
    let path = data_dir.join(FILE_NAME);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(MachineHistory::parse(&bytes)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(MachineHistory::default()),
        Err(source) => Err(HistoryError::Read { path, source }),
    }
}

/// Writes `history` to `data_dir` through a temp file and a rename, creating
/// the directory. An existing path that cannot be read is refused and left
/// as it is.
pub fn save(data_dir: &Path, history: &MachineHistory) -> Result<(), HistoryError> {
    load(data_dir)?;
    std::fs::create_dir_all(data_dir).map_err(|source| ResumeError::Io {
        path: data_dir.to_path_buf(),
        source,
    })?;
    Ok(write_atomic(&data_dir.join(FILE_NAME), history)?)
}

/// The history a run starts from: `None` without a directory or when the file
/// cannot be read, so the run estimates without it and the path is left alone.
pub fn load_for_run(data_dir: Option<&Path>) -> Option<MachineHistory> {
    match load(data_dir?) {
        Ok(history) => Some(history),
        Err(err) => {
            tracing::warn!("duration history not used: {err}");
            None
        }
    }
}

/// Teaches the history in `data_dir` what a completed run measured against
/// the shipped seconds for its plan. `first_frame` names the RAW source
/// extension; the adapter recorded is the fuse `backend`'s, else
/// `align_adapter` when the align accelerator ran. The file is reloaded
/// first; a history that cannot be read or written never fails the run.
pub fn learn(
    data_dir: Option<&Path>,
    report: &FuseReport,
    aligned: bool,
    first_frame: Option<&Path>,
    backend: Option<&dyn TileBackend>,
    align_adapter: Option<&str>,
) {
    let (Some(dir), Some(run_plan), Some(key)) =
        (data_dir, report.run_plan.as_ref(), MachineKey::current())
    else {
        return;
    };
    let features = RunPlanFeatures {
        align_cache_hit: report.align_cached,
        resumed_units: report.resumed_units,
        ..run_plan.clone()
    };
    let raw = report
        .developed_store
        .zip(first_frame.and_then(|p| p.extension()?.to_str()))
        .map(|(store, extension)| RawSource { extension, store });
    let Some(shipped) = predict_run(&features, aligned, raw, &MachineTerm::IDENTITY) else {
        return;
    };
    let mut history = match load(dir) {
        Ok(history) => history,
        Err(err) => {
            tracing::warn!("duration history not updated: {err}");
            return;
        }
    };
    let adapter = match backend {
        Some(backend) => Some(backend.info().to_string()),
        None if report.align_backend_used => align_adapter.map(str::to_string),
        None => None,
    };
    history.observe(
        &key,
        &features,
        raw,
        &shipped.stages,
        &report.stages,
        &RunOutcome {
            resumed_units: report.resumed_units,
            paused: report.paused,
            backed_off: report.backed_off,
            gpu_fallback: report.gpu_fallback.is_some(),
            align_cached: report.align_cached,
            adapter: adapter.as_deref(),
        },
    );
    if let Err(err) = save(dir, &history) {
        tracing::warn!("duration history not updated: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use focusforge_core::io::ReaderKind;
    use focusforge_core::job::machine::{MachineKey, RunOutcome};
    use focusforge_core::pipeline::RunPlanFeatures;
    use focusforge_core::timing::StageTimings;

    fn learned() -> MachineHistory {
        let features = RunPlanFeatures {
            frame_count: 30,
            frame_pixels: 15_000_000,
            source_bits_per_sample: 16,
            reader: ReaderKind::Mmap,
            gpu: false,
            align_gpu: false,
            workers: 8,
            algorithm: "laplacian".to_string(),
            align_cache_hit: false,
            resumed_units: 0,
        };
        let shipped = StageTimings {
            setup: Some(1.0),
            develop: None,
            align: Some(10.0),
            fuse: Some(20.0),
            warp_write: None,
            finalize: Some(5.0),
        };
        let actual = StageTimings {
            setup: Some(0.5),
            align: Some(5.0),
            fuse: Some(10.0),
            finalize: Some(5.0),
            ..shipped
        };
        let mut history = MachineHistory::default();
        let key = MachineKey::new("Test CPU", 8).expect("brand and cores");
        history.observe(
            &key,
            &features,
            None,
            &shipped,
            &actual,
            &RunOutcome::default(),
        );
        history
    }

    fn file(dir: &Path) -> PathBuf {
        dir.join(FILE_NAME)
    }

    #[test]
    fn a_missing_or_empty_file_gives_the_empty_history() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), MachineHistory::default());
        std::fs::write(file(dir.path()), b"").unwrap();
        assert_eq!(load(dir.path()).unwrap(), MachineHistory::default());
    }

    #[test]
    fn garbage_and_another_revision_give_the_empty_history_and_the_next_save_parses() {
        for bytes in [
            &b"\x00not json{"[..],
            &br#"{"revision":999,"machines":[]}"#[..],
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(file(dir.path()), bytes).unwrap();
            assert_eq!(load(dir.path()).unwrap(), MachineHistory::default());
            let history = learned();
            save(dir.path(), &history).unwrap();
            assert_eq!(load(dir.path()).unwrap(), history);
        }
    }

    #[test]
    fn save_creates_the_directory_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        let history = learned();
        save(&nested, &history).unwrap();
        save(&nested, &history).unwrap();
        let names: Vec<_> = std::fs::read_dir(&nested)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from(FILE_NAME)]);
        assert_eq!(load(&nested).unwrap(), history);
    }

    #[test]
    fn a_directory_in_the_files_place_is_an_error_and_stays_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(file(dir.path())).unwrap();
        assert!(matches!(load(dir.path()), Err(HistoryError::Read { .. })));
        assert!(save(dir.path(), &learned()).is_err());
        assert!(file(dir.path()).is_dir());
        assert_eq!(std::fs::read_dir(file(dir.path())).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temp file"
        );
    }
}
