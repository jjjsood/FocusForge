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
    /// The checkpoint's number on the colour wheel, counted from 0 in creation
    /// order. A delete leaves every other checkpoint's number as it was. A
    /// record without one reads with a number derived from creation order
    /// ([`History::colour_index`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour_index: Option<u32>,
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

/// Where a brush stroke blends its pixels from: a frame of the stack or a
/// checkpoint's result. Serialized as `{"frame": n}` or `{"checkpoint": "<id>"}`;
/// inside a stroke a frame is written as the plain `source_id: n` and a
/// checkpoint as `source: {"checkpoint": "<id>"}`; a stroke record holding both
/// fields, or neither, is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StrokeSource {
    Frame(u16),
    Checkpoint(String),
}

/// One edit in a checkpoint's op log. **Externally tagged** -- the serde
/// default, stated here because it is frozen: an untagged enum would make
/// deserialization buffered and slow (serde-rs/serde#2101).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "OpRepr", into = "OpRepr")]
#[non_exhaustive]
pub enum Op {
    /// Blend `source` into the result along `path` through a soft round mask
    /// of `radius` pixels and `softness` in `0.0..=1.0`. Strokes apply in
    /// list order; the result image is the document.
    ///
    /// `mode` and `opacity` read as [`BrushMode::Normal`] and `1.0` when
    /// absent and are left out of the serialized stroke while they hold that
    /// default, so a default stroke keeps its bytes (and the id hashed from
    /// them). `opacity` scales the stroke's coverage.
    BrushStroke {
        source: StrokeSource,
        path: Vec<Point>,
        radius: f32,
        softness: f32,
        mode: BrushMode,
        opacity: f32,
    },
}

impl Op {
    /// A [`BrushMode::Normal`] stroke at full opacity painted from frame `frame`.
    pub fn frame_stroke(frame: u16, path: Vec<Point>, radius: f32, softness: f32) -> Op {
        Op::BrushStroke {
            source: StrokeSource::Frame(frame),
            path,
            radius,
            softness,
            mode: BrushMode::Normal,
            opacity: 1.0,
        }
    }

    /// The id of the checkpoint this stroke paints from, `None` for a stroke
    /// painted from a frame.
    pub fn checkpoint_source(&self) -> Option<&str> {
        let Op::BrushStroke { source, .. } = self;
        match source {
            StrokeSource::Checkpoint(id) => Some(id),
            StrokeSource::Frame(_) => None,
        }
    }
}

/// The stored shape of [`Op`]: a stroke's source is two optional fields so
/// that the `source_id` and the `source` record both read.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum OpRepr {
    BrushStroke {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_id: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<StrokeSource>,
        path: Vec<Point>,
        radius: f32,
        softness: f32,
        #[serde(default, skip_serializing_if = "BrushMode::is_normal")]
        mode: BrushMode,
        #[serde(default = "full_opacity", skip_serializing_if = "is_full_opacity")]
        opacity: f32,
    },
}

impl From<Op> for OpRepr {
    fn from(op: Op) -> OpRepr {
        let Op::BrushStroke {
            source,
            path,
            radius,
            softness,
            mode,
            opacity,
        } = op;
        let (source_id, source) = match source {
            StrokeSource::Frame(n) => (Some(n), None),
            checkpoint => (None, Some(checkpoint)),
        };
        OpRepr::BrushStroke {
            source_id,
            source,
            path,
            radius,
            softness,
            mode,
            opacity,
        }
    }
}

impl TryFrom<OpRepr> for Op {
    type Error = &'static str;

    fn try_from(repr: OpRepr) -> Result<Op, Self::Error> {
        let OpRepr::BrushStroke {
            source_id,
            source,
            path,
            radius,
            softness,
            mode,
            opacity,
        } = repr;
        let source = match (source_id, source) {
            (Some(n), None) => StrokeSource::Frame(n),
            (None, Some(source)) => source,
            (Some(_), Some(_)) => return Err("a brush stroke names both `source_id` and `source`"),
            (None, None) => return Err("missing field `source_id` (or `source`)"),
        };
        Ok(Op::BrushStroke {
            source,
            path,
            radius,
            softness,
            mode,
            opacity,
        })
    }
}

