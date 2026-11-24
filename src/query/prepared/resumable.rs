// Each tile retains its checked traversal and one ordered queue of feature handles.
// A rendering miss parks the feature's ticket while the tile can discover more.
use super::*;
use crate::source::{RangeQuery, RangeQueryEvent, TraversalOrder, TraversalOrdinal};
use std::collections::VecDeque;

const MAX_TILES: usize = 128;
const MAX_BUFFERED: usize = 4096;
const MAX_FRONT_FEATURES: usize = 128;
const MAX_PENDING_RENDERS: usize = 16;

struct Active<'a, S: Source>(&'a dyn RangeQuery<S>);
impl<S: Source> Drop for Active<'_, S> {
    fn drop(&mut self) {
        self.0.end();
    }
}

struct Entry<'a, S> {
    index: u32,
    state: State<'a, S>,
}

enum State<'a, S> {
    Waiting(usize, Feature<'a, S>),
    Ready(Result<Feature<'a, S>>),
}

struct Lane<'a, S> {
    id: usize,
    start: u64,
    tile_index: u32,
    next_index: u32,
    cursor: Features<'a, S>,
    header: Option<(u64, Bounds)>,
    entries: VecDeque<Entry<'a, S>>,
    pending_renders: usize,
    scanned: bool,
    done: bool,
}

impl<'a, S> Lane<'a, S> {
    fn push(&mut self, state: State<'a, S>) {
        self.entries.push_back(Entry {
            index: self.next_index,
            state,
        });
        self.next_index += 1;
    }

    fn take_ready(
        &mut self,
        schedule: &dyn RangeQuery<S>,
        ready_first: bool,
    ) -> Option<Result<Feature<'a, S>>>
    where
        S: Source,
    {
        let index = if ready_first {
            self.entries
                .iter()
                .position(|entry| matches!(entry.state, State::Ready(_)))?
        } else {
            matches!(self.entries.front()?.state, State::Ready(_)).then_some(0)?
        };
        let entry = self.entries.remove(index).expect("ready entry");
        let State::Ready(result) = entry.state else {
            unreachable!("ready entry")
        };
        if ready_first && let Ok(feature) = result {
            schedule.feature_ready(
                feature,
                TraversalOrdinal {
                    tile: self.tile_index,
                    feature: entry.index,
                },
            );
        }
        Some(result)
    }
}

pub(super) struct Pool<'a, S> {
    directory: Features<'a, S>,
    lanes: VecDeque<Lane<'a, S>>,
    exhausted: bool,
    directory_error: Option<crate::Error>,
    steps: u64,
    next_tile: u32,
}

impl<'a, S: Source> Pool<'a, S> {
    pub(super) fn new(directory: Features<'a, S>) -> Self {
        Self {
            directory,
            lanes: VecDeque::new(),
            exhausted: false,
            directory_error: None,
            steps: 0,
            next_tile: 0,
        }
    }

