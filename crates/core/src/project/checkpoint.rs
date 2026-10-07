// SPDX-License-Identifier: GPL-3.0-or-later

//! `checkpoints/<id>.json`: one node of a checkpoint tree, plus all four
//! [`Op`] variants fully defined and round-tripped here. A freshly stacked
//! project's own checkpoint is parentless with an empty `ops`; a checkpoint
//! `super::append::append_ops` seals carries both. [`History`] is every
//! checkpoint of a project indexed by id, answering the questions the rest
//! of the crate asks of the tree: enumerate, resolve, walk ancestors, find
//! children and descendants.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::ProjectError;
use super::manifest::{AlignRecord, Metrics, PlaneRef, SourceRef};
use crate::align::Registration;
use crate::params::ParamValues;

/// One node of a checkpoint tree: `parent: None` and an empty `ops` for
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
    /// The composite, alignment, crop, sources and metrics this checkpoint's
    /// own fuse produced -- `Some` on the checkpoint a seal root or a
    /// re-fuse writes, `null` on one `append_ops` seals. Open refuses an
    /// op-less checkpoint without it.
    pub fuse: Option<FuseRecord>,
    /// Set by a delete: the checkpoint no longer shows in a listing, but
    /// stays stored while a visible checkpoint is built on it.
    pub hidden: bool,
}

/// One fused checkpoint's own planes: what [`super::seal::seal_at`] and a
/// re-fuse `append` record beside the checkpoint they write, so branching
/// onto an older fuse and repointing the head after a delete can read them
/// back without depending on the manifest, which only ever caches the
/// current head's own fuse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FuseRecord {
    pub composite: PlaneRef,
    pub align: Option<AlignRecord>,
    /// Every frame's transform and the applied crop.
    pub registration: Registration,
    pub sources: Vec<SourceRef>,
    pub metrics: Metrics,
}

impl Checkpoint {
    /// `blake3(stack_fingerprint ‖ zmap_blob ‖ created_at)`, hex-truncated
    /// to 32 characters -- deterministic, so two seals of the same stack at
    /// the same instant name the same checkpoint file, and no `uuid`
    /// dependency is needed to get a collision-free name.
    pub fn derive_id(stack_fingerprint: &str, zmap_blob: &str, created_at: i64) -> String {
        Self::derive(None, stack_fingerprint, zmap_blob, created_at)
    }

    /// `blake3(parent ‖ stack_fingerprint ‖ content ‖ created_at)`,
    /// hex-truncated to 32 characters -- the parent is folded into the hash
    /// so appending onto unchanged sources within the same wall-clock second
    /// still derives an id distinct from the checkpoint it is parented on.
    /// `content` is the Z-Map blob hash of a fused checkpoint, and the hash
    /// of its ops' JSON for an op checkpoint, which names its fused
    /// ancestor's Z-Map and so cannot tell two branches apart by it.
    pub fn derive_child_id(
        parent: &str,
        stack_fingerprint: &str,
        content: &str,
        created_at: i64,
    ) -> String {
        Self::derive(Some(parent), stack_fingerprint, content, created_at)
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
/// deserialization buffered and slow (serde-rs/serde#2101).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Op {
    /// Blend frame `source_id` into the result along `path` through a soft
    /// round mask of `radius` pixels and `softness` in `0.0..=1.0`. Strokes
    /// apply in list order; the result image is the document.
    BrushStroke {
        source_id: u16,
        path: Vec<Point>,
        radius: f32,
        softness: f32,
    },
}

/// Every checkpoint of a project, indexed by id, with each one's parent and
/// children resolved. `parent` is validated at construction: a checkpoint
/// naming a `parent` no checkpoint carries, or a parent cycle, is refused
/// there rather than walked into later.
#[derive(Debug)]
pub struct History {
    path: PathBuf,
    by_id: HashMap<String, Checkpoint>,
    children: HashMap<String, Vec<String>>,
}

impl History {
    /// Builds a validated history over `checkpoints`, every one belonging
    /// to the project at `path` (named in an error only).
    pub fn build(checkpoints: Vec<Checkpoint>, path: &Path) -> Result<History, ProjectError> {
        let by_id: HashMap<String, Checkpoint> =
            checkpoints.into_iter().map(|c| (c.id.clone(), c)).collect();

        for checkpoint in by_id.values() {
            if let Some(parent) = &checkpoint.parent
                && !by_id.contains_key(parent)
            {
                return Err(ProjectError::DanglingParent {
                    path: path.to_path_buf(),
                    checkpoint: checkpoint.id.clone(),
                    parent: parent.clone(),
                });
            }
        }
        for checkpoint in by_id.values() {
            let mut seen = HashSet::new();
            let mut current = checkpoint.id.as_str();
            loop {
                if !seen.insert(current) {
                    return Err(ProjectError::ParentCycle {
                        path: path.to_path_buf(),
                        checkpoint: checkpoint.id.clone(),
                    });
                }
                match by_id[current].parent.as_deref() {
                    Some(parent) => current = parent,
                    None => break,
                }
            }
        }

        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        for checkpoint in by_id.values() {
            if let Some(parent) = &checkpoint.parent {
                children
                    .entry(parent.clone())
                    .or_default()
                    .push(checkpoint.id.clone());
            }
        }

        Ok(History {
            path: path.to_path_buf(),
            by_id,
            children,
        })
    }