/// The blend mode a stroke records. `Detail` keeps the result's own
/// brightness and takes only the source's fine structure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrushMode {
    #[default]
    Normal,
    Lighten,
    Darken,
    Detail,
}

impl BrushMode {
    fn is_normal(&self) -> bool {
        *self == BrushMode::Normal
    }
}

fn full_opacity() -> f32 {
    1.0
}

fn is_full_opacity(opacity: &f32) -> bool {
    *opacity == 1.0
}

/// The checkpoints `checkpoint`'s strokes are painted from, in op order.
fn stroke_sources(checkpoint: &Checkpoint) -> impl Iterator<Item = &str> {
    checkpoint.ops.iter().filter_map(Op::checkpoint_source)
}

/// Refuses a stroke painted from a checkpoint `by_id` lacks, and any chain of
/// parent and stroke-source links that returns to where it started. Parent
/// links are already known to be sound, so every cycle found holds a source
/// link. The walk is iterative and visits checkpoints in id order, so the
/// error does not depend on input order.
fn refuse_bad_stroke_sources(
    by_id: &HashMap<String, Checkpoint>,
    path: &Path,
) -> Result<(), ProjectError> {
    let mut ids: Vec<&str> = by_id.keys().map(String::as_str).collect();
    ids.sort_unstable();
    for &id in &ids {
        if let Some(missing) = stroke_sources(&by_id[id]).find(|s| !by_id.contains_key(*s)) {
            return Err(ProjectError::DanglingStrokeSource {
                path: path.to_path_buf(),
                checkpoint: id.to_string(),
                referenced: missing.to_string(),
            });
        }
    }

    // One frame per checkpoint on the current path: its out-links, each
    // flagged when it is a stroke source, and how far they are walked.
    struct Frame<'a> {
        id: &'a str,
        entered_by_source: bool,
        links: Vec<(&'a str, bool)>,
        next: usize,
    }
    fn frame<'a>(
        by_id: &'a HashMap<String, Checkpoint>,
        id: &'a str,
        entered_by_source: bool,
    ) -> Frame<'a> {
        let checkpoint = &by_id[id];
        Frame {
            id,
            entered_by_source,
            links: checkpoint
                .parent
                .as_deref()
                .map(|p| (p, false))
                .into_iter()
                .chain(stroke_sources(checkpoint).map(|s| (s, true)))
                .collect(),
            next: 0,
        }
    }

    let mut on_path: HashSet<&str> = HashSet::new();
    let mut done: HashSet<&str> = HashSet::new();
    for &start in &ids {
        if done.contains(start) {
            continue;
        }
        let mut stack = vec![frame(by_id, start, false)];
        on_path.insert(start);
        while let Some(top) = stack.last_mut() {
            let Some(&(to, is_source)) = top.links.get(top.next) else {
                on_path.remove(top.id);
                done.insert(top.id);
                stack.pop();
                continue;
            };
            top.next += 1;
            let from = top.id;
            if on_path.contains(to) {
                // The cycle is `to` .. `from` along the path, closed by this link.
                let first = stack
                    .iter()
                    .position(|f| f.id == to)
                    .expect("`to` is on the path");
                let (holder, referenced) = if is_source {
                    (from, to)
                } else {
                    let k = (first + 1..stack.len())
                        .find(|&k| stack[k].entered_by_source)
                        .expect("a cycle of parent links alone was refused earlier");
                    (stack[k - 1].id, stack[k].id)
                };
                return Err(ProjectError::StrokeSourceCycle {
                    path: path.to_path_buf(),
                    checkpoint: holder.to_string(),
                    referenced: referenced.to_string(),
                });
            }
            if !done.contains(to) {
                on_path.insert(to);
                stack.push(frame(by_id, to, is_source));
            }
        }
    }
    Ok(())
}

/// Every checkpoint of a project, indexed by id, with each one's parent and
/// children resolved. Links are validated at construction: a checkpoint
/// naming a `parent` or a stroke source no checkpoint carries, or a cycle of
/// parent and stroke-source links, is refused there rather than walked into
/// later.
#[derive(Debug)]
pub struct History {
    path: PathBuf,
    by_id: HashMap<String, Checkpoint>,
    children: HashMap<String, Vec<String>>,
    colours: HashMap<String, u32>,
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

