// SPDX-License-Identifier: GPL-3.0-or-later

//! The append-only project container: every save only adds bytes at the end
//! and commits with one footer, a cut or damaged tail opens to the last
//! committed state, and a delete drops index entries without moving a byte.
//! Everything runs over `fixtures/mantis-512`.

mod common;

use std::io::Read;
use std::path::{Path, PathBuf};

use focusforge_core::align::Registration;
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::container::{ContainerWriter, HEADER_LEN, Index, TRAILER_LEN};
use focusforge_core::project::{
    self, AppendRequest, HeldCheckpoint, InMemoryCheckpoint, InMemoryKind, Metrics, Op, ParentRef,
    Plane, Point, Project, ProjectError, SealRequest,
};

const T0: i64 = 1_700_000_000;

fn tmp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn params() -> ParamValues {
    let mut values = ParamValues::new();
    values.insert("levels".to_string(), ParamValue::Int(5));
    values
}

fn stroke(x: i32, y: i32) -> Vec<Op> {
    vec![Op::BrushStroke {
        source_id: 1,
        path: vec![Point { x, y }],
        radius: 3.0,
        softness: 0.0,
    }]
}

/// A project over `fixtures/mantis-512`'s golden planes, in its own directory.
struct Fixture {
    dir: PathBuf,
    project: PathBuf,
    composite: PathBuf,
    zmap: PathBuf,
    sources: Vec<PathBuf>,
    root: String,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = tmp_dir(name);
        let mantis = common::mantis_512_dir();
        let composite = dir.join("golden-composite.tif");
        std::fs::copy(mantis.join("golden").join("laplacian.tif"), &composite).unwrap();
        let zmap = dir.join("golden-zmap.tif");
        std::fs::copy(mantis.join("golden").join("laplacian-zmap.tif"), &zmap).unwrap();
        let sources: Vec<PathBuf> = (0..8)
            .map(|i| mantis.join("frames").join(format!("{i:02}.tif")))
            .collect();
        let project = dir.join("demo.ffp");
        let values = params();
        let root = project::seal_at(
            SealRequest {
                project_path: &project,
                staged_composite: &composite,
                staged_zmap: &zmap,
                sources: &sources,
                algorithm: "laplacian",
                params: &values,
                align: None,
                registration: Registration::default(),
                composite_description: "golden".to_string(),
                label: None,
                metrics: Metrics::default(),
            },
            T0,
        )
        .expect("seal must succeed")
        .checkpoint;
        Fixture {
            dir,
            project,
            composite,
            zmap,
            sources,
            root,
        }
    }

    fn head(&self) -> String {
        Project::open(&self.project)
            .unwrap()
            .checkpoint()
            .id
            .clone()
    }

    /// A fused checkpoint over the same planes as the root, appended on the head.
    fn append_fused(&self, created_at: i64) -> String {
        let values = params();
        project::append_at(
            AppendRequest {
                project_path: &self.project,
                staged_composite: &self.composite,
                staged_zmap: &self.zmap,
                sources: &self.sources,
                algorithm: "laplacian",
                params: &values,
                align: None,
                registration: Registration::default(),
                composite_description: "golden".to_string(),
                label: None,
                metrics: Metrics::default(),
            },
            created_at,
        )
        .expect("append must succeed")
        .checkpoint
    }

    fn append_ops(&self, parent: &str, ops: Vec<Op>, created_at: i64) -> String {
        project::append_ops_onto(&self.project, parent, ops, None, created_at)
            .expect("append_ops_onto must succeed")
            .checkpoint
    }

    fn held(&self, created_at: i64) -> HeldCheckpoint {
        HeldCheckpoint {
            composite: self.composite.clone(),
            zmap: self.zmap.clone(),
            width: 512,
            height: 512,
            sources: self.sources.clone(),
            algorithm: "laplacian".to_string(),
            params: params(),
            align: None,
            registration: Registration::default(),
            composite_description: "golden".to_string(),
            metrics: Metrics::default(),
            created_at,
            compressed_composite: None,
        }
    }
}

