use super::{Features, Work};
use crate::{Feature, Result, decode::ReadWindow, feature::TileExtent, source::Source};

impl<'a, S: Source> Features<'a, S> {
    /// Walk exactly the tile, category and branch checks of ordinary traversal,
    /// stopping before reading each group's physical records.
    pub(crate) fn next_sidecar_leaf(&mut self) -> Result<Option<(u64, bool, TileExtent)>> {
        loop {
            if let Some((p, node)) = self.leaf.take() {
                return Ok(Some((p, node, self.tile)));
            }
            let Some(work) = self.work.pop() else {
                return Ok(None);
            };
            self.try_step(&mut || Ok(true))?;
            match work {
                Work::Tile(tile) => self.start_tile(tile)?,
                Work::Children {
                    parent,
                    mask,
                    entries,
                    extent,
                    ordinal,
                } => {
                    self.push_child(parent, mask, entries, extent, ordinal);
                }
                Work::Roots { p, node } => {
                    self.scan_roots(p, node, &mut || Ok(true))?;
                }
                Work::Branch { p, node, depth } => self.expand_branch(p, node, depth)?,
                #[cfg(feature = "parallel")]
                Work::Payload { .. } => unreachable!("serial traversal does not partition"),
                Work::Leaf { .. } => unreachable!("leaf cursor is handled before the work stack"),
            }
        }
    }

    pub(crate) fn charge_sidecar_build_record(&mut self) -> Result<()> {
        self.try_step(&mut || Ok(true))?;
        Ok(())
    }

    pub(crate) fn sidecar_candidate(
        &mut self,
        p: u64,
        node: bool,
        mut predicate: impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
        ) -> Result<bool>,
    ) -> Result<Option<Feature<'a, S>>> {
        let result = self.leaf_candidate::<true>(p, node, &mut |f, tags, strings, _| {
            predicate(f, tags, strings)
        });
        // The sidecar cursor owns the position in the current group.
        self.leaf = None;
        result
    }
}