        refuse_bad_stroke_sources(&by_id, path)?;

        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        for checkpoint in by_id.values() {
            if let Some(parent) = &checkpoint.parent {
                children
                    .entry(parent.clone())
                    .or_default()
                    .push(checkpoint.id.clone());
            }
        }

        let mut history = History {
            path: path.to_path_buf(),
            by_id,
            children,
            colours: HashMap::new(),
        };
        history.colours = history.resolve_colours();
        Ok(history)
    }

    /// Each checkpoint's colour number: the stored one, else the next number
    /// above everything before it in creation order.
    fn resolve_colours(&self) -> HashMap<String, u32> {
        let mut next = 0;
        let mut colours = HashMap::with_capacity(self.by_id.len());
        for checkpoint in self.all().into_iter().rev() {
            let colour = checkpoint.colour_index.unwrap_or(next);
            next = next.max(colour.saturating_add(1));
            colours.insert(checkpoint.id.clone(), colour);
        }
        colours
    }

    /// The colour number of checkpoint `id`, or
    /// [`ProjectError::UnknownCheckpoint`].
    pub fn colour_index(&self, id: &str) -> Result<u32, ProjectError> {
        self.get(id)?;
        Ok(self.colours[id])
    }

    /// The colour number a checkpoint written next takes: one above the
    /// largest number among the checkpoints the history holds, 0 for none.
    pub fn next_colour_index(&self) -> u32 {
        self.colours
            .values()
            .map(|c| c.saturating_add(1))
            .max()
            .unwrap_or(0)
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

    /// How many strokes, across every stored checkpoint (hidden ones too),
    /// paint from the checkpoint `id`; 0 for an id nothing paints from.
    pub fn strokes_using(&self, id: &str) -> usize {
        strokes_using_in(id, self.op_groups().map(|(_, ops)| ops)).0
    }

    /// The id and ops of every stored checkpoint (hidden ones too), in no
    /// particular order.
    pub fn op_groups(&self) -> impl Iterator<Item = (&str, &[Op])> {
        self.by_id
            .iter()
            .map(|(id, c)| (id.as_str(), c.ops.as_slice()))
    }
}