fn fused_entry(held: HeldCheckpoint, parent: Option<ParentRef>) -> InMemoryCheckpoint {
    InMemoryCheckpoint {
        kind: InMemoryKind::Fused(held),
        parent,
    }
}

fn ops_entry(ops: Vec<Op>, created_at: i64, parent: ParentRef) -> InMemoryCheckpoint {
    InMemoryCheckpoint {
        kind: InMemoryKind::Ops { ops, created_at },
        parent: Some(parent),
    }
}

fn blob_name(hash: &str) -> String {
    format!("blobs/{hash}.zst")
}

/// The save left `before` untouched, wrote its records after it and exactly
/// one footer behind them. Returns the new index.
fn assert_pure_append(before: &[u8], old: &Index, path: &Path) -> Index {
    let after = std::fs::read(path).unwrap();
    assert!(after.len() > before.len(), "the file did not grow");
    assert!(&after[..before.len()] == before, "an existing byte changed");
    let new = Index::read(path).unwrap();
    assert_eq!(new.end(), after.len() as u64);
    let appended: u64 = new
        .entries()
        .filter(|&(_, offset, _)| offset >= old.end())
        .map(|(_, _, len)| len)
        .sum();
    assert_eq!(
        new.footer_start(),
        old.end() + appended,
        "the new footer follows the appended records directly"
    );
    new
}

fn no_partial_beside(path: &Path) {
    let partial = project::partial_path(path);
    assert!(!partial.exists(), "{} was left behind", partial.display());
}

#[test]
fn save_only_appends() {
    // A new project, written by `seal_in_memory` with a root and an ops entry.
    let dir = tmp_dir("container_new_project");
    let fx = Fixture::new("container_new_project_src");
    let target = dir.join("fresh.ffp");
    let ids = project::seal_in_memory(
        &target,
        &[
            fused_entry(fx.held(T0), None),
            ops_entry(stroke(100, 100), T0 + 5, ParentRef::Entry(0)),
        ],
        &[],
        None,
        false,
    )
    .unwrap();
    assert_eq!(ids.len(), 2);
    assert_eq!(Project::open(&target).unwrap().checkpoint().id, ids[1]);
    no_partial_beside(&target);
    let target_root = dir.join("root-only.ffp");
    project::seal_in_memory(
        &target_root,
        &[fused_entry(fx.held(T0), None)],
        &[],
        None,
        false,
    )
    .unwrap();
    Project::open(&target_root).unwrap();
    no_partial_beside(&target_root);
    no_partial_beside(&fx.project);

    // Fusion appended to an existing project.
    let fx = Fixture::new("container_save_fused");
    let before = std::fs::read(&fx.project).unwrap();
    let old = Index::read(&fx.project).unwrap();
    let fused = fx.append_fused(T0 + 10);
    assert_pure_append(&before, &old, &fx.project);
    assert_eq!(fx.head(), fused);

    // A multi-entry in-memory save with a delete, in one call.
    let fx = Fixture::new("container_save_in_memory");
    let a = fx.append_fused(T0 + 10);
    let before = std::fs::read(&fx.project).unwrap();
    let old = Index::read(&fx.project).unwrap();
    let ids = project::seal_in_memory(
        &fx.project,
        &[
            fused_entry(fx.held(T0 + 20), Some(ParentRef::Saved(a))),
            ops_entry(stroke(120, 90), T0 + 30, ParentRef::Entry(0)),
        ],
        &[ParentRef::Saved(fx.root.clone())],
        None,
        true,
    )
    .unwrap();
    assert_eq!(ids.len(), 2);
    let new = assert_pure_append(&before, &old, &fx.project);
    assert!(new.names().count() > old.names().count());
    let reopened = Project::open(&fx.project).unwrap();
    assert_eq!(reopened.checkpoint().id, ids[1]);
    assert!(reopened.history().get(&fx.root).unwrap().hidden);

    // Ops sealed onto a parent that is not the head.
    let fx = Fixture::new("container_save_ops_parent");
    let _head = fx.append_fused(T0 + 10);
    let before = std::fs::read(&fx.project).unwrap();
    let old = Index::read(&fx.project).unwrap();
    let branch = fx.append_ops(&fx.root.clone(), stroke(90, 90), T0 + 20);
    assert_pure_append(&before, &old, &fx.project);
    assert_eq!(fx.head(), branch);

    // A delete.
    let fx = Fixture::new("container_save_delete");
    let a = fx.append_fused(T0 + 10);
    let before = std::fs::read(&fx.project).unwrap();
    let old = Index::read(&fx.project).unwrap();
    let deleted = project::delete_checkpoint(&fx.project, &fx.root).unwrap();
    assert_pure_append(&before, &old, &fx.project);
    assert_eq!(fx.head(), a);
    assert_eq!(deleted.head, a);
}

