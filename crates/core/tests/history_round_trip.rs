// SPDX-License-Identifier: GPL-3.0-or-later

//! Random op sequences over synthetic Z-Maps are sealed, branched and deleted
//! through both project writers; after reopening the file, every checkpoint
//! must materialize to the map an in-memory model computed. The model holds
//! plain `Vec<u16>` maps and shares nothing with the container. A failing
//! case prints its shrunk action list; no failure file is kept.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use focusforge_core::align::Registration;
use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::{
    self, BlobKind, InMemoryCheckpoint, InMemoryKind, Metrics, Op, ParentRef, Point, Project,
    SealRequest,
};
use focusforge_core::zmap::{ZMapInfo, ZMapReader, ZMapWriter};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::{Config, TestCaseError, TestError, TestRunner};

const SPP: u32 = 3;

/// The root's `created_at`; node `i` is sealed at `BASE_TIME + i`, so creation
/// order is strictly increasing and no two siblings derive one id.
const BASE_TIME: i64 = 1_700_000_000;

fn tmp_dir(label: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("history_round_trip_{label}_{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn full(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

fn zmap_noisy(w: u32, h: u32, frames: u32, salt: u32) -> Vec<u16> {
    (0..w * h)
        .map(|i| {
            let x = i % w;
            let y = i / w;
            let h1 = x
                .wrapping_mul(2_654_435_761)
                .wrapping_add(y.wrapping_mul(40_503))
                .wrapping_add(salt);
            (h1 % frames) as u16
        })
        .collect()
}

fn stage_planes(dir: &Path, g: Geometry, salt: u32) -> (PathBuf, PathBuf) {
    let Geometry { w, h, frames } = g;
    let composite = dir.join("root-composite.tif");
    let zmap = dir.join("root-zmap.tif");
    let mut writer = TiffWriter::create(
        &composite,
        TiffSpec {
            width: w,
            height: h,
            samples_per_pixel: SPP,
            description: Some("staged composite".to_string()),
        },
    )
    .unwrap();
    let samples: Vec<u16> = (0..(w * h * SPP) as usize).map(|i| i as u16).collect();
    writer.write_tile(full(w, h), &samples).unwrap();
    writer.finish().unwrap();

    let mut map = ZMapWriter::create(&zmap, ZMapInfo::new(w, h, frames).unwrap()).unwrap();
    map.write_tile(full(w, h), &zmap_noisy(w, h, frames, salt))
        .unwrap();
    map.finish().unwrap();
    (composite, zmap)
}

fn write_sources(dir: &Path, count: usize, g: Geometry) -> Vec<PathBuf> {
    (0..count)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            let mut writer = TiffWriter::create(
                &path,
                TiffSpec {
                    width: g.w,
                    height: g.h,
                    samples_per_pixel: SPP,
                    description: None,
                },
            )
            .unwrap();
            let samples: Vec<u16> = (0..(g.w * g.h * SPP) as usize)
                .map(|j| ((j + i * 13) % 65_536) as u16)
                .collect();
            writer.write_tile(full(g.w, g.h), &samples).unwrap();
            writer.finish().unwrap();
            path
        })
        .collect()
}

fn read_zmap_pixels(path: &Path) -> Vec<u16> {
    let reader = ZMapReader::open(path).unwrap();
    let (w, h) = reader.dimensions();
    let mut buf = vec![0u16; w as usize * h as usize];
    reader.read_tile_into(full(w, h), &mut buf).unwrap();
    buf
}

