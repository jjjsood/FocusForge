// SPDX-License-Identifier: GPL-3.0-or-later

//! What every thread pool this crate builds does with a panic rayon cannot
//! hand back to a caller.

use std::any::Any;

/// Records a panic on a pool worker. Without a handler, such a panic --
/// one raised in work rayon cannot propagate, as `spawn` cannot -- aborts
/// the process with nothing said about it.
pub(crate) fn log_worker_panic(payload: Box<dyn Any + Send>) {
    tracing::error!(
        target: "focusforge_core",
        payload = %payload_text(&*payload),
        "a worker thread panicked"
    );
}

/// A panic payload as text: the `&str` and `String` forms a `panic!`
/// produces, and a fixed stand-in for anything else.
fn payload_text(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_text_reads_both_panic_payload_shapes() {
        assert_eq!(payload_text(&"literal message"), "literal message");
        assert_eq!(payload_text(&format!("formatted {}", 7)), "formatted 7");
        assert_eq!(payload_text(&7_u32), "unknown panic payload");
    }
}