#[test]
fn ops_save_appends_no_image() {
    let fx = Fixture::new("container_ops_no_image");
    let head = fx.append_fused(T0 + 10);
    let old = Index::read(&fx.project).unwrap();
    let composite = Project::open(&fx.project)
        .unwrap()
        .manifest()
        .composite
        .blob
        .clone();
    let (_, composite_len) = old.get(&blob_name(&composite)).unwrap();
    let old_len = std::fs::metadata(&fx.project).unwrap().len();

    let assert_no_image = |old: &Index, grew_from: u64| {
        let new = Index::read(&fx.project).unwrap();
        let grown = new.end() - grew_from;
        assert!(
            grown < composite_len,
            "the save grew the file by {grown} bytes, the composite blob is {composite_len}"
        );
        for (name, offset, _) in new.entries() {
            if offset >= old.end() && name.starts_with("blobs/") {
                assert!(
                    old.get(name).is_none(),
                    "{name} was appended although the project already held it"
                );
            }
        }
        new
    };

    fx.append_ops(&head, stroke(100, 100), T0 + 20);
    let after_first = assert_no_image(&old, old_len);

    let old_len = std::fs::metadata(&fx.project).unwrap().len();
    let head = fx.head();
    project::seal_in_memory(
        &fx.project,
        &[ops_entry(stroke(60, 140), T0 + 30, ParentRef::Saved(head))],
        &[],
        None,
        true,
    )
    .unwrap();
    assert_no_image(&after_first, old_len);
}

/// Every checkpoint with its hidden flag, sorted by id.
type Snapshot = Vec<(String, bool)>;

fn snapshot(project: &Project) -> Snapshot {
    let mut all: Snapshot = project
        .history()
        .all()
        .into_iter()
        .map(|c| (c.id.clone(), c.hidden))
        .collect();
    all.sort();
    all
}