/// A sealed root over a noisy synthetic map in a fresh directory; returns the
/// directory, the project path and the model that starts at that root.
fn start(label: &str, g: Geometry, salt: u32) -> (PathBuf, PathBuf, Model) {
    let dir = tmp_dir(label);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, g, salt);
    let sources = write_sources(&dir, 3, g);
    let mut params = ParamValues::new();
    params.insert("levels".to_string(), ParamValue::Int(5));
    let path = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    project::seal_at(
        SealRequest {
            project_path: &path,
            staged_composite: &composite,
            staged_zmap: &zmap,
            sources: &sources,
            algorithm: "laplacian",
            params: &params,
            align: None,
            registration: Registration::default(),
            composite_description: "staged composite".to_string(),
            label: None,
            metrics: Metrics::default(),
        },
        BASE_TIME,
    )
    .expect("seal must succeed");
    let root_id = Project::open(&path).unwrap().checkpoint().id.clone();
    let model = Model::new(root_id, zmap_noisy(g.w, g.h, g.frames, salt));
    (dir, path, model)
}

// ---------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------

/// Odd sizes on purpose: the 512 x 512 golden map is the CLI suite's.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    w: u32,
    h: u32,
    frames: u32,
}

fn geometry() -> impl Strategy<Value = Geometry> {
    (17u32..64, 9u32..48, 2u32..12).prop_map(|(w, h, frames)| Geometry { w, h, frames })
}

fn brush_stroke(g: Geometry) -> BoxedStrategy<Op> {
    let point =
        (-10i32..g.w as i32 + 10, -10i32..g.h as i32 + 10).prop_map(|(x, y)| Point { x, y });
    (
        0u16..g.frames as u16,
        vec(point, 1..=4),
        0.5f32..6.0,
        0.0f32..=1.0,
    )
        .prop_map(|(source_id, path, radius, softness)| Op::BrushStroke {
            source_id,
            path,
            radius,
            softness,
        })
        .boxed()
}

fn ops_list(g: Geometry) -> BoxedStrategy<Vec<Op>> {
    vec(brush_stroke(g), 1..=3).boxed()
}

/// One step of a generated history. Picks index the model's visible nodes in
/// creation order, so every action is valid by construction.
#[derive(Debug, Clone)]
enum Action {
    /// Seal `ops` under the picked visible node; `None` is the root.
    Seal { parent: Option<Index>, ops: Vec<Op> },
    /// Delete the picked visible node; skipped while only one is visible.
    Delete { pick: Index },
}

fn seal_action(g: Geometry) -> BoxedStrategy<Action> {
    (any::<Index>(), ops_list(g))
        .prop_map(|(parent, ops)| Action::Seal {
            parent: Some(parent),
            ops,
        })
        .boxed()
}

fn seal_or_delete(g: Geometry) -> BoxedStrategy<Action> {
    prop_oneof![
        3 => seal_action(g),
        1 => any::<Index>().prop_map(|pick| Action::Delete { pick }),
    ]
    .boxed()
}

fn random_seals(g: Geometry) -> BoxedStrategy<Vec<Action>> {
    vec(seal_action(g), 0..24).boxed()
}

/// Two seals onto the root, a random tail with deletes, then a final delete.
fn branched_and_deleted(g: Geometry) -> BoxedStrategy<Vec<Action>> {
    (
        vec(ops_list(g), 2),
        vec(seal_or_delete(g), 0..16),
        any::<Index>(),
    )
        .prop_map(|(forks, tail, pick)| {
            let mut actions: Vec<Action> = forks
                .into_iter()
                .map(|ops| Action::Seal { parent: None, ops })
                .collect();
            actions.extend(tail);
            actions.push(Action::Delete { pick });
            actions
        })
        .boxed()
}

fn long_chain(g: Geometry) -> BoxedStrategy<Vec<Vec<Op>>> {
    vec(ops_list(g), 70..=80).boxed()
}

/// A geometry, a root salt and whatever `shape` generates for that geometry.
fn case<T: std::fmt::Debug + 'static>(
    shape: fn(Geometry) -> BoxedStrategy<T>,
) -> impl Strategy<Value = (Geometry, u32, T)> {
    (geometry(), 0u32..1000).prop_flat_map(move |(g, salt)| (Just(g), Just(salt), shape(g)))
}

// ---------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------