/// How many strokes in `groups` paint from the checkpoint named `key`, and
/// how many of the groups hold at least one of them: `(strokes, containers)`.
/// A group is the ops of one checkpoint or of the live edit; `key` is a
/// stored checkpoint's id or an unsaved one's `held:<index>` key, the form a
/// stroke carries for its source (see [`Op::checkpoint_source`]).
pub fn strokes_using_in<'a>(
    key: &str,
    groups: impl IntoIterator<Item = &'a [Op]>,
) -> (usize, usize) {
    groups
        .into_iter()
        .map(|ops| {
            ops.iter()
                .filter(|op| op.checkpoint_source() == Some(key))
                .count()
        })
        .filter(|&n| n > 0)
        .fold((0, 0), |(strokes, groups), n| (strokes + n, groups + 1))
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
            colour_index: None,
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
            colour_index: None,
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

    /// `checkpoint` with one default stroke painted from each of `sources`.
    fn painting_from(mut checkpoint: Checkpoint, sources: &[&str]) -> Checkpoint {
        for source in sources {
            checkpoint.ops.push(Op::BrushStroke {
                source: StrokeSource::Checkpoint((*source).to_string()),
                path: vec![Point { x: 1, y: 2 }],
                radius: 3.0,
                softness: 0.5,
                mode: BrushMode::Normal,
                opacity: 1.0,
            });
        }
        checkpoint
    }

    #[test]
    fn strokes_using_in_counts_strokes_and_the_groups_holding_them() {
        let stored = "0123456789abcdef0123456789abcdef";
        let from = |sources: &[&str]| painting_from(bare("x", None, 1), sources).ops;
        let frame = Op::frame_stroke(0, vec![Point { x: 0, y: 0 }], 1.0, 0.0);
        let stored_group = from(&[stored, stored, "held:0"]);
        let held_group = from(&["held:0", stored]);
        let live_group = [from(&["held:0"]), vec![frame.clone()]].concat();
        let none = from(&[]);

        let count = |key: &str, groups: &[&[Op]]| strokes_using_in(key, groups.iter().copied());
        let groups: [&[Op]; 4] = [&stored_group, &held_group, &live_group, &none];
        assert_eq!(count(stored, &groups), (3, 2), "a stored id, two groups");
        assert_eq!(count("held:0", &groups), (3, 3), "the held:<i> key form");
        assert_eq!(count("held:1", &groups), (0, 0), "no stroke names it");
        assert_eq!(
            count(stored, &[&none, &[frame]]),
            (0, 0),
            "frames never use it"
        );
        assert_eq!(count(stored, &[]), (0, 0), "no groups");
    }

    fn build_err(checkpoints: Vec<Checkpoint>) -> ProjectError {
        History::build(checkpoints, Path::new("p")).unwrap_err()
    }

    fn assert_cycle(err: ProjectError, holder: &str, named: &str) {
        match err {
            ProjectError::StrokeSourceCycle {
                checkpoint,
                referenced,
                ..
            } => assert_eq!((checkpoint.as_str(), referenced.as_str()), (holder, named)),
            other => panic!("expected StrokeSourceCycle, got {other:?}"),
        }
    }

    #[test]
    fn stroke_source_dangling_names_both_ids() {
        let err = build_err(vec![
            bare("root", None, 1),
            painting_from(bare("child", Some("root"), 2), &["ghost"]),
        ]);
        assert!(matches!(
            err,
            ProjectError::DanglingStrokeSource { checkpoint, referenced, .. }
                if checkpoint == "child" && referenced == "ghost"
        ));
    }

    #[test]
    fn stroke_source_naming_its_own_checkpoint_is_a_cycle() {
        let err = build_err(vec![
            bare("root", None, 1),
            painting_from(bare("a", Some("root"), 2), &["a"]),
        ]);
        assert_cycle(err, "a", "a");
    }

    #[test]
    fn stroke_source_naming_a_descendant_is_a_cycle() {
        let err = build_err(vec![
            painting_from(bare("root", None, 1), &["leaf"]),
            bare("mid", Some("root"), 2),
            bare("leaf", Some("mid"), 3),
        ]);
        assert_cycle(err, "root", "leaf");
    }

    #[test]
    fn stroke_source_two_checkpoints_naming_each_other_is_a_cycle() {
        let err = build_err(vec![
            painting_from(bare("a", None, 1), &["b"]),
            painting_from(bare("b", None, 2), &["a"]),
        ]);
        assert!(matches!(err, ProjectError::StrokeSourceCycle { .. }));
    }

    #[test]
    fn stroke_source_cycle_through_parent_and_source_links_is_refused() {
        // a -source-> b -parent-> c -source-> a.
        let err = build_err(vec![
            painting_from(bare("a", None, 1), &["b"]),
            bare("b", Some("c"), 2),
            painting_from(bare("c", None, 3), &["a"]),
        ]);
        assert!(matches!(err, ProjectError::StrokeSourceCycle { .. }));
    }

    #[test]
    fn stroke_source_naming_an_ancestor_a_sibling_or_the_same_one_twice_is_accepted() {
        let history = History::build(
            vec![
                bare("root", None, 1),
                bare("x", Some("root"), 2),
                painting_from(bare("y", Some("root"), 3), &["x", "root", "x"]),
                painting_from(bare("z", Some("y"), 4), &["root", "x"]),
            ],
            Path::new("p"),
        );
        assert!(history.is_ok(), "{history:?}");
    }

    #[test]
    fn stroke_source_verdict_does_not_depend_on_the_input_order() {
        let cyclic = || {
            vec![
                painting_from(bare("a", None, 1), &["b"]),
                bare("b", Some("c"), 2),
                painting_from(bare("c", None, 3), &["a"]),
                bare("d", Some("a"), 4),
            ]
        };
        let sound = || {
            vec![
                bare("a", None, 1),
                painting_from(bare("b", Some("a"), 2), &["a"]),
                painting_from(bare("c", Some("b"), 3), &["a", "b"]),
            ]
        };
        for build in [cyclic, sound] {
            let forward = build();
            let mut reversed = build();
            reversed.reverse();
            let verdict = |v| {
                History::build(v, Path::new("p"))
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            };
            assert_eq!(verdict(forward), verdict(reversed));
        }
        assert!(History::build(cyclic(), Path::new("p")).is_err());
        assert!(History::build(sound(), Path::new("p")).is_ok());
    }

    #[test]
    fn stroke_source_cycle_over_a_long_parent_chain_does_not_overflow_the_stack() {
        let mut chain: Vec<Checkpoint> = (0..5_000)
            .map(|i| {
                let parent = (i > 0).then(|| format!("c{}", i - 1));
                bare(&format!("c{i}"), parent.as_deref(), i)
            })
            .collect();
        chain[0] = painting_from(chain[0].clone(), &["c4999"]);
        assert_cycle(build_err(chain), "c0", "c4999");
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

    fn numbered(mut checkpoint: Checkpoint, colour: u32) -> Checkpoint {
        checkpoint.colour_index = Some(colour);
        checkpoint
    }

    fn colours(history: &History, ids: &[&str]) -> Vec<u32> {
        ids.iter()
            .map(|id| history.colour_index(id).unwrap())
            .collect()
    }

    #[test]
    fn colour_index_is_derived_in_creation_order_for_records_without_one() {
        // Input order is not creation order; the sibling shares its second.
        let history = History::build(
            vec![
                bare("c", Some("b"), 30),
                bare("root", None, 10),
                bare("b", Some("root"), 20),
                bare("d", Some("root"), 20),
            ],
            Path::new("p"),
        )
        .unwrap();
        let derived = colours(&history, &["root", "b", "d", "c"]);
        assert_eq!(derived[0], 0);
        assert_eq!(derived[3], 3, "the newest takes the highest number");
        let mut middle = [derived[1], derived[2]];
        middle.sort();
        assert_eq!(middle, [1, 2]);
        assert_eq!(history.next_colour_index(), 4);
    }

    #[test]
    fn colour_index_stored_numbers_win_and_derived_ones_never_collide() {
        // A legacy record first, then a stored one: the legacy one takes 0.
        let legacy_first = History::build(
            vec![bare("a", None, 1), numbered(bare("b", Some("a"), 2), 5)],
            Path::new("p"),
        )
        .unwrap();
        assert_eq!(colours(&legacy_first, &["a", "b"]), [0, 5]);
        assert_eq!(legacy_first.next_colour_index(), 6);

        // A stored one first, then a legacy record: it continues above it.
        let stored_first = History::build(
            vec![numbered(bare("a", None, 1), 3), bare("b", Some("a"), 2)],
            Path::new("p"),
        )
        .unwrap();
        assert_eq!(colours(&stored_first, &["a", "b"]), [3, 4]);
        assert_eq!(stored_first.next_colour_index(), 5);

        // A gap left by a dropped checkpoint stays a gap.
        let gap = History::build(
            vec![
                numbered(bare("a", None, 1), 0),
                numbered(bare("c", Some("a"), 3), 2),
            ],
            Path::new("p"),
        )
        .unwrap();
        assert_eq!(colours(&gap, &["a", "c"]), [0, 2]);
        assert_eq!(gap.next_colour_index(), 3);
    }

    #[test]
    fn colour_index_next_is_zero_for_an_empty_history_and_an_unknown_id_is_refused() {
        let empty = History::build(Vec::new(), Path::new("p")).unwrap();
        assert_eq!(empty.next_colour_index(), 0);
        assert!(matches!(
            empty.colour_index("ghost"),
            Err(ProjectError::UnknownCheckpoint { id, .. }) if id == "ghost"
        ));
    }

    #[test]
    fn colour_index_is_written_only_when_stored() {
        let stored = serde_json::to_string(&numbered(bare("a", None, 1), 7)).unwrap();
        assert!(stored.contains(r#""colour_index":7"#), "{stored}");
        let read: Checkpoint = serde_json::from_str(&stored).unwrap();
        assert_eq!(read.colour_index, Some(7));

        let without = serde_json::to_string(&bare("a", None, 1)).unwrap();
        assert!(!without.contains("colour_index"), "{without}");
        let read: Checkpoint = serde_json::from_str(&without).unwrap();
        assert_eq!(read.colour_index, None);
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
