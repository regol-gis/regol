use super::{Bounds, Features, LeafRecord, Work, next_leaf};
use crate::{
    Error, Result,
    decode::{Decode, add},
    feature::TileExtent,
    source::Source,
};

struct Branch {
    offset: u64,
    depth: usize,
    parent_bounds: Option<Bounds>,
}

impl<S: Source> Features<'_, S> {
    // World scans suppress duplicate features and skip spatial pruning. Visit
    // the physical trees separately so neither behavior hides bad bounds.
    pub(crate) fn verify_tile_tree_bounds(&mut self, start: u64, end: u64) -> Result<()> {
        self.work.clear();
        self.load_tile(start, Bounds::WORLD)?;
        self.tile = TileExtent::new(start, end)?;
        let mut branches = Vec::new();
        while let Some(work) = self.work.pop() {
            let Work::Roots { mut p, node } = work else {
                unreachable!("tile loading only schedules spatial roots");
            };
            loop {
                self.try_step(&mut || Ok(true))?;
                let (more, target) = self.root(p)?;
                if let Some(offset) = target {
                    branches.push(Branch {
                        offset,
                        depth: 0,
                        parent_bounds: None,
                    });
                }
                while let Some(Branch {
                    offset,
                    depth,
                    parent_bounds,
                }) = branches.pop()
                {
                    self.try_step(&mut || Ok(true))?;
                    if depth >= self.library.limits.max_tree_depth {
                        return Err(Error::Limit("R-tree depth"));
                    }
                    let (word, bounds) = self.branch_record(offset)?;
                    if let Some(parent) = parent_bounds {
                        check_bounds(
                            offset,
                            parent,
                            bounds,
                            "R-tree branch bounds exclude child branch",
                        )?;
                    }
                    if word & 1 == 0 {
                        branches.push(Branch {
                            offset: offset + 20,
                            depth,
                            parent_bounds,
                        });
                    }
                    let target = add(offset, i64::from(word & !3))?;
                    if word & 2 == 0 {
                        branches.push(Branch {
                            offset: target,
                            depth: depth + 1,
                            parent_bounds: Some(bounds),
                        });
                    } else if node {
                        self.verify_leaf_bounds::<true, 20>(target, bounds)?;
                    } else {
                        self.verify_leaf_bounds::<false, 32>(target, bounds)?;
                    }
                }
                if !more {
                    break;
                }
                p += 8;
            }
        }
        Ok(())
    }

    fn verify_leaf_bounds<const NODE: bool, const SIZE: usize>(
        &mut self,
        mut offset: u64,
        parent: Bounds,
    ) -> Result<()> {
        loop {
            self.try_step(&mut || Ok(true))?;
            self.in_tile(offset, SIZE as u64)?;
            let bytes = self.library.source.array::<SIZE>(offset)?;
            let record = LeafRecord::read::<NODE>(&bytes).expect("complete spatial leaf record");
            if NODE && record.flags & 4 != 0 {
                self.in_tile(offset, SIZE as u64 + 4)?;
            }
            // Deleted leaves do not contribute to the live subtree bounds.
            // Multi-tile copies do, including parts outside the tile itself.
            if record.flags & 0x800 == 0 {
                if !record.has_layout::<NODE>() {
                    return Err(Error::invalid(offset, "invalid R-tree leaf type"));
                }
                let bounds = if NODE {
                    let point = record.point();
                    Bounds {
                        min: point,
                        max: point,
                    }
                } else {
                    record.bbox()?
                };
                check_bounds(
                    offset,
                    parent,
                    bounds,
                    "R-tree branch bounds exclude feature",
                )?;
            }
            let Some(next) = next_leaf(offset, NODE, record.flags) else {
                return Ok(());
            };
            offset = next;
        }
    }
}

fn check_bounds(offset: u64, parent: Bounds, child: Bounds, message: &'static str) -> Result<()> {
    if parent.contains(child.min) && parent.contains(child.max) {
        Ok(())
    } else {
        Err(Error::invalid(offset, message))
    }
}
