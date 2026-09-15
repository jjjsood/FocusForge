// SPDX-License-Identifier: GPL-3.0-or-later

//! Maps a `request_adapter` result to a path choice. Generic over the
//! adapter and error types (only `Display` is read), so it compiles and is
//! tested without the `gpu` feature.

use std::fmt;

/// The two things a `request_adapter` call can produce, generic over the
/// concrete adapter and error types so this module never names `wgpu`.
#[derive(Debug)]
pub enum AdapterPath<A> {
    Present(A),
    Absent { reason: String },
}

/// Classifies a `request_adapter` result without ever naming its `wgpu`
/// types: only `E`'s [`Display`](fmt::Display) is read, into `Absent`'s
/// owned `reason`.
pub fn classify_adapter<A, E: fmt::Display>(result: Result<A, E>) -> AdapterPath<A> {
    match result {
        Ok(adapter) => AdapterPath::Present(adapter),
        Err(err) => AdapterPath::Absent {
            reason: err.to_string(),
        },
    }
}

/// The message a parity test prints when it skips for lack of a usable
/// adapter -- visible in test output rather than indistinguishable from a
/// pass.
pub fn skip_message(reason: &str) -> String {
    format!("SKIP: no usable GPU adapter -- {reason}")
}

/// Which path a `stack`/`align` run takes: the GPU implementation, or the
/// CPU one. GPU is not opt-in -- a usable adapter is
/// used automatically, and `--no-gpu` is the only way to force the CPU path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathChoice {
    Gpu,
    CpuFallback,
}

/// Maps a `--no-gpu` flag and a `request_adapter`-shaped result to a path
/// choice, the adapter (kept on the `Gpu` arm) and one line naming what
/// happened. `--no-gpu` wins without looking at `result`.
pub fn choose_path<A: fmt::Display, E: fmt::Display>(
    forced_cpu: bool,
    result: Result<A, E>,
) -> (PathChoice, Result<A, String>, String) {
    if forced_cpu {
        let message = "--no-gpu given: forcing the CPU path".to_string();
        return (PathChoice::CpuFallback, Err(message.clone()), message);
    }
    match classify_adapter(result) {
        AdapterPath::Present(value) => {
            let message = format!("adapter selected: {value}; using the GPU path");
            (PathChoice::Gpu, Ok(value), message)
        }
        AdapterPath::Absent { reason } => {
            // Reason-led, because `GpuError::VramExhausted` (an adapter that
            // cannot serve the minimum budget) lands here too. The message
            // always contains the reason and "CPU path".
            let message = format!("{reason}; continuing on the CPU path");
            (PathChoice::CpuFallback, Err(reason), message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    #[test]
    fn ok_result_classifies_as_present() {
        let result: Result<u32, FakeError> = Ok(42);

        match classify_adapter(result) {
            AdapterPath::Present(value) => assert_eq!(value, 42),
            AdapterPath::Absent { reason } => panic!("expected Present, got Absent({reason})"),
        }
    }

    #[test]
    fn err_result_classifies_as_absent_quoting_display() {
        let result: Result<u32, FakeError> = Err(FakeError("no backend available"));

        match classify_adapter(result) {
            AdapterPath::Present(value) => panic!("expected Absent, got Present({value})"),
            AdapterPath::Absent { reason } => assert_eq!(reason, "no backend available"),
        }
    }

    #[test]
    fn skip_message_names_starts_with_skip_and_quotes_reason() {
        let message = skip_message("no backend available");

        assert!(message.starts_with("SKIP:"));
        assert!(message.contains("no backend available"));
    }

    #[test]
    fn choose_path_on_a_synthetic_err_names_the_reason_and_the_cpu_fallback() {
        let result: Result<u32, FakeError> = Err(FakeError("no backend available"));

        let (path, value, message) = choose_path(false, result);

        assert_eq!(path, PathChoice::CpuFallback);
        assert_eq!(value, Err("no backend available".to_string()));
        assert!(
            message.contains("no backend available"),
            "message must name what was missing: {message}"
        );
        assert!(
            message.to_lowercase().contains("cpu"),
            "message must name the CPU fallback: {message}"
        );
    }

    #[test]
    fn choose_path_on_a_synthetic_ok_names_the_adapter_and_takes_the_gpu_path() {
        let result: Result<u32, FakeError> = Ok(42);

        let (path, value, message) = choose_path(false, result);

        assert_eq!(path, PathChoice::Gpu);
        assert_eq!(value, Ok(42));
        assert!(
            message.contains("42"),
            "message must name the adapter: {message}"
        );
    }

    /// `GpuError::VramExhausted`'s message as a `FakeError`: the VRAM-floor
    /// refusal falls back to the CPU like a missing adapter.
    #[test]
    fn choose_path_on_a_vram_exhausted_shaped_err_falls_back_to_cpu_naming_the_reason() {
        let result: Result<u32, FakeError> = Err(FakeError(
            "device cannot serve even the minimum VRAM budget of 268435456 bytes: refused",
        ));

        let (path, value, message) = choose_path(false, result);

        assert_eq!(path, PathChoice::CpuFallback);
        assert!(value.is_err());
        assert!(
            message.contains("minimum VRAM budget"),
            "message must name the reason: {message}"
        );
        assert!(
            message.to_lowercase().contains("cpu path"),
            "message must name the CPU fallback: {message}"
        );
    }

    #[test]
    fn choose_path_with_forced_cpu_ignores_a_present_adapter() {
        let result: Result<u32, FakeError> = Ok(42);

        let (path, value, message) = choose_path(true, result);

        assert_eq!(path, PathChoice::CpuFallback);
        assert!(value.is_err());
        assert!(message.contains("--no-gpu"));
    }
}