    /// The checkpoint named `id`, or [`ProjectError::UnknownCheckpoint`].
    pub fn get(&self, id: &str) -> Result<&Checkpoint, ProjectError> {
        self.by_id
            .get(id)
            .ok_or_else(|| ProjectError::UnknownCheckpoint {
                path: self.path.clone(),
                id: id.to_string(),
            })
    }

    /// Every checkpoint, newest first by `created_at`; a `created_at` tie is
    /// broken by ancestor depth (the deeper checkpoint first), and a
    /// remaining tie by id.
    pub fn all(&self) -> Vec<&Checkpoint> {
        let mut depths: HashMap<String, usize> = HashMap::new();
        for id in self.by_id.keys() {
            self.depth(id, &mut depths);
        }
        let mut all: Vec<&Checkpoint> = self.by_id.values().collect();
        all.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| depths[&b.id].cmp(&depths[&a.id]))
                .then_with(|| a.id.cmp(&b.id))
        });
        all
    }

    /// `id`'s distance from its root (0 for a root), memoized in `cache`.
    fn depth(&self, id: &str, cache: &mut HashMap<String, usize>) -> usize {
        if let Some(&d) = cache.get(id) {
            return d;
        }
        let d = match self.by_id[id].parent.as_deref() {
            Some(parent) => 1 + self.depth(parent, cache),
            None => 0,
        };
        cache.insert(id.to_string(), d);
        d
    }

    /// [`Self::all`] without the hidden checkpoints: what a listing shows.
    pub fn visible(&self) -> Vec<&Checkpoint> {
        let mut all = self.all();
        all.retain(|c| !c.hidden);
        all
    }

    /// `id` and every ancestor above it, self first, down to the root.
    pub fn ancestors(&self, id: &str) -> Result<Vec<&Checkpoint>, ProjectError> {
        let mut chain = Vec::new();
        let mut current = self.get(id)?;
        loop {
            chain.push(current);
            match &current.parent {
                Some(parent) => current = self.get(parent)?,
                None => return Ok(chain),
            }
        }
    }

    /// The ops a render of `id` applies, oldest first: those of the unbroken
    /// run of op-bearing checkpoints from `id` down, since a fused checkpoint
    /// holds the planes the ops above it blend over.
    pub fn op_chain(&self, id: &str) -> Result<Vec<&Op>, ProjectError> {
        let chain = self.ancestors(id)?;
        let applied = chain.iter().take_while(|c| !c.ops.is_empty()).count();
        Ok(chain[..applied]
            .iter()
            .rev()
            .flat_map(|c| c.ops.iter())
            .collect())
    }

    /// The direct children of `id`.
    pub fn children(&self, id: &str) -> Vec<&Checkpoint> {
        self.children
            .get(id)
            .map(|ids| ids.iter().map(|cid| &self.by_id[cid]).collect())
            .unwrap_or_default()
    }

    /// Every checkpoint descended from `id`, not including `id` itself.
    pub fn descendants(&self, id: &str) -> Vec<&Checkpoint> {
        let mut result = Vec::new();
        let mut stack: Vec<String> = self.children.get(id).cloned().unwrap_or_default();
        let mut visited = HashSet::new();
        while let Some(cid) = stack.pop() {
            if !visited.insert(cid.clone()) {
                continue;
            }
            if let Some(more) = self.children.get(&cid) {
                stack.extend(more.iter().cloned());
            }
            result.push(&self.by_id[&cid]);
        }
        result
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
    fn an_op_with_no_fields_is_refused() {
        assert!(serde_json::from_str::<Op>(r#"{"brush-stroke":{}}"#).is_err());
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
            fuse: None,
            hidden: false,
        };
        let text = serde_json::to_string(&checkpoint).unwrap();
        assert_eq!(
            serde_json::from_str::<Checkpoint>(&text).unwrap(),
            checkpoint
        );
    }

    fn bare(id: &str, parent: Option<&str>, created_at: i64) -> Checkpoint {
        Checkpoint {
            id: id.to_string(),
            parent: parent.map(str::to_string),
            ops: Vec::new(),
            zmap_blob: "blob".to_string(),
            stack_fingerprint: "fp".to_string(),
            algorithm: "laplacian".to_string(),
            params: ParamValues::new(),
            created_at,
            label: None,
            fuse: None,
            hidden: false,
        }
    }

    fn ids(checkpoints: Vec<&Checkpoint>) -> Vec<String> {
        checkpoints.into_iter().map(|c| c.id.clone()).collect()
    }

    #[test]
    fn history_resolves_by_id_and_refuses_an_unknown_one() {
        let history = History::build(vec![bare("root", None, 1)], Path::new("p")).unwrap();
        assert_eq!(history.get("root").unwrap().id, "root");
        assert!(matches!(
            history.get("missing"),
            Err(ProjectError::UnknownCheckpoint { id, .. }) if id == "missing"
        ));
    }

    #[test]
    fn history_all_is_newest_first_with_id_as_the_tiebreak() {
        let history = History::build(
            vec![
                bare("root", None, 1),
                bare("b", Some("root"), 5),
                bare("a", Some("root"), 5),
            ],
            Path::new("p"),
        )
        .unwrap();
        assert_eq!(ids(history.all()), vec!["a", "b", "root"]);
    }

    #[test]
    fn history_all_breaks_a_same_second_parent_child_tie_by_depth_before_id() {
        let history = History::build(
            vec![bare("aaa", None, 5), bare("zzz", Some("aaa"), 5)],
            Path::new("p"),
        )
        .unwrap();
        assert_eq!(ids(history.all()), vec!["zzz", "aaa"]);
    }

    #[test]
    fn history_walks_ancestors_children_and_descendants() {
        let history = History::build(
            vec![
                bare("root", None, 1),
                bare("mid", Some("root"), 2),
                bare("leaf-a", Some("mid"), 3),
                bare("leaf-b", Some("mid"), 4),
                bare("branch", Some("root"), 5),
            ],
            Path::new("p"),
        )
        .unwrap();

        let mut ancestors = ids(history.ancestors("leaf-a").unwrap());
        ancestors.sort();
        let mut expected = vec!["leaf-a".to_string(), "mid".to_string(), "root".to_string()];
        expected.sort();
        assert_eq!(ancestors, expected);

        let mut children = ids(history.children("root"));
        children.sort();
        assert_eq!(children, vec!["branch", "mid"]);

        let mut descendants = ids(history.descendants("root"));
        descendants.sort();
        assert_eq!(descendants, vec!["branch", "leaf-a", "leaf-b", "mid"]);
        assert!(history.descendants("leaf-a").is_empty());
    }

    #[test]
    fn history_refuses_a_dangling_parent() {
        let err =
            History::build(vec![bare("child", Some("ghost"), 1)], Path::new("p")).unwrap_err();
        assert!(matches!(
            err,
            ProjectError::DanglingParent { checkpoint, parent, .. }
                if checkpoint == "child" && parent == "ghost"
        ));
    }

    #[test]
    fn history_refuses_a_self_cycle_and_a_two_checkpoint_cycle() {
        let self_cycle = History::build(vec![bare("a", Some("a"), 1)], Path::new("p")).unwrap_err();
        assert!(matches!(self_cycle, ProjectError::ParentCycle { .. }));

        let two_cycle = History::build(
            vec![bare("a", Some("b"), 1), bare("b", Some("a"), 2)],
            Path::new("p"),
        )
        .unwrap_err();
        assert!(matches!(two_cycle, ProjectError::ParentCycle { .. }));
    }

    #[test]
    fn history_visible_skips_a_hidden_checkpoint_the_other_walks_keep() {
        let mut mid = bare("mid", Some("root"), 2);
        mid.hidden = true;
        let history = History::build(
            vec![bare("root", None, 1), mid, bare("leaf", Some("mid"), 3)],
            Path::new("p"),
        )
        .unwrap();
        assert_eq!(ids(history.visible()), vec!["leaf", "root"]);
        assert_eq!(ids(history.all()), vec!["leaf", "mid", "root"]);
        assert!(history.get("mid").unwrap().hidden);
        assert_eq!(
            ids(history.ancestors("leaf").unwrap()),
            ["leaf", "mid", "root"]
        );
    }

    #[test]
    fn the_hidden_flag_is_always_written_and_required() {
        let mut checkpoint = bare("a", None, 1);
        let visible = serde_json::to_string(&checkpoint).unwrap();
        assert!(visible.contains(r#""hidden":false"#));
        let read: Checkpoint = serde_json::from_str(&visible).unwrap();
        assert!(!read.hidden);
        let without = visible.replace(r#","hidden":false"#, "");
        assert!(serde_json::from_str::<Checkpoint>(&without).is_err());

        checkpoint.hidden = true;
        let hidden = serde_json::to_string(&checkpoint).unwrap();
        assert!(hidden.contains(r#""hidden":true"#));
        assert!(serde_json::from_str::<Checkpoint>(&hidden).unwrap().hidden);
    }
}
