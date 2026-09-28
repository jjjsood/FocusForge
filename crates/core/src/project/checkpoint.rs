// SPDX-License-Identifier: GPL-3.0-or-later

//! `checkpoints/<id>.json`: one node of a checkpoint chain, plus all four
//! [`Op`] variants fully defined and round-tripped here. A freshly stacked
//! project's own checkpoint is parentless with an empty `ops`; a checkpoint
//! `super::append::append_ops` seals carries both.
//!
//! The extension rule runs the opposite way from [`super::manifest`]:
//! **every op field carries `#[serde(default)]`**, so a new field on a
//! variant does not make an existing project unreadable, while the
//! manifest's own commitment fields deliberately refuse rather than
//! default.

use serde::{Deserialize, Serialize};

use crate::params::ParamValues;
use crate::tile::Rect;

/// One node of a checkpoint chain: `parent: None` and an empty `ops` for
/// the checkpoint a fresh stack or a re-fuse writes; a checkpoint
/// `super::append::append_ops` seals carries both instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    /// `None` for the checkpoint a fresh stack writes.
    pub parent: Option<String>,
    /// Empty for the checkpoint a fresh stack writes.
    pub ops: Vec<Op>,
    /// The hash of the Z-Map blob this checkpoint's state is.
    pub zmap_blob: String,
    pub stack_fingerprint: String,
    pub algorithm: String,
    pub params: ParamValues,
    /// Unix seconds, UTC.
    pub created_at: i64,
    pub label: Option<String>,
}

impl Checkpoint {
    /// `blake3(stack_fingerprint ‖ zmap_blob ‖ created_at)`, hex-truncated
    /// to 32 characters -- deterministic, so two seals of the same stack at
    /// the same instant name the same checkpoint file, and no `uuid`
    /// dependency is needed to get a collision-free name.
    pub fn derive_id(stack_fingerprint: &str, zmap_blob: &str, created_at: i64) -> String {
        Self::derive(None, stack_fingerprint, zmap_blob, created_at)
    }

    /// `blake3(parent ‖ stack_fingerprint ‖ zmap_blob ‖ created_at)`,
    /// hex-truncated to 32 characters -- the parent is folded into the hash
    /// so appending onto unchanged sources within the same wall-clock second
    /// still derives an id distinct from the checkpoint it is parented on.
    pub fn derive_child_id(
        parent: &str,
        stack_fingerprint: &str,
        zmap_blob: &str,
        created_at: i64,
    ) -> String {
        Self::derive(Some(parent), stack_fingerprint, zmap_blob, created_at)
    }

    fn derive(
        parent: Option<&str>,
        stack_fingerprint: &str,
        zmap_blob: &str,
        created_at: i64,
    ) -> String {
        let mut hasher = blake3::Hasher::new();
        if let Some(parent) = parent {
            hasher.update(parent.as_bytes());
        }
        hasher.update(stack_fingerprint.as_bytes());
        hasher.update(zmap_blob.as_bytes());
        hasher.update(&created_at.to_le_bytes());
        hasher.finalize().to_hex()[..32].to_string()
    }
}

/// A point in **image pixel coordinates**. `i32` rather than `u32` so a
/// stroke may legitimately leave the canvas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// One edit in a checkpoint's op log. **Externally tagged** -- the serde
/// default, stated here because it is frozen: an untagged enum would make
/// deserialization buffered and slow (serde-rs/serde#2101), and `Refuse`
/// carries a [`ParamValues`] that would be caught by exactly that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Op {
    /// Paint `source_id` along `path`. The Z-Map is the document: this
    /// writes indices, never pixels.
    BrushStroke {
        #[serde(default)]
        source_id: u16,
        #[serde(default)]
        path: Vec<Point>,
        #[serde(default)]
        radius: f32,
        #[serde(default)]
        softness: f32,
    },
    /// Flood `source_id` outward from `seed` while the map stays within
    /// `tolerance`.
    FloodFill {
        #[serde(default)]
        seed: Point,
        #[serde(default)]
        source_id: u16,
        #[serde(default)]
        tolerance: f32,
    },
    /// Put `rect` back to what fusion computed.
    ResetRegion {
        /// `crate::tile::Rect` deliberately derives no `Default` (an empty
        /// rect is not a meaningful geometry anywhere else in the
        /// workspace), so the op-field default is spelled here instead.
        #[serde(default = "empty_rect")]
        rect: Rect,
    },
    /// Re-fuse `rect`-free: run `algorithm` with `params` again over this
    /// checkpoint's state.
    Refuse {
        #[serde(default)]
        algorithm: String,
        #[serde(default)]
        params: ParamValues,
    },
}

/// The zero rect, for [`Op::ResetRegion`]'s `#[serde(default)]`.
fn empty_rect() -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_id_is_deterministic_and_32_chars() {
        let a = Checkpoint::derive_id("fp", "blob", 42);
        assert_eq!(a, Checkpoint::derive_id("fp", "blob", 42));
        assert_ne!(a, Checkpoint::derive_id("fp", "blob", 43));
        assert_ne!(a, Checkpoint::derive_id("fp2", "blob", 42));
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn derive_child_id_differs_from_derive_id_and_per_parent() {
        let plain = Checkpoint::derive_id("fp", "blob", 42);
        let child_a = Checkpoint::derive_child_id("parent-a", "fp", "blob", 42);
        let child_b = Checkpoint::derive_child_id("parent-b", "fp", "blob", 42);
        assert_ne!(child_a, plain);
        assert_ne!(child_a, child_b);
        assert_eq!(child_a.len(), 32);
    }

    #[test]
    fn an_op_with_no_fields_still_deserializes() {
        let op: Op = serde_json::from_str(r#"{"brush-stroke":{}}"#).unwrap();
        assert_eq!(
            op,
            Op::BrushStroke {
                source_id: 0,
                path: Vec::new(),
                radius: 0.0,
                softness: 0.0,
            }
        );
    }

    #[test]
    fn a_parentless_op_less_checkpoint_round_trips() {
        let checkpoint = Checkpoint {
            id: "abc".to_string(),
            parent: None,
            ops: Vec::new(),
            zmap_blob: "deadbeef".to_string(),
            stack_fingerprint: "cafe".to_string(),
            algorithm: "laplacian".to_string(),
            params: ParamValues::new(),
            created_at: 1,
            label: None,
        };
        let text = serde_json::to_string(&checkpoint).unwrap();
        assert_eq!(
            serde_json::from_str::<Checkpoint>(&text).unwrap(),
            checkpoint
        );
    }
}
