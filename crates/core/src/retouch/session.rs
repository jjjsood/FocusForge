// SPDX-License-Identifier: GPL-3.0-or-later

//! An in-memory undo/redo session over one resident tile of a Z-Map's
//! indices. State is always exactly the base plus the applied op list
//! replayed on top; undo and redo are an inverse-patch log rather than a
//! replay, so a step costs one region, not the whole list.

use crate::project::Op;
use crate::tile::Rect;
use crate::zmap::ZMapInfo;

use super::RetouchError;
use super::brush;

type FootprintFn = fn(&Op, ZMapInfo) -> Result<Option<Rect>, RetouchError>;
type ApplyFn = fn(&Op, ZMapInfo, Rect, &mut [u16]) -> Result<u64, RetouchError>;

/// One applied op: its dirty region (within the session's tile) and the
/// region's pre-image, so undo can restore it without replaying anything
/// earlier.
#[derive(Debug)]
struct Applied {
    op: Op,
    region: Option<Rect>,
    before: Vec<u16>,
}

/// One outstanding preview: the same region/pre-image pair as [`Applied`],
/// minus the op itself -- a preview is never undone or redone, only
/// discarded.
#[derive(Debug)]
struct PreviewEntry {
    region: Option<Rect>,
    before: Vec<u16>,
}

/// An edit session over one caller-chosen resident tile -- the whole map or
/// any sub-rect. Dies with the process: nothing here is durable.
#[derive(Debug)]
pub struct EditSession {
    info: ZMapInfo,
    tile: Rect,
    indices: Vec<u16>,
    footprint: FootprintFn,
    apply: ApplyFn,
    applied: Vec<Applied>,
    undone: Vec<Op>,
    /// Outstanding previews, oldest first -- each op a live stroke ticked
    /// through [`Self::preview`] before release, restored in reverse by
    /// [`Self::discard_preview`]. Never touched by undo or redo.
    preview: Vec<PreviewEntry>,
}

impl EditSession {
    /// Refuses a `tile` outside `info`'s extent
    /// ([`RetouchError::TileOutsideMap`]) or an `indices` length that does
    /// not match `tile`'s area ([`RetouchError::TileBufferSize`]).
    pub fn new(info: ZMapInfo, tile: Rect, indices: Vec<u16>) -> Result<Self, RetouchError> {
        Self::with_applier(info, tile, indices, brush::footprint, brush::apply)
    }

    fn with_applier(
        info: ZMapInfo,
        tile: Rect,
        indices: Vec<u16>,
        footprint: FootprintFn,
        apply: ApplyFn,
    ) -> Result<Self, RetouchError> {
        if !info.contains(tile) {
            return Err(RetouchError::TileOutsideMap { tile });
        }
        let expected = info.tile_len(tile);
        if indices.len() != expected {
            return Err(RetouchError::TileBufferSize {
                expected,
                actual: indices.len(),
            });
        }
        Ok(EditSession {
            info,
            tile,
            indices,
            footprint,
            apply,
            applied: Vec::new(),
            undone: Vec::new(),
            preview: Vec::new(),
        })
    }

    pub fn indices(&self) -> &[u16] {
        &self.indices
    }

    pub fn tile(&self) -> Rect {
        self.tile
    }

    pub fn info(&self) -> ZMapInfo {
        self.info
    }

    pub fn op_count(&self) -> usize {
        self.applied.len()
    }