#[test]
fn interrupted_append_keeps_previous_state() {
    let fx = Fixture::new("container_interrupted");
    let a = fx.append_fused(T0 + 10);
    fx.append_ops(&a, stroke(100, 100), T0 + 20);
    let before = std::fs::read(&fx.project).unwrap();
    let previous = Index::read(&fx.project).unwrap();
    let (previous_head, previous_state) = {
        let project = Project::open(&fx.project).unwrap();
        (project.checkpoint().id.clone(), snapshot(&project))
    };

    project::seal_in_memory(
        &fx.project,
        &[
            fused_entry(
                fx.held(T0 + 30),
                Some(ParentRef::Saved(previous_head.clone())),
            ),
            ops_entry(stroke(130, 80), T0 + 40, ParentRef::Entry(0)),
        ],
        &[ParentRef::Saved(a.clone())],
        None,
        true,
    )
    .unwrap();
    let full = std::fs::read(&fx.project).unwrap();
    let new = Index::read(&fx.project).unwrap();
    assert_eq!(&full[..before.len()], &before[..]);

    // What the same follow-up save produces on the uncut previous file.
    let control = fx.dir.join("control.ffp");
    std::fs::write(&control, &before).unwrap();
    project::append_ops_onto(&control, &previous_head, stroke(10, 10), None, T0 + 50).unwrap();
    let control_bytes = std::fs::read(&control).unwrap();

    let mut records: Vec<(u64, u64)> = new
        .entries()
        .filter(|&(_, offset, _)| offset >= previous.end())
        .map(|(_, offset, len)| (offset, len))
        .collect();
    records.sort();
    assert!(
        records.len() >= 4,
        "the save wrote {} records",
        records.len()
    );

    let end = full.len() as u64;
    let footer = new.footer_start();
    let mut cuts: Vec<u64> = vec![previous.end(), footer];
    for &(offset, len) in &records {
        cuts.extend([offset, offset + 1, offset + len / 2, offset + len - 1]);
    }
    let json_end = end - TRAILER_LEN;
    cuts.extend([
        footer + 1,
        footer + (json_end - footer) / 2,
        json_end - 1,
        json_end,
    ]);
    cuts.extend([
        json_end + 1,
        json_end + 20,
        json_end + 33,
        end - 9,
        end - 8,
        end - 1,
    ]);
    cuts.sort();
    cuts.dedup();
    cuts.retain(|&cut| cut >= previous.end() && cut < end);

    let cut_path = fx.dir.join("cut.ffp");
    let check = |bytes: Vec<u8>, what: &str| {
        std::fs::write(&cut_path, &bytes).unwrap();
        let mut project =
            Project::open(&cut_path).unwrap_or_else(|err| panic!("{what}: does not open: {err}"));
        assert_eq!(project.checkpoint().id, previous_head, "{what}");
        assert_eq!(snapshot(&project), previous_state, "{what}");
        project
            .read_plane_into(Plane::Composite, &mut |_, _| {})
            .unwrap_or_else(|err| panic!("{what}: composite: {err}"));
        project
            .read_plane_into(Plane::ZMap, &mut |_, _| {})
            .unwrap_or_else(|err| panic!("{what}: z-map: {err}"));
        drop(project);

        project::append_ops_onto(&cut_path, &previous_head, stroke(10, 10), None, T0 + 50)
            .unwrap_or_else(|err| panic!("{what}: the next save failed: {err}"));
        assert!(
            std::fs::read(&cut_path).unwrap() == control_bytes,
            "{what}: the next save did not cut the torn tail"
        );
    };

    for &cut in &cuts {
        check(full[..cut as usize].to_vec(), &format!("cut at {cut}"));
    }
    for at in [
        footer + 3,
        json_end - 2,
        json_end + 5,
        end - 20,
        end - 12,
        end - 3,
    ] {
        let mut flipped = full.clone();
        flipped[at as usize] ^= 0x55;
        check(flipped, &format!("byte {at} flipped"));
    }
}