struct Node {
    /// Filled in once the writer has sealed the node; empty for a node a
    /// writer never wrote.
    id: String,
    parent: Option<usize>,
    map: Vec<u16>,
    hidden: bool,
    removed: bool,
}

/// A resolved action: what a writer replays against the file.
#[derive(Clone)]
enum Step {
    Seal {
        node: usize,
        parent: usize,
        ops: Vec<Op>,
        created_at: i64,
    },
    /// `removed` is every node this delete takes out of the file.
    Delete { node: usize, removed: Vec<usize> },
}

/// Mirrors the project's seal, hide and collect rules over plain maps. Every
/// op node materializes the root's map: an op checkpoint names its fused
/// ancestor's, and the history holds no re-fuse.
struct Model {
    nodes: Vec<Node>,
    steps: Vec<Step>,
}

impl Model {
    fn new(root_id: String, root_map: Vec<u16>) -> Self {
        Model {
            nodes: vec![Node {
                id: root_id,
                parent: None,
                map: root_map,
                hidden: false,
                removed: false,
            }],
            steps: Vec::new(),
        }
    }

    fn visible(&self) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|&i| !self.nodes[i].hidden)
            .collect()
    }

    /// Seals `ops` under `parent`, returning the new node.
    fn seal(&mut self, parent: usize, ops: Vec<Op>) -> usize {
        let map = self.nodes[0].map.clone();
        let node = self.nodes.len();
        self.nodes.push(Node {
            id: String::new(),
            parent: Some(parent),
            map,
            hidden: false,
            removed: false,
        });
        self.steps.push(Step::Seal {
            node,
            parent,
            ops,
            created_at: BASE_TIME + node as i64,
        });
        node
    }

    fn descends_from(&self, mut node: usize, ancestor: usize) -> bool {
        while let Some(parent) = self.nodes[node].parent {
            if parent == ancestor {
                return true;
            }
            node = parent;
        }
        false
    }

    /// Hides `node`, then removes every hidden node whose descendants are all
    /// hidden.
    fn delete(&mut self, node: usize) {
        self.nodes[node].hidden = true;
        let removed: Vec<usize> = (0..self.nodes.len())
            .filter(|&i| {
                let n = &self.nodes[i];
                n.hidden
                    && !n.removed
                    && (i + 1..self.nodes.len())
                        .all(|j| !self.descends_from(j, i) || self.nodes[j].hidden)
            })
            .collect();
        for &i in &removed {
            self.nodes[i].removed = true;
        }
        self.steps.push(Step::Delete { node, removed });
    }

    fn play(&mut self, actions: &[Action]) {
        for action in actions {
            let visible = self.visible();
            match action {
                Action::Seal { parent, ops } => {
                    let parent = parent
                        .as_ref()
                        .map_or(0, |pick| visible[pick.index(visible.len())]);
                    self.seal(parent, ops.clone());
                }
                Action::Delete { pick } if visible.len() >= 2 => {
                    self.delete(visible[pick.index(visible.len())]);
                }
                Action::Delete { .. } => {}
            }
        }
    }

    fn has_branch(&self) -> bool {
        (0..self.nodes.len()).any(|i| {
            self.nodes
                .iter()
                .filter(|n| n.parent == Some(i))
                .nth(1)
                .is_some()
        })
    }

    fn delete_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| matches!(s, Step::Delete { .. }))
            .count()
    }

    fn ids(&self, nodes: &[usize]) -> BTreeSet<String> {
        nodes.iter().map(|&i| self.nodes[i].id.clone()).collect()
    }
}

// ---------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------

fn fail(what: &str, err: impl std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(format!("{what}: {err}"))
}

