// SPDX-License-Identifier: GPL-3.0-or-later

//! The `Result<Adapter, RequestAdapterError> -> path` mapping, deliberately
//! `wgpu`-free so it stays testable in the default, GPU-free gate even
//! though `wgpu` is an optional dependency of this crate
//! (`crates/gpu/Cargo.toml`). Only [`std::fmt::Display`] is ever used on the
//! error, which is what keeps this module free of any `wgpu` type; `mod
//! device` instantiates [`classify_adapter`] at `A = wgpu::Adapter`,
//! `E = wgpu::RequestAdapterError` once the `gpu` feature is on.

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
}