    /// Makes the current map the session's new origin: outstanding previews
    /// are discarded, then every applied and undone op is forgotten, so
    /// nothing is left to undo or redo and [`Self::op_count`] is 0. The
    /// indices and tile are kept.
    pub fn rebase(&mut self) {
        self.discard_preview();
        self.applied.clear();
        self.undone.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.applied.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// Applied ops, oldest first -- undone ops are not included.
    pub fn ops(&self) -> impl Iterator<Item = &Op> {
        self.applied.iter().map(|applied| &applied.op)
    }

    /// Applies `op` against the session's own footprint/apply pair,
    /// returning its dirty region and the region's pre-image. Every refusal
    /// happens before the first write (the applier's own contract), so
    /// `indices` is untouched on `Err`. Shared by [`Self::apply_op`]
    /// (permanent) and [`Self::preview`] (undone by
    /// [`Self::discard_preview`]).
    fn apply_and_snapshot(&mut self, op: &Op) -> Result<(Option<Rect>, Vec<u16>), RetouchError> {
        let footprint = (self.footprint)(op, self.info)?;
        let region = footprint.and_then(|f| brush::intersect(f, self.tile));
        let before = region.map(|r| self.extract(r)).unwrap_or_default();
        (self.apply)(op, self.info, self.tile, &mut self.indices)?;
        Ok((region, before))
    }

    /// Applies `op` permanently, recording its dirty region and pre-image on
    /// `applied`.
    fn apply_op(&mut self, op: Op) -> Result<Option<Rect>, RetouchError> {
        let (region, before) = self.apply_and_snapshot(&op)?;
        self.applied.push(Applied { op, region, before });
        Ok(region)
    }

    /// Appends `op`. Discards an outstanding preview first, so a preview
    /// never outlives the next op. On success, discards the redo stack
    /// regardless of which of the four [`Op`] variants `op` is. On `Err`,
    /// the applied list, the redo stack and `indices` are exactly as before.
    pub fn append(&mut self, op: Op) -> Result<Option<Rect>, RetouchError> {
        self.discard_preview();
        let region = self.apply_op(op)?;
        self.undone.clear();
        Ok(region)
    }

    /// Steps back one op, restoring its region's pre-image. Discards an
    /// outstanding preview first. `None` means there was nothing applied to
    /// undo.
    pub fn undo(&mut self) -> Option<Option<Rect>> {
        self.discard_preview();
        let applied = self.applied.pop()?;
        if let Some(region) = applied.region {
            self.restore(region, &applied.before);
        }
        self.undone.push(applied.op);
        Some(applied.region)
    }

    /// Re-applies the most recently undone op. Discards an outstanding
    /// preview first. `None` means there was nothing undone to redo. Does
    /// not clear the redo stack -- it empties only by exhaustion or by a new
    /// [`Self::append`].
    pub fn redo(&mut self) -> Option<Option<Rect>> {
        self.discard_preview();
        let op = self.undone.pop()?;
        let region = self
            .apply_op(op)
            .expect("an op that applied to this state before applies to it again");
        Some(region)
    }

    /// Applies `op` without recording it on the permanent op list: pushes
    /// its region/pre-image pair onto a separate preview stack instead of
    /// `applied`, leaving `undone` untouched. [`Self::discard_preview`]
    /// restores every outstanding preview in LIFO order; [`Self::append`],
    /// [`Self::undo`] and [`Self::redo`] each discard one first.
    pub fn preview(&mut self, op: &Op) -> Result<Option<Rect>, RetouchError> {
        let (region, before) = self.apply_and_snapshot(op)?;
        self.preview.push(PreviewEntry { region, before });
        Ok(region)
    }

    /// Restores every outstanding preview, most recent first, undoing them
    /// exactly as if they had never been applied. A no-op with nothing
    /// outstanding.
    pub fn discard_preview(&mut self) {
        while let Some(entry) = self.preview.pop() {
            if let Some(region) = entry.region {
                self.restore(region, &entry.before);
            }
        }
    }

    /// Copies `region`'s rows out of `self.indices` (tile-local, row-major).
    fn extract(&self, region: Rect) -> Vec<u16> {
        let mut out = Vec::with_capacity(self.info.tile_len(region));
        for y in region.y..region.y + region.height {
            let row_start =
                ((y - self.tile.y) * self.tile.width + (region.x - self.tile.x)) as usize;
            out.extend_from_slice(&self.indices[row_start..row_start + region.width as usize]);
        }
        out
    }

    /// Writes `before` back over `region`'s rows.
    fn restore(&mut self, region: Rect, before: &[u16]) {
        let width = region.width as usize;
        for (row, y) in (region.y..region.y + region.height).enumerate() {
            let row_start =
                ((y - self.tile.y) * self.tile.width + (region.x - self.tile.x)) as usize;
            self.indices[row_start..row_start + width]
                .copy_from_slice(&before[row * width..(row + 1) * width]);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::project::{Op, Point};
    use crate::retouch::EditSession;
    use crate::retouch::RetouchError;
    use crate::tile::Rect;
    use crate::zmap::ZMapInfo;

    const WIDTH: u32 = 37;
    const HEIGHT: u32 = 23;
    const FRAMES: u32 = 10;

    fn info() -> ZMapInfo {
        ZMapInfo::new(WIDTH, HEIGHT, FRAMES).unwrap()
    }

    /// A non-8-aligned sub-rect, so region clipping to the tile is exercised.
    fn tile() -> Rect {
        Rect {
            x: 5,
            y: 3,
            width: 29,
            height: 17,
        }
    }

    fn pt(x: i32, y: i32) -> Point {
        Point { x, y }
    }

    fn stroke_op(source_id: u16, path: Vec<Point>, radius: f32, softness: f32) -> Op {
        Op::BrushStroke {
            source_id,
            path,
            radius,
            softness,
        }
    }

    fn base_indices(info: ZMapInfo, tile: Rect) -> Vec<u16> {
        (0..info.tile_len(tile))
            .map(|i| (i as u32 % info.frame_count()) as u16)
            .collect()
    }

    fn replay(base: &[u16], info: ZMapInfo, tile: Rect, ops: &[Op]) -> Vec<u16> {
        let mut buf = base.to_vec();
        for op in ops {
            crate::retouch::brush::apply(op, info, tile, &mut buf).unwrap();
        }
        buf
    }

    #[test]
    fn session_state_equals_replay() {
        let info = info();
        let tile = tile();
        let base = base_indices(info, tile);
        let ops = vec![
            // Crosses the tile's left edge (tile starts at x = 5).
            stroke_op(2, vec![pt(0, 0), pt(10, 10)], 3.0, 0.4),
            // Wholly off-map.
            stroke_op(5, vec![pt(-50, -50)], 2.0, 0.0),
            // Wholly inside the tile.
            stroke_op(7, vec![pt(20, 10), pt(25, 15)], 4.0, 0.6),
        ];
        let mut session = EditSession::new(info, tile, base.clone()).unwrap();
        for op in &ops {
            session.append(op.clone()).unwrap();
        }
        let expected = replay(&base, info, tile, &ops);
        assert_eq!(session.indices(), expected.as_slice());
    }

    #[test]
    fn undo_is_unlimited_and_exact() {
        let info = info();
        let tile = tile();
        let base = base_indices(info, tile);
        let mut session = EditSession::new(info, tile, base.clone()).unwrap();
        let n = 5_000;
        let mut history = Vec::with_capacity(n + 1);
        history.push(base.clone());
        for i in 0..n {
            let op = stroke_op(
                (i % 9) as u16,
                vec![pt(5 + (i % 20) as i32, 3 + (i % 10) as i32)],
                1.5,
                0.3,
            );
            session.append(op).unwrap();
            history.push(session.indices().to_vec());
        }
        for k in 1..=n {
            session.undo().expect("n ops remain to undo");
            assert_eq!(session.indices(), history[n - k].as_slice());
        }
        assert!(!session.can_undo());
        assert_eq!(session.indices(), base.as_slice());
    }

    #[test]
    fn rebase_keeps_the_map_and_forgets_undo_and_redo() {
        let info = info();
        let tile = tile();
        let base = base_indices(info, tile);
        let mut session = EditSession::new(info, tile, base).unwrap();
        session
            .append(stroke_op(1, vec![pt(6, 4)], 2.0, 0.0))
            .unwrap();
        session
            .append(stroke_op(2, vec![pt(12, 9)], 2.0, 0.0))
            .unwrap();
        session.undo().unwrap();
        let before = session.indices().to_vec();

        session.rebase();

        assert_eq!(session.indices(), before.as_slice());
        assert_eq!(session.tile(), tile);
        assert_eq!(session.op_count(), 0);
        assert!(!session.can_undo() && !session.can_redo());

        session
            .append(stroke_op(3, vec![pt(20, 14)], 2.0, 0.0))
            .unwrap();
        assert_ne!(session.indices(), before.as_slice());
        session.undo().unwrap();
        assert_eq!(session.indices(), before.as_slice());
        assert!(session.undo().is_none(), "undo stops at the rebased map");
    }

    #[test]
    fn redo_restores_and_exhausts() {
        let info = info();
        let tile = tile();
        let base = base_indices(info, tile);
        let mut session = EditSession::new(info, tile, base.clone()).unwrap();
        let ops = vec![
            stroke_op(1, vec![pt(6, 4)], 1.0, 0.0),
            stroke_op(2, vec![pt(10, 8)], 1.5, 0.2),
            stroke_op(3, vec![pt(15, 12)], 2.0, 0.5),
        ];
        let mut history = vec![base.clone()];
        for op in &ops {
            session.append(op.clone()).unwrap();
            history.push(session.indices().to_vec());
        }
        session.undo().unwrap();
        session.undo().unwrap();
        session.undo().unwrap();
        assert_eq!(session.indices(), history[0].as_slice());
        session.redo().unwrap();
        assert_eq!(session.indices(), history[1].as_slice());
        session.redo().unwrap();
        assert_eq!(session.indices(), history[2].as_slice());
        session.redo().unwrap();
        assert_eq!(session.indices(), history[3].as_slice());
        assert!(!session.can_redo());
        assert!(session.redo().is_none());
    }

    #[test]
    fn append_discards_redo_and_only_append_does() {
        let info = info();
        let tile = tile();
        let base = base_indices(info, tile);
        let mut session = EditSession::new(info, tile, base.clone()).unwrap();
        let op1 = stroke_op(1, vec![pt(6, 4)], 1.0, 0.0);
        let op2 = stroke_op(2, vec![pt(10, 8)], 1.5, 0.2);
        let op3 = stroke_op(3, vec![pt(15, 12)], 2.0, 0.5);
        session.append(op1).unwrap();
        session.append(op2).unwrap();
        session.append(op3).unwrap();
        session.undo().unwrap();
        session.undo().unwrap();
        assert!(session.can_redo());
        let op_count_before = session.op_count();
        let indices_before = session.indices().to_vec();

        // A rejected append (source_id out of range for a 10-frame map)
        // leaves the applied list, the redo stack and the indices untouched.
        let bad = stroke_op(99, vec![pt(0, 0)], 1.0, 0.0);
        let err = session.append(bad).unwrap_err();
        assert!(matches!(err, RetouchError::SourceOutOfRange { .. }));
        assert_eq!(session.op_count(), op_count_before);
        assert!(session.can_redo());
        assert_eq!(session.indices(), indices_before.as_slice());

        // Redo still walks forward exactly as before the rejected append.
        session.redo().unwrap();
        session.redo().unwrap();
        assert!(!session.can_redo());

        // A successful append after an undo discards whatever redo remained.
        session.undo().unwrap();
        assert!(session.can_redo());
        let op4 = stroke_op(4, vec![pt(20, 15)], 1.0, 0.0);
        session.append(op4).unwrap();
        assert!(!session.can_redo());
        assert!(session.redo().is_none());
    }

    fn stub_footprint(_op: &Op, _info: ZMapInfo) -> Result<Option<Rect>, RetouchError> {
        Ok(Some(Rect {
            x: 5,
            y: 3,
            width: 1,
            height: 1,
        }))
    }

    fn stub_apply(
        _op: &Op,
        _info: ZMapInfo,
        _tile: Rect,
        indices: &mut [u16],
    ) -> Result<u64, RetouchError> {
        if let Some(v) = indices.first_mut() {
            *v = v.wrapping_add(1);
        }
        Ok(1)
    }

    #[test]
    fn every_op_variant_discards_redo() {
        let info = info();
        let tile = tile();
        let first = stroke_op(0, vec![pt(6, 4)], 1.0, 0.0);
        let variants = [
            stroke_op(0, vec![pt(6, 4)], 1.0, 0.0),
            Op::FloodFill {
                seed: pt(6, 4),
                source_id: 0,
                tolerance: 0.1,
            },
            Op::ResetRegion { rect: tile },
            Op::Refuse {
                algorithm: String::new(),
                params: crate::params::ParamValues::default(),
            },
        ];
        for second in &variants {
            let base = base_indices(info, tile);
            let mut session =
                EditSession::with_applier(info, tile, base, stub_footprint, stub_apply).unwrap();
            session.append(first.clone()).unwrap();
            session.undo().unwrap();
            assert!(session.can_redo());
            session.append(second.clone()).unwrap();
            assert!(
                !session.can_redo(),
                "appending {second:?} did not discard redo"
            );
        }
    }

    #[test]
    fn queries_match_behaviour() {
        let info = info();
        let tile = tile();
        let base = base_indices(info, tile);
        let mut session = EditSession::new(info, tile, base.clone()).unwrap();
        assert!(!session.can_undo());
        assert!(session.undo().is_none());
        assert!(!session.can_redo());
        assert!(session.redo().is_none());

        let op = stroke_op(1, vec![pt(10, 10)], 1.0, 0.0);
        session.append(op).unwrap();
        assert!(session.can_undo());
        assert!(session.undo().is_some());
        assert!(!session.can_undo());
        assert!(session.undo().is_none());
        assert!(session.can_redo());
        assert!(session.redo().is_some());
        assert!(!session.can_redo());
        assert!(session.redo().is_none());
    }

    #[test]
    fn new_refuses_a_mismatched_tile() {
        let info = info();
        let outside = Rect {
            x: 0,
            y: 0,
            width: info.width() + 1,
            height: info.height(),
        };
        let err = EditSession::new(info, outside, vec![0u16; 1]).unwrap_err();
        assert!(matches!(err, RetouchError::TileOutsideMap { .. }));

        let tile = tile();
        let wrong_len = vec![0u16; info.tile_len(tile) + 1];
        let err = EditSession::new(info, tile, wrong_len).unwrap_err();
        assert!(matches!(err, RetouchError::TileBufferSize { .. }));
    }

    #[test]
    fn ops_lists_applied_oldest_first_and_drops_undone() {
        let info = info();
        let tile = tile();
        let ops = [
            stroke_op(1, vec![pt(8, 5)], 2.0, 0.0),
            stroke_op(2, vec![pt(12, 8), pt(20, 12)], 3.0, 0.5),
            stroke_op(3, vec![pt(30, 18)], 1.5, 0.2),
        ];
        let mut session = EditSession::new(info, tile, base_indices(info, tile)).unwrap();
        assert_eq!(session.ops().count(), 0);
        for op in &ops {
            session.append(op.clone()).unwrap();
        }
        assert_eq!(session.ops().cloned().collect::<Vec<_>>(), ops.to_vec());
        session.undo();
        assert_eq!(
            session.ops().cloned().collect::<Vec<_>>(),
            ops[..2].to_vec()
        );
        session.redo();
        assert_eq!(session.ops().cloned().collect::<Vec<_>>(), ops.to_vec());
    }

    /// Everything a preview must leave alone, captured for comparison.
    fn snapshot(session: &EditSession) -> (Vec<u16>, Vec<Op>, bool, bool) {
        (
            session.indices().to_vec(),
            session.ops().cloned().collect(),
            session.can_undo(),
            session.can_redo(),
        )
    }

    /// Two overlapping preview segments crossing the tile's left and top
    /// edges, joined at a shared point as a live stroke's ticks are.
    fn preview_segments() -> [Op; 2] {
        [
            stroke_op(9, vec![pt(2, 1), pt(14, 9)], 3.0, 0.3),
            stroke_op(9, vec![pt(14, 9), pt(22, 4)], 3.0, 0.3),
        ]
    }

    #[test]
    fn preview_then_discard_restores_exactly() {
        let info = info();
        let tile = tile();
        let mut session = EditSession::new(info, tile, base_indices(info, tile)).unwrap();
        session
            .append(stroke_op(4, vec![pt(10, 10), pt(16, 12)], 4.0, 0.0))
            .unwrap();
        session
            .append(stroke_op(6, vec![pt(24, 6)], 3.0, 0.0))
            .unwrap();
        session.undo();
        let before = snapshot(&session);
        assert!(before.3, "the redo stack is non-empty going in");

        let mut painted = session.indices().to_vec();
        for op in &preview_segments() {
            let region = session.preview(op).unwrap();
            assert!(region.is_some(), "each segment touches the tile");
            crate::retouch::brush::apply(op, info, tile, &mut painted).unwrap();
            assert_eq!(session.indices(), painted.as_slice());
            assert_eq!(session.ops().count(), before.1.len());
            assert!(session.can_redo(), "a preview leaves the redo stack alone");
        }
        assert_ne!(session.indices(), before.0.as_slice());

        session.discard_preview();
        assert_eq!(snapshot(&session), before);
        session.discard_preview();
        assert_eq!(snapshot(&session), before, "a second discard is a no-op");
    }

    #[test]
    fn preview_is_discarded_before_append_undo_and_redo() {
        let info = info();
        let tile = tile();
        let first = stroke_op(4, vec![pt(10, 10), pt(16, 12)], 4.0, 0.0);
        let second = stroke_op(6, vec![pt(12, 11)], 3.0, 0.0);

        // Each case: the session with a preview outstanding must end exactly
        // where the same call leaves a session that never previewed.
        type Call = fn(&mut EditSession, &Op);
        let cases: [(&str, Call); 3] = [
            ("append", |s, op| {
                s.append(op.clone()).unwrap();
            }),
            ("undo", |s, _| {
                s.undo();
            }),
            ("redo", |s, _| {
                s.redo();
            }),
        ];
        for (name, call) in cases {
            let mut plain = EditSession::new(info, tile, base_indices(info, tile)).unwrap();
            plain.append(first.clone()).unwrap();
            if name == "redo" {
                plain.append(second.clone()).unwrap();
                plain.undo();
            }
            let mut previewed = EditSession::new(info, tile, base_indices(info, tile)).unwrap();
            previewed.append(first.clone()).unwrap();
            if name == "redo" {
                previewed.append(second.clone()).unwrap();
                previewed.undo();
            }
            for op in &preview_segments() {
                previewed.preview(op).unwrap();
            }

            call(&mut plain, &second);
            call(&mut previewed, &second);
            assert_eq!(snapshot(&previewed), snapshot(&plain), "{name}");

            // Nothing of the preview is left for a later discard to restore.
            previewed.discard_preview();
            assert_eq!(snapshot(&previewed), snapshot(&plain), "{name}: discard");
        }
    }

    /// Append a random valid stroke, or undo, or redo.
    #[derive(Debug, Clone)]
    enum Action {
        Append(Op),
        Undo,
        Redo,
    }

    fn arb_point() -> impl proptest::strategy::Strategy<Value = Point> {
        use proptest::prelude::*;
        (-10i32..WIDTH as i32 + 10, -10i32..HEIGHT as i32 + 10).prop_map(|(x, y)| pt(x, y))
    }

    fn arb_stroke() -> impl proptest::strategy::Strategy<Value = Op> {
        use proptest::prelude::*;
        (
            0u16..FRAMES as u16,
            prop::collection::vec(arb_point(), 1..=4),
            0.5f32..6.0,
            0.0f32..=1.0,
        )
            .prop_map(|(source_id, path, radius, softness)| {
                stroke_op(source_id, path, radius, softness)
            })
    }

    fn arb_action() -> impl proptest::strategy::Strategy<Value = Action> {
        use proptest::prelude::*;
        prop_oneof![
            arb_stroke().prop_map(Action::Append),
            Just(Action::Undo),
            Just(Action::Redo),
        ]
    }

    /// Asserts that every pixel that differs between `before` and `after`
    /// lies inside `region` (absolute map coordinates, `tile`-local
    /// buffers) -- containment, not tightness.
    fn assert_contained(
        before: &[u16],
        after: &[u16],
        tile: Rect,
        region: Option<Rect>,
    ) -> Result<(), proptest::test_runner::TestCaseError> {
        use proptest::prop_assert;
        for y in 0..tile.height {
            for x in 0..tile.width {
                let idx = (y * tile.width + x) as usize;
                if before[idx] != after[idx] {
                    let abs_x = tile.x + x;
                    let abs_y = tile.y + y;
                    let inside = region.is_some_and(|r| {
                        abs_x >= r.x
                            && abs_x < r.x + r.width
                            && abs_y >= r.y
                            && abs_y < r.y + r.height
                    });
                    prop_assert!(
                        inside,
                        "pixel ({abs_x}, {abs_y}) changed outside reported region {region:?}"
                    );
                }
            }
        }
        Ok(())
    }

    /// Drives `actions` through an edit session alongside a model, checking
    /// after every action that the session's queries agree with the model,
    /// that its indices equal a full replay of the model's applied list,
    /// that every changed pixel lies within the transition's reported dirty
    /// region, and that an undo or redo with nothing to step over leaves
    /// the indices untouched.
    fn check_sequence(actions: &[Action]) -> Result<(), proptest::test_runner::TestCaseError> {
        use proptest::prop_assert_eq;

        let info = info();
        let tile = tile();
        let base = base_indices(info, tile);
        let mut session = EditSession::new(info, tile, base.clone()).unwrap();
        let mut applied_ops: Vec<Op> = Vec::new();
        let mut undone_ops: Vec<Op> = Vec::new();

        for action in actions {
            let before = session.indices().to_vec();
            let outcome = match action {
                Action::Append(op) => {
                    let region = session
                        .append(op.clone())
                        .expect("generated stroke is valid");
                    applied_ops.push(op.clone());
                    undone_ops.clear();
                    Some(region)
                }
                Action::Undo => {
                    if applied_ops.is_empty() {
                        prop_assert_eq!(session.undo(), None);
                        None
                    } else {
                        let op = applied_ops.pop().expect("checked non-empty above");
                        undone_ops.push(op);
                        Some(session.undo().expect("model says an op is applied"))
                    }
                }
                Action::Redo => {
                    if undone_ops.is_empty() {
                        prop_assert_eq!(session.redo(), None);
                        None
                    } else {
                        let op = undone_ops.pop().expect("checked non-empty above");
                        applied_ops.push(op);
                        Some(session.redo().expect("model says an op is undone"))
                    }
                }
            };
            prop_assert_eq!(session.op_count(), applied_ops.len());
            prop_assert_eq!(session.can_undo(), !applied_ops.is_empty());
            prop_assert_eq!(session.can_redo(), !undone_ops.is_empty());
            let after = session.indices();
            match outcome {
                Some(region) => assert_contained(&before, after, tile, region)?,
                None => prop_assert_eq!(after, before.as_slice()),
            }
            let expected = replay(&base, info, tile, &applied_ops);
            prop_assert_eq!(after, expected.as_slice());
        }
        Ok(())
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            failure_persistence: None,
            ..proptest::prelude::ProptestConfig::default()
        })]

        #[test]
        fn prop_random_op_sequences(actions in proptest::collection::vec(arb_action(), 0..300)) {
            check_sequence(&actions)?;
        }
    }
}