#[test]
fn open_refuses_non_projects() {
    let dir = tmp_dir("container_non_projects");
    let fx = Fixture::new("container_non_projects_src");
    let real = std::fs::read(&fx.project).unwrap();
    let tiff = std::fs::read(common::mantis_512_dir().join("frames").join("00.tif")).unwrap();

    let not_a_project: [(&str, Vec<u8>); 4] = [
        ("empty", Vec::new()),
        ("short", real[..(HEADER_LEN as usize - 1)].to_vec()),
        ("tiff", tiff),
        (
            "zip",
            b"PK\x03\x04\x14\x00\x00\x00\x00\x00 an old zip project".to_vec(),
        ),
    ];
    for (name, bytes) in not_a_project {
        let path = dir.join(format!("{name}.ffp"));
        std::fs::write(&path, bytes).unwrap();
        match Project::open(&path) {
            Err(ProjectError::NotAProject { path: named }) => assert_eq!(named, path, "{name}"),
            Err(other) => panic!("{name}: expected NotAProject, got {other}"),
            Ok(_) => panic!("{name}: opened"),
        }
    }

    let truncated: [(&str, Vec<u8>); 2] = [
        ("header-only", real[..HEADER_LEN as usize].to_vec()),
        ("one-byte-short", real[..real.len() - 1].to_vec()),
    ];
    for (name, bytes) in truncated {
        let path = dir.join(format!("{name}.ffp"));
        std::fs::write(&path, bytes).unwrap();
        match Project::open(&path) {
            Err(ProjectError::Truncated { path: named }) => assert_eq!(named, path, "{name}"),
            Err(other) => panic!("{name}: expected Truncated, got {other}"),
            Ok(_) => panic!("{name}: opened"),
        }
        let message = Project::open(&path).err().unwrap().to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
    }
}

/// Yields a few bytes, then fails.
struct FailsAfter(usize);

impl Read for FailsAfter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.0 == 0 {
            return Err(std::io::Error::other("the source failed"));
        }
        let n = self.0.min(buf.len()).min(16);
        buf[..n].fill(7);
        self.0 -= n;
        Ok(n)
    }
}

#[test]
fn failed_save_reports_and_keeps_state() {
    let fx = Fixture::new("container_failed_save");
    let head = fx.append_fused(T0 + 10);
    let before = std::fs::read(&fx.project).unwrap();

    // A project the process cannot write to.
    let mut permissions = std::fs::metadata(&fx.project).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&fx.project, permissions.clone()).unwrap();
    let outcome = project::seal_in_memory(
        &fx.project,
        &[ops_entry(
            stroke(100, 100),
            T0 + 20,
            ParentRef::Saved(head.clone()),
        )],
        &[],
        None,
        true,
    );
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&fx.project, permissions).unwrap();
    match outcome {
        Err(ProjectError::Io { path, .. }) => assert_eq!(path, fx.project),
        Err(other) => panic!("expected an Io error naming the project, got {other}"),
        Ok(_) => panic!("a read-only project was saved"),
    }
    assert!(std::fs::read(&fx.project).unwrap() == before);
    assert_eq!(fx.head(), head);

    // A record whose source fails partway.
    let index = Index::read(&fx.project).unwrap();
    let mut writer = ContainerWriter::append(&fx.project, &index).unwrap();
    writer.put_bytes("checkpoints/first.json", b"{}").unwrap();
    let failure = writer.put("blobs/second.zst", &mut FailsAfter(40_000));
    assert!(failure.is_err(), "a failing source went unreported");
    drop(writer);
    assert!(std::fs::read(&fx.project).unwrap() == before);
    assert_eq!(fx.head(), head);
}

