//! Discover independent range requests with the checked spatial traversal.
use super::*;
use crate::source::{RangeQuery, RangeQueryEvent, TileVisit};
use std::collections::BTreeSet;

const MAX_STEPS: u64 = 1_000_000;

impl<'a, S: Source> Features<'a, S> {
    fn planning_cursor(&self) -> Self {
        let mut cursor = Features::new(self.library, Some(self.bounds))
            .restrict(self.types, self.required_keys)
            .require_any(self.alternative_keys);
        cursor.min_branch_width = self.min_branch_width;
        cursor.work.clear();
        cursor
    }

    pub(super) fn plan_group(
        &mut self,
        schedule: &dyn RangeQuery<S>,
        start: u64,
        bounds: Bounds,
    ) -> Result<()> {
        let width = schedule.plan_width().min(128);
        if width == 0 || !self.prune {
            return Ok(());
        }
        if let Some(last) = self.frontier_until {
            if last == start {
                self.frontier_until = None;
            }
            return Ok(());
        }
        let mut tiles = vec![(start, bounds)];
        let mut directory = self.planning_cursor();
        directory.work.clone_from(&self.work);
        let limit = self.library.limits.max_traversal_steps.min(MAX_STEPS);
        let mut steps = 0;
        while tiles.len() < width {
            let Some(work) = directory.work.pop() else {
                break;
            };
            steps += 1;
            if steps > limit {
                return Ok(());
            }
            match work {
                Work::Tile(tile) => tiles.extend(directory.index_tile(tile)?),
                Work::Children {
                    parent,
                    mask,
                    entries,
                    extent,
                    ordinal,
                } => directory.push_child(parent, mask, entries, extent, ordinal),
                _ => return Ok(()),
            }
        }
        if tiles.len() > 1 {
            self.frontier_until = Some(tiles.last().unwrap().0);
        }

        let headers = schedule.plan_unit(true, tiles.len()).max(1);
        let mut known = BTreeSet::new();
        for &(offset, _) in &tiles {
            self.library.source().check(offset, 24)?;
            known.extend(page_span(offset, 24, headers));
        }
        schedule.warm_pages(&known.iter().copied().collect::<Vec<_>>(), headers)?;
        let mut cursors = Vec::with_capacity(tiles.len());
        for (offset, bbox) in tiles {
            let mut cursor = self.planning_cursor();
            cursor.load_tile(offset, bbox)?;
            schedule.observe(RangeQueryEvent::TileExtent {
                start: offset,
                length: cursor.tile.end() - offset,
            });
            cursors.push(cursor);
        }

        // The header batch provides the first parallel link sample. If the next
        // stage chooses a different unit, rediscover its aligned page keys.
        let unit = schedule.plan_unit(false, cursors.len()).max(1);
        if unit != headers {
            known.clear();
        }

        // Process every sibling whose page is known. Suspend only its dependent
        // subtree when another page is needed, so discovery can continue in the
        // other subtrees and tiles before issuing the next batch.
        let tags = schedule.plan_tags() && (self.required_keys | self.alternative_keys != 0);
        loop {
            let mut missing = BTreeSet::new();
            for cursor in &mut cursors {
                let mut waiting = Vec::new();
                while let Some(work) = cursor.work.pop() {
                    steps += 1;
                    if steps > limit {
                        return Ok(());
                    }
                    let (offset, length) = match work {
                        Work::Roots { p, .. } => (p, 8),
                        Work::Branch { p, .. } => (p, 20),
                        Work::Leaf { p, node } => (p, if node { 20 } else { 32 }),
                        _ => unreachable!("tile payload work"),
                    };
                    cursor.in_tile(offset, length)?;
                    let mut ready = true;
                    for page in page_span(offset, length, unit) {
                        if !known.contains(&page) {
                            missing.insert(page);
                            ready = false;
                        }
                    }
                    if !ready {
                        if tags || !matches!(work, Work::Leaf { .. }) {
                            waiting.push(work);
                        }
                        continue;
                    }
                    match work {
                        Work::Roots { p, node } => {
                            let (more, target) = cursor.root(p)?;
                            cursor.push_root(p, node, more, target);
                        }
                        Work::Branch { p, node, depth } => cursor.expand_branch(p, node, depth)?,
                        Work::Leaf { p, node } if tags => {
                            let feature =
                                cursor
                                    .leaf_candidate::<true>(p, node, &mut |_, _, _, _| Ok(true))?;
                            if let Some(feature) = feature {
                                let table = feature.checked_tag_table()?;
                                cursor.in_tile(table, 4)?;
                                for page in page_span(table, 4, unit) {
                                    if !known.contains(&page) {
                                        missing.insert(page);
                                    }
                                }
                            }
                        }
                        Work::Leaf { .. } => {}
                        _ => unreachable!(),
                    }
                    if let Some((p, node)) = cursor.leaf.take() {
                        cursor.work.push(Work::Leaf { p, node });
                    }
                }
                cursor.work = waiting;
            }
            if missing.is_empty() {
                return Ok(());
            }
            schedule.warm_pages(&missing.iter().copied().collect::<Vec<_>>(), unit)?;
            known.extend(missing);
        }
    }
}

fn page_span(offset: u64, length: u64, unit: u64) -> impl Iterator<Item = u64> {
    (offset / unit..=(offset + length - 1) / unit).map(move |n| n * unit)
}

// Checkpoint only traversal state. Retaining immutable record/tag read windows
// across retries is safe; failed ReadWindow fills detach their partial contents.
pub(crate) struct Checkpoint {
    work: Vec<Work>,
    leaf: Option<(u64, bool)>,
    steps: u64,
    done: bool,
}

impl<'a, S: Source> Features<'a, S> {
    pub(crate) fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            work: self.work.clone(),
            leaf: self.leaf,
            steps: self.steps,
            done: self.done,
        }
    }

    pub(crate) fn restore(&mut self, saved: Checkpoint) {
        self.work = saved.work;
        self.leaf = saved.leaf;
        self.steps = saved.steps;
        self.done = saved.done;
    }

    pub(crate) fn next_payload_tile(
        &mut self,
        schedule: &dyn RangeQuery<S>,
    ) -> Result<Option<(u64, Bounds)>> {
        while let Some(work) = self.work.pop() {
            self.steps += 1;
            if self.steps > self.library.limits.max_traversal_steps {
                return Err(Error::Limit("tile directory traversal"));
            }
            match work {
                Work::Tile(tile) => {
                    if let Some(tile) = self.index_tile(tile)? {
                        match schedule.visit_tile(tile.0) {
                            TileVisit::Scan => return Ok(Some(tile)),
                            TileVisit::Skip => {}
                            TileVisit::PlanOnly => {
                                let _ = self.plan_group(schedule, tile.0, tile.1);
                            }
                        }
                    }
                }
                Work::Children {
                    parent,
                    mask,
                    entries,
                    extent,
                    ordinal,
                } => self.push_child(parent, mask, entries, extent, ordinal),
                _ => unreachable!("directory cursor"),
            }
        }
        Ok(None)
    }

    pub(crate) fn empty_payload_cursor(&self) -> Self {
        self.planning_cursor()
    }

    pub(crate) fn enter_payload(&mut self, start: u64, bounds: Bounds) -> Result<()> {
        self.work.clear();
        self.load_tile(start, bounds)
    }
}