    pub(super) fn next(
        &mut self,
        query: &PreparedQuery<'a, '_, S>,
    ) -> Option<Result<Feature<'a, S>>> {
        self.advance(query).transpose()
    }

    fn advance(&mut self, query: &PreparedQuery<'a, '_, S>) -> Result<Option<Feature<'a, S>>> {
        let schedule = query.library.source().range_query().ok_or_else(|| {
            std::io::Error::other("resumable range query adapter was removed during a scan")
        })?;
        let width = schedule.width().min(MAX_TILES);
        let ready_first = schedule.order() == TraversalOrder::Ready;
        loop {
            if ready_first {
                if let Some(result) = self
                    .lanes
                    .iter_mut()
                    .find_map(|lane| lane.take_ready(schedule, true))
                {
                    return result.map(Some);
                }
                self.lanes.retain(|lane| !lane.done);
            } else if let Some(first) = self.lanes.front_mut() {
                if let Some(result) = first.take_ready(schedule, false) {
                    return result.map(Some);
                }
                if first.done {
                    self.lanes.pop_front();
                    continue;
                }
            }

            let mut occupied = self.lanes.iter().filter(|lane| !lane.done).count();
            let mut buffered: usize = self.lanes.iter().map(|lane| lane.entries.len()).sum();
            while occupied < width
                && self.lanes.len() < MAX_TILES
                && buffered < MAX_BUFFERED
                && !self.exhausted
            {
                let tile = match self.directory.next_payload_tile(schedule) {
                    Ok(tile) => tile,
                    Err(error) => {
                        self.directory_error = Some(error);
                        None
                    }
                };
                if let Some(tile) = tile {
                    self.lanes.push_back(Lane {
                        id: schedule.allocate_probe(),
                        start: tile.0,
                        tile_index: self.next_tile,
                        next_index: 0,
                        cursor: self.directory.empty_payload_cursor(),
                        header: Some(tile),
                        entries: VecDeque::new(),
                        pending_renders: 0,
                        scanned: false,
                        done: false,
                    });
                    self.next_tile += 1;
                    occupied += 1;
                } else {
                    self.exhausted = true;
                }
            }
            if self.lanes.is_empty() {
                return self.directory_error.take().map_or(Ok(None), Err);
            }

            let mut completed_tile = false;
            for (index, lane) in self.lanes.iter_mut().enumerate() {
                if lane.done {
                    continue;
                }

                if lane.pending_renders > 0 {
                    for entry in &mut lane.entries {
                        let State::Waiting(id, feature) = entry.state else {
                            continue;
                        };
                        if schedule.begin(id) {
                            let _active = Active(schedule);
                            schedule.prepare_feature(feature);
                            if !schedule.pending() {
                                entry.state = State::Ready(Ok(feature));
                                lane.pending_renders -= 1;
                            }
                        }
                    }
                }

                let limit = if index == 0 {
                    MAX_FRONT_FEATURES
                } else {
                    lane.entries.len() + MAX_BUFFERED.saturating_sub(buffered)
                };
                while !lane.scanned
                    && lane.entries.len() < limit
                    && lane.pending_renders < MAX_PENDING_RENDERS
                    && schedule.begin(lane.id)
                {
                    let _active = Active(schedule);
                    if let Some((start, bounds)) = lane.header {
                        let outcome = lane.cursor.enter_payload(start, bounds);
                        if schedule.pending() {
                            break;
                        }
                        if let Err(error) = outcome {
                            lane.push(State::Ready(Err(error)));
                            buffered += 1;
                            lane.scanned = true;
                            break;
                        }
                        lane.header = None;
                    }

                    let mut parked = false;
                    while lane.entries.len() < limit {
                        let saved = lane.cursor.checkpoint();
                        let before = lane.cursor.steps();
                        let found = query.next_unbounded(&mut lane.cursor);
                        if schedule.pending() {
                            lane.cursor.restore(saved);
                            break;
                        }
                        self.steps += lane.cursor.steps() - before;
                        if self.steps + self.directory.steps()
                            > query.library.limits.max_traversal_steps
                        {
                            return Err(crate::Error::Limit("spatial traversal steps"));
                        }
                        let Some(Ok(feature)) = found else {
                            if let Some(error) = found {
                                lane.push(State::Ready(error));
                                buffered += 1;
                            }
                            lane.scanned = true;
                            break;
                        };

                        schedule.prepare_feature(feature);
                        buffered += 1;
                        if schedule.pending() {
                            lane.push(State::Waiting(lane.id, feature));
                            lane.pending_renders += 1;
                            lane.id = schedule.allocate_probe();
                            parked = true;
                            break;
                        }
                        lane.push(State::Ready(Ok(feature)));
                    }
                    if !parked {
                        break;
                    }
                }
                if lane.scanned && lane.pending_renders == 0 {
                    lane.done = true;
                    completed_tile = true;
                    schedule.observe(RangeQueryEvent::TileCompleted { start: lane.start });
                }
            }
            schedule.observe(RangeQueryEvent::Buffered { entries: buffered });
            if completed_tile
                && !self.exhausted
                && self.lanes.len() < MAX_TILES
                && buffered < MAX_BUFFERED
            {
                continue;
            }
            let wait = if ready_first {
                self.lanes.iter().any(|lane| !lane.done)
                    && !self.lanes.iter().any(|lane| {
                        lane.entries
                            .iter()
                            .any(|entry| matches!(entry.state, State::Ready(_)))
                    })
            } else {
                self.lanes.front().is_some_and(|lane| {
                    !lane.done
                        && !lane
                            .entries
                            .front()
                            .is_some_and(|entry| matches!(entry.state, State::Ready(_)))
                })
            };
            if wait {
                schedule.wait()?;
            }
        }
    }
}