#[test]
fn delete_appends_hide_record() {
    let fx = Fixture::new("container_delete");
    let o1 = fx.append_ops(&fx.root.clone(), stroke(100, 100), T0 + 10);
    let o2 = fx.append_ops(&o1, stroke(140, 100), T0 + 20);

    // O1 has a visible descendant: it is hidden, nothing is dropped.
    let hidden = project::delete_checkpoint(&fx.project, &o1).unwrap();
    assert!(hidden.removed.is_empty());
    assert!(
        Project::open(&fx.project)
            .unwrap()
            .history()
            .get(&o1)
            .unwrap()
            .hidden
    );

    // O2 goes, and O1 with it: nothing visible is built on either.
    let before = std::fs::read(&fx.project).unwrap();
    let old = Index::read(&fx.project).unwrap();
    let deleted = project::delete_checkpoint(&fx.project, &o2).unwrap();
    let new = assert_pure_append(&before, &old, &fx.project);

    let mut removed = deleted.removed.clone();
    removed.sort();
    let mut expected = vec![o1.clone(), o2.clone()];
    expected.sort();
    assert_eq!(removed, expected);
    assert_eq!(deleted.head, fx.root);

    let project = Project::open(&fx.project).unwrap();
    assert_eq!(project.checkpoint().id, fx.root);
    let ids: Vec<String> = project
        .history()
        .all()
        .iter()
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(ids, vec![fx.root.clone()]);

    let dropped: Vec<(&str, u64)> = old
        .entries()
        .filter(|&(name, _, _)| new.get(name).is_none())
        .map(|(name, _, len)| (name, len))
        .collect();
    let dropped_checkpoints: Vec<&str> = dropped
        .iter()
        .map(|&(name, _)| name)
        .filter(|name| name.starts_with("checkpoints/"))
        .collect();
    assert_eq!(dropped_checkpoints.len(), 2, "{dropped:?}");
    for id in &removed {
        assert!(new.get(&format!("checkpoints/{id}.json")).is_none());
    }
    assert!(
        dropped.iter().all(|(name, _)| !name.starts_with("blobs/")),
        "both op checkpoints name the root's Z-Map, so no blob is dropped: {dropped:?}"
    );
    let dropped_len: u64 = dropped.iter().map(|&(_, len)| len).sum();
    assert!(
        new.dead_bytes() >= old.dead_bytes() + dropped_len,
        "dead bytes {} -> {}, dropped {dropped_len}",
        old.dead_bytes(),
        new.dead_bytes()
    );
}

/// Appends a record of `len` zero bytes to the project, then stops listing it.
fn append_dead_record(path: &Path, len: u64) {
    let index = Index::read(path).unwrap();
    let mut writer = ContainerWriter::append(path, &index).unwrap();
    writer
        .put("filler.bin", &mut std::io::repeat(0).take(len))
        .unwrap();
    writer.drop_entry("filler.bin");
    writer.finish().unwrap();
}

/// A container with a 100-byte live record whose `filler` bytes are dead.
fn filler_container(path: &Path, filler: u64) -> Index {
    let mut writer = ContainerWriter::create(path).unwrap();
    writer.put_bytes("live.bin", &[7; 100]).unwrap();
    writer.finish().unwrap();
    append_dead_record(path, filler);
    Index::read(path).unwrap()
}

/// The filler size at which exactly half of the committed file is dead.
fn half_dead_filler(dir: &Path) -> u64 {
    let probe = dir.join("probe.ffp");
    let mut filler = 2000u64;
    for _ in 0..8 {
        let index = filler_container(&probe, filler);
        let excess = index.dead_bytes() as i64 * 2 - index.end() as i64;
        if excess == 0 {
            return filler;
        }
        filler = filler.checked_add_signed(-excess).unwrap();
    }
    panic!("no filler size puts exactly half of the file into dead bytes");
}