/// One `append_ops_onto` / `delete_checkpoint` call per step: the core
/// calls behind the GUI's save and `history delete`.
fn write_per_checkpoint(path: &Path, model: &mut Model) -> Result<(), TestCaseError> {
    for step in model.steps.clone() {
        match step {
            Step::Seal {
                node,
                parent,
                ops,
                created_at,
            } => {
                let sealed =
                    project::append_ops_onto(path, &model.nodes[parent].id, ops, None, created_at)
                        .map_err(|e| fail(&format!("seal node {node} under node {parent}"), e))?;
                model.nodes[node].id = sealed.checkpoint;
            }
            Step::Delete { node, removed } => {
                let deleted = project::delete_checkpoint(path, &model.nodes[node].id)
                    .map_err(|e| fail(&format!("delete node {node}"), e))?;
                let got: BTreeSet<String> = deleted.removed.into_iter().collect();
                prop_assert_eq!(
                    got,
                    model.ids(&removed),
                    "delete of node {} removed a different set than the model's",
                    node
                );
            }
        }
    }
    Ok(())
}

/// Every step in one `seal_in_memory` call: what the shell's project save
/// runs. The entries it leaves out must be the model's removed nodes.
fn write_in_memory(path: &Path, model: &mut Model) -> Result<(), TestCaseError> {
    let root = model.nodes[0].id.clone();
    let reference = |node: usize| match node {
        0 => ParentRef::Saved(root.clone()),
        n => ParentRef::Entry(n - 1),
    };
    let mut entries = Vec::new();
    let mut deletes = Vec::new();
    for step in &model.steps {
        match step {
            Step::Seal {
                parent,
                ops,
                created_at,
                ..
            } => entries.push(InMemoryCheckpoint {
                kind: InMemoryKind::Ops {
                    ops: ops.clone(),
                    created_at: *created_at,
                },
                parent: Some(reference(*parent)),
            }),
            Step::Delete { node, .. } => deletes.push(reference(*node)),
        }
    }

    let kept = project::kept_entries(&entries, &deletes, None);
    let expected: Vec<bool> = model.nodes[1..].iter().map(|n| !n.removed).collect();
    prop_assert_eq!(&kept, &expected, "entries left out of the save");

    let ids = project::seal_in_memory(path, &entries, &deletes, None, true)
        .map_err(|e| fail("in-memory save", e))?;
    let written: Vec<usize> = (1..model.nodes.len())
        .filter(|&i| !model.nodes[i].removed)
        .collect();
    prop_assert_eq!(ids.len(), written.len(), "ids returned per written entry");
    for (node, id) in written.into_iter().zip(ids) {
        model.nodes[node].id = id;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------

/// Reopens `path` in a fresh `Project::open` and compares it with `model`:
/// every removed node is gone, every other one materializes to its sealed map
/// with the flag the model holds, the listing is the model's visible set and
/// the head is the newest visible node.
fn check_round_trip(path: &Path, model: &Model, scratch: &Path) -> Result<(), TestCaseError> {
    let mut project = Project::open(path).map_err(|e| fail("reopen", e))?;
    let stored = (0..model.nodes.len())
        .filter(|&i| !model.nodes[i].removed)
        .count();
    prop_assert_eq!(project.history().all().len(), stored, "stored checkpoints");

    for (i, node) in model.nodes.iter().enumerate() {
        if node.removed {
            prop_assert!(
                node.id.is_empty() || project.history().get(&node.id).is_err(),
                "removed node {} is still in the file",
                i
            );
            continue;
        }
        let checkpoint = project
            .history()
            .get(&node.id)
            .map_err(|e| fail(&format!("node {i} missing"), e))?
            .clone();
        prop_assert_eq!(checkpoint.hidden, node.hidden, "node {}: hidden flag", i);
        let out = scratch.join(format!("node-{i}.tif"));
        project::materialize_zmap(&mut project, &checkpoint, &out)
            .map_err(|e| fail(&format!("materialize node {i}"), e))?;
        prop_assert!(
            read_zmap_pixels(&out) == node.map,
            "node {}: the reopened map differs from the map it was sealed from",
            i
        );
        std::fs::remove_file(&out).unwrap();
    }

    let listed: BTreeSet<String> = project
        .history()
        .visible()
        .iter()
        .map(|c| c.id.clone())
        .collect();
    let visible = model.visible();
    prop_assert_eq!(listed, model.ids(&visible), "visible checkpoints");
    let newest = visible.last().expect("one node stays visible");
    prop_assert_eq!(
        &project.checkpoint().id,
        &model.nodes[*newest].id,
        "head is not the newest visible checkpoint"
    );
    Ok(())
}

fn config(cases: u32) -> ProptestConfig {
    ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

// ---------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------

proptest! {
    #![proptest_config(config(32))]

    #[test]
    fn random_sequences_round_trip((g, salt, actions) in case(random_seals)) {
        let (dir, path, mut model) = start("random", g, salt);
        model.play(&actions);
        write_per_checkpoint(&path, &mut model)?;
        check_round_trip(&path, &model, &dir)?;
    }

    #[test]
    fn branched_and_pruned_histories_round_trip(
        (g, salt, actions) in case(branched_and_deleted)
    ) {
        let (dir, path, mut model) = start("branched", g, salt);
        model.play(&actions);
        prop_assert!(model.has_branch(), "the case has no branch");
        prop_assert!(model.delete_count() >= 1, "the case deletes nothing");
        write_per_checkpoint(&path, &mut model)?;
        check_round_trip(&path, &model, &dir)?;
    }

    #[test]
    fn in_memory_save_round_trips((g, salt, actions) in case(branched_and_deleted)) {
        let (dir, path, mut model) = start("in_memory", g, salt);
        model.play(&actions);
        prop_assert!(model.has_branch(), "the case has no branch");
        prop_assert!(model.delete_count() >= 1, "the case deletes nothing");
        write_in_memory(&path, &mut model)?;
        check_round_trip(&path, &model, &dir)?;
    }
}

proptest! {
    #![proptest_config(config(4))]

    #[test]
    fn long_histories_round_trip((g, salt, chain) in case(long_chain)) {
        let (dir, path, mut model) = start("long", g, salt);
        for ops in chain {
            let newest = model.nodes.len() - 1;
            model.seal(newest, ops);
        }
        write_per_checkpoint(&path, &mut model)?;
        check_round_trip(&path, &model, &dir)?;

        let project = Project::open(&path).map_err(|e| fail("reopen", e))?;
        let full_blobs = model
            .nodes
            .iter()
            .filter(|n| {
                let checkpoint = project.history().get(&n.id).expect("no node was deleted");
                project
                    .manifest()
                    .blobs
                    .iter()
                    .any(|b| b.hash == checkpoint.zmap_blob && b.kind == BlobKind::Full)
            })
            .count();
        prop_assert_eq!(
            full_blobs,
            model.nodes.len(),
            "every checkpoint's Z-Map is one full blob"
        );
    }
}

#[test]
fn failure_prints_the_minimal_sequence() {
    let g = Geometry {
        w: 32,
        h: 24,
        frames: 6,
    };
    let mut runner = TestRunner::new(Config {
        failure_persistence: None,
        ..Config::default()
    });
    let result = runner.run(&vec(seal_action(g), 0..24), |actions| {
        prop_assert!(actions.is_empty(), "{} seals", actions.len());
        Ok(())
    });
    let Err(error @ TestError::Fail(..)) = result else {
        panic!("a property that rejects every non-empty sequence must fail");
    };
    let TestError::Fail(_, minimal) = &error else {
        unreachable!("matched above");
    };
    assert_eq!(minimal.len(), 1, "shrinking must reach one seal");
    let printed = error.to_string();
    assert!(
        printed.contains("minimal failing input"),
        "the failure does not print the shrunk input:\n{printed}"
    );
    assert!(
        printed.contains("BrushStroke"),
        "the printed input does not name the ops:\n{printed}"
    );
}
