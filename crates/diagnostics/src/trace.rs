// SPDX-License-Identifier: GPL-3.0-or-later

//! The profiling layers: a Chrome-trace writer and a Tracy client, both
//! reading the spans the subscriber already carries.

use std::path::PathBuf;

use tracing::Subscriber;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

use crate::WORKSPACE_TARGETS;

/// The name of the file a Chrome trace is written to. Unset, no trace is
/// written and no profiling output path is opened at all.
const TRACE_FILE_VAR: &str = "FOCUSFORGE_TRACE_FILE";

/// A layer that fits either profiler's slot in the subscriber's chain.
type BoxedLayer<S> = Box<dyn tracing_subscriber::Layer<S> + Send + Sync + 'static>;

/// Both profiling layers' per-target filter: this workspace's own targets
/// down to DEBUG, everything else off. An unfiltered layer would raise the
/// registry's global maximum level to TRACE, turning every `wgpu` and
/// `naga` record arriving through the `log` bridge into a trace event and a
/// profiler message.
fn trace_targets() -> Targets {
    Targets::new()
        .with_default(LevelFilter::OFF)
        .with_targets(WORKSPACE_TARGETS.map(|t| (t, LevelFilter::DEBUG)))
}

/// What [`chrome`] resolved from the environment.
pub(crate) struct Chrome<S> {
    /// The Chrome-trace layer, or `None` when no output file was named or
    /// the named one could not be opened.
    pub(crate) layer: Option<BoxedLayer<S>>,
    /// Dropping this flushes the writer's buffer and closes the trace's
    /// JSON array; without the drop the file stays an unterminated array.
    pub(crate) guard: Option<tracing_chrome::FlushGuard>,
    /// The output path that could not be opened, with the reason.
    pub(crate) refused: Option<(PathBuf, std::io::Error)>,
}

/// The Chrome-trace layer for the file `FOCUSFORGE_TRACE_FILE` names, and
/// its flush guard. The file is opened here rather than through the
/// builder's own path argument, which panics when the file cannot be
/// created; a refused path is reported and the run continues untraced.
pub(crate) fn chrome<S>() -> Chrome<S>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    let Some(path) = std::env::var_os(TRACE_FILE_VAR) else {
        return Chrome {
            layer: None,
            guard: None,
            refused: None,
        };
    };
    let path = PathBuf::from(path);
    match std::fs::File::create(&path) {
        Ok(file) => {
            // Span fields reach the trace's `args` only with this set: no
            // stage or tile entry is distinguishable without it.
            let (layer, guard) = tracing_chrome::ChromeLayerBuilder::new()
                .writer(file)
                .include_args(true)
                .build();
            Chrome {
                layer: Some(layer.with_filter(trace_targets()).boxed()),
                guard: Some(guard),
                refused: None,
            }
        }
        Err(error) => Chrome {
            layer: None,
            guard: None,
            refused: Some((path, error)),
        },
    }
}

/// The Tracy layer. Constructing it starts the Tracy client, which in this
/// build's configuration listens on localhost and collects nothing until a
/// profiler connects.
pub(crate) fn tracy<S>() -> BoxedLayer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    tracing_tracy::TracyLayer::default()
        .with_filter(trace_targets())
        .boxed()
}

#[cfg(test)]
mod tests {
    use tracing::Level;
    use tracing_subscriber::filter::Targets;

    use super::trace_targets;
    use crate::{MIRRORED_OUTPUT_TARGET, file_targets};

    fn enabled(filter: &Targets, target: &str, level: Level) -> bool {
        filter.would_enable(target, &level)
    }

    #[test]
    fn trace_filter_refuses_a_third_party_target_the_file_filter_accepts() {
        assert!(!enabled(
            &trace_targets(),
            "some_third_party_crate",
            Level::ERROR
        ));
        assert!(enabled(
            &file_targets(),
            "some_third_party_crate",
            Level::ERROR
        ));
    }

    #[test]
    fn trace_filter_accepts_a_workspace_target_down_to_debug() {
        let filter = trace_targets();
        assert!(enabled(&filter, "focusforge_core", Level::DEBUG));
        assert!(enabled(&filter, "focusforge_cli", Level::DEBUG));
        assert!(!enabled(&filter, "focusforge_core", Level::TRACE));
    }

    #[test]
    fn trace_filter_refuses_the_mirrored_output_target() {
        assert!(!enabled(
            &trace_targets(),
            MIRRORED_OUTPUT_TARGET,
            Level::DEBUG
        ));
        assert!(enabled(
            &file_targets(),
            MIRRORED_OUTPUT_TARGET,
            Level::DEBUG
        ));
    }
}