#[test]
fn auto_compaction_threshold() {
    let dir = tmp_dir("container_threshold");
    let half = half_dead_filler(&dir);

    let at_half = dir.join("half.ffp");
    let index = filler_container(&at_half, half);
    assert_eq!(index.dead_bytes() * 2, index.end());
    assert!(!index.mostly_dead(), "exactly half dead is not over half");
    let before = std::fs::read(&at_half).unwrap();
    assert_eq!(
        project::container::compact_if_mostly_dead(&at_half).unwrap(),
        None
    );
    assert!(
        std::fs::read(&at_half).unwrap() == before,
        "a file at the threshold was rewritten"
    );

    let over = dir.join("over.ffp");
    assert!(filler_container(&over, half + 1).mostly_dead());

    // A real project made mostly dead; a save and a delete leave it as it is.
    let fx = Fixture::new("container_threshold_project");
    let o1 = fx.append_ops(&fx.root.clone(), stroke(100, 100), T0 + 10);
    append_dead_record(&fx.project, 8 << 20);
    assert!(Index::read(&fx.project).unwrap().mostly_dead());

    let before = std::fs::read(&fx.project).unwrap();
    project::seal_in_memory(
        &fx.project,
        &[ops_entry(
            stroke(120, 90),
            T0 + 20,
            ParentRef::Saved(o1.clone()),
        )],
        &[],
        None,
        true,
    )
    .unwrap();
    let saved = std::fs::read(&fx.project).unwrap();
    assert!(saved.len() > before.len() && saved[..before.len()] == before[..]);
    assert!(Index::read(&fx.project).unwrap().mostly_dead());

    project::delete_checkpoint(&fx.project, &o1).unwrap();
    let deleted = std::fs::read(&fx.project).unwrap();
    assert!(deleted[..saved.len()] == saved[..], "a delete moved bytes");
    let state = snapshot(&Project::open(&fx.project).unwrap());

    let reclaimed = project::container::compact_if_mostly_dead(&fx.project)
        .unwrap()
        .expect("a mostly dead file is compacted");
    let compacted = std::fs::read(&fx.project).unwrap();
    assert_eq!(reclaimed, (deleted.len() - compacted.len()) as u64);
    assert!(reclaimed > 8 << 20);
    assert_eq!(Index::read(&fx.project).unwrap().dead_bytes(), 0);
    assert_eq!(snapshot(&Project::open(&fx.project).unwrap()), state);
    no_partial_beside(&fx.project);
    assert_eq!(
        project::container::compact_if_mostly_dead(&fx.project).unwrap(),
        None
    );
    assert!(std::fs::read(&fx.project).unwrap() == compacted);
}

#[cfg(unix)]
#[test]
fn interrupted_compaction_keeps_project() {
    let fx = Fixture::new("container_interrupted_compaction");
    fx.append_ops(&fx.root.clone(), stroke(100, 100), T0 + 10);
    append_dead_record(&fx.project, 1 << 20);
    let before = std::fs::read(&fx.project).unwrap();
    let head = fx.head();

    // Writes into the sibling file fail with "no space left on device".
    std::os::unix::fs::symlink("/dev/full", project::partial_path(&fx.project)).unwrap();
    let err =
        project::container::compact(&fx.project).expect_err("the sibling file cannot be written");
    assert!(matches!(err, ProjectError::Io { .. }), "{err}");

    assert!(
        std::fs::read(&fx.project).unwrap() == before,
        "the project changed"
    );
    assert_eq!(fx.head(), head);
    no_partial_beside(&fx.project);
}

#[test]
fn compressed_composite_is_appended_without_the_tiff() {
    let fx = Fixture::new("container_compressed_composite");
    let held_dir = fx.dir.join("held");
    std::fs::create_dir_all(&held_dir).unwrap();
    let blob = project::blob::stage_plane(
        &fx.composite,
        Plane::Composite,
        project::blob::ZSTD_LEVEL,
        &held_dir,
    )
    .unwrap();
    let mut held = fx.held(T0 + 10);
    held.composite = held_dir.join("composite.tif");
    held.compressed_composite = Some(Box::new(blob.clone()));
    assert!(!held.composite.exists());

    for (name, onto_existing) in [("new.ffp", false), ("existing.ffp", true)] {
        let target = fx.dir.join(name);
        let parent = if onto_existing {
            std::fs::copy(&fx.project, &target).unwrap();
            Some(ParentRef::Saved(fx.root.clone()))
        } else {
            None
        };
        let entry = fused_entry(held.clone(), parent);
        project::seal_in_memory(&target, &[entry], &[], None, onto_existing)
            .unwrap_or_else(|err| panic!("{name}: {err}"));
        let index = Index::read(&target).unwrap();
        assert_eq!(
            index.read_entry(&target, &blob_name(&blob.hash)).unwrap(),
            std::fs::read(&blob.path).unwrap(),
            "{name}: the record is the staged file"
        );
        assert!(blob.path.exists(), "{name}: the held blob is not consumed");
    }
}
