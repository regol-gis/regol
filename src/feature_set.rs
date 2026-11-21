use crate::{
    Bounds, Error, Feature, FeatureId, FeatureType, Library, Point, Query, Result, SpatialFilter,
    source::Source,
    units::{Area, Length, area::square_meter, length::meter},
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

/// A reusable selection from one open library. Set operations combine conditions and
/// iteration visits candidates once, without materializing the result. Member and parent
/// sets retain handles to their direct targets, keyed by typed OSM ID. Set order is
/// unspecified. Use [`Feature::members`] to retain original member order and repetitions.
pub struct FeatureSet<'a, S> {
    library: &'a Library<S>,
    condition: Arc<Condition<'a, S>>,
    window: Option<Bounds>,
}

type MemberMap<'a, S> = BTreeMap<FeatureId, Feature<'a, S>>;

enum Condition<'a, S> {
    All,
    Query(Query),
    Bounds(Bounds),
    Spatial(SpatialFilter),
    Connected {
        origin: FeatureId,
        points: HashSet<Point>,
    },
    Members(Arc<MemberMap<'a, S>>),
    Intersection(Arc<Self>, Arc<Self>),
    Union {
        left: Arc<Self>,
        right: Arc<Self>,
        candidates: Option<Arc<MemberMap<'a, S>>>,
    },
    Difference(Arc<Self>, Arc<Self>),
}

impl<'a, S: Source> Condition<'a, S> {
    fn matches(&self, feature: Feature<'a, S>) -> Result<bool> {
        Ok(match self {
            Self::All => true,
            Self::Query(query) => query.matches(feature)?,
            Self::Bounds(bounds) => bounds.intersects(feature.bounds()?),
            Self::Spatial(filter) => filter.matches(feature)?,
            Self::Connected { origin, points } => {
                feature.id() != *origin
                    && any_vertex(feature, &mut |point| points.contains(&point))?
            }
            Self::Members(members) => members.contains_key(&feature.id()),
            Self::Intersection(left, right) => left.matches(feature)? && right.matches(feature)?,
            Self::Union { left, right, .. } => left.matches(feature)? || right.matches(feature)?,
            Self::Difference(left, right) => left.matches(feature)? && !right.matches(feature)?,
        })
    }

    // A positive identity restriction can drive the traversal directly.
    fn candidates(&self) -> Option<&Arc<MemberMap<'a, S>>> {
        match self {
            Self::Members(members) => Some(members),
            Self::Intersection(left, right) => match (left.candidates(), right.candidates()) {
                (Some(a), Some(b)) => Some(if a.len() <= b.len() { a } else { b }),
                (a, b) => a.or(b),
            },
            Self::Difference(left, _) => left.candidates(),
            Self::Union { candidates, .. } => candidates.as_ref(),
            _ => None,
        }
    }
}

impl<S> Clone for FeatureSet<'_, S> {
    fn clone(&self) -> Self {
        Self {
            library: self.library,
            condition: Arc::clone(&self.condition),
            window: self.window,
        }
    }
}

impl<S: Source> Library<S> {
    /// All features of this library, as a selection that can be narrowed or combined.
    pub fn feature_set(&self) -> FeatureSet<'_, S> {
        FeatureSet {
            library: self,
            condition: Arc::new(Condition::All),
            window: None,
        }
    }

    /// Find a typed OSM ID. An unrestricted lookup may scan the entire library.
    pub fn feature_by_id(&self, id: FeatureId) -> Result<Option<Feature<'_, S>>> {
        self.feature_set().get(id)
    }
}

impl<'a, S: Source> Feature<'a, S> {
    fn collect_set(
        self,
        iter: impl Iterator<Item = Result<Feature<'a, S>>>,
    ) -> Result<FeatureSet<'a, S>> {
        let members = iter
            .map(|item| item.map(|f| (f.id(), f)))
            .collect::<Result<_>>()?;
        Ok(FeatureSet {
            library: self.library,
            condition: Arc::new(Condition::Members(Arc::new(members))),
            window: None,
        })
    }

    /// Distinct direct members of a relation as a feature set. Reading the membership
    /// list checks its references. For ordered members with roles use [`Self::members`].
    pub fn member_set(self) -> Result<FeatureSet<'a, S>> {
        self.collect_set(self.members()?.map(|member| member.map(|m| m.feature)))
    }

    /// Distinct direct relation members with the specified role (including the empty role).
    pub fn members_with_role(self, role: &str) -> Result<FeatureSet<'a, S>> {
        self.collect_set(self.members()?.filter_map(|member| match member {
            Ok(m) if m.role == role => Some(Ok(m.feature)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        }))
    }

    /// All direct parent relations and, for a node, ways that contain it.
    pub fn parent_set(self) -> Result<FeatureSet<'a, S>> {
        let ways = if self.kind() == FeatureType::Node {
            Some(self.parent_ways()?)
        } else {
            None
        };
        self.collect_set(self.parent_relations()?.chain(ways.into_iter().flatten()))
    }
}

impl<'a, S: Source> FeatureSet<'a, S> {
    fn and(&self, condition: Condition<'a, S>, window: Option<Bounds>) -> Self {
        let condition = if matches!(*self.condition, Condition::All) {
            Arc::new(condition)
        } else {
            Arc::new(Condition::Intersection(
                self.condition.clone(),
                Arc::new(condition),
            ))
        };
        Self {
            library: self.library,
            condition,
            window: smaller(self.window, window),
        }
    }

    /// Keep features that match this parsed GOQL expression.
    pub fn select(&self, query: Query) -> Self {
        self.and(Condition::Query(query), None)
    }
    /// Keep features whose stored bounding boxes intersect this window, inclusively.
    pub fn in_bounds(&self, bounds: Bounds) -> Self {
        self.and(Condition::Bounds(bounds), Some(bounds))
    }
    /// Select other features sharing a vertex position with the supplied feature.
    /// Tests stored integer coordinates, including vertices of recursively nested relation
    /// members. Coincident vertices match even when their OSM IDs differ. For identity-based
    /// membership, use [`Feature::parent_ways`] or [`Feature::members`].
    pub fn connected_to(&self, other: Feature<'a, S>) -> Result<Self> {
        if !std::ptr::eq(self.library, other.library) {
            return Err(Error::DifferentLibraries);
        }
        let bounds = other.bounds()?;
        let mut points = HashSet::new();
        any_vertex(other, &mut |point| {
            points.insert(point);
            false
        })?;
        Ok(self.and(
            Condition::Connected {
                origin: other.id(),
                points,
            },
            Some(bounds),
        ))
    }

    /// Keep features satisfying an exact spatial filter.
    pub fn spatial(&self, filter: SpatialFilter) -> Self {
        if filter.has_no_matches() {
            return Self {
                library: self.library,
                condition: Arc::new(Condition::Members(Arc::default())),
                window: None,
            };
        }
        let bounds = filter.candidate_bounds();
        self.and(Condition::Spatial(filter), bounds)
    }

    fn check_library(&self, other: &Self) -> Result<()> {
        if !std::ptr::eq(self.library, other.library) {
            return Err(Error::DifferentLibraries);
        }
        Ok(())
    }

    fn known_empty(&self) -> bool {
        self.condition
            .candidates()
            .is_some_and(|members| members.is_empty())
    }

    fn combine(&self, condition: Condition<'a, S>, window: Option<Bounds>) -> Self {
        Self {
            library: self.library,
            condition: Arc::new(condition),
            window,
        }
    }
    /// Features in both sets. Both selections must belong to the same open library.
    pub fn intersection(&self, other: &Self) -> Result<Self> {
        self.check_library(other)?;
        if self.known_empty() {
            return Ok(self.clone());
        }
        if other.known_empty() {
            return Ok(other.clone());
        }
        Ok(self.combine(
            Condition::Intersection(self.condition.clone(), other.condition.clone()),
            smaller(self.window, other.window),
        ))
    }
    /// Features in either set, traversed once even when both conditions match.
    pub fn union(&self, other: &Self) -> Result<Self> {
        self.check_library(other)?;
        let left = self.condition.candidates();
        let right = other.condition.candidates();
        if left.is_some_and(|map| map.is_empty()) {
            return Ok(other.clone());
        }
        if right.is_some_and(|map| map.is_empty()) {
            return Ok(self.clone());
        }
        // Filtered sets can share the same handles while accepting different
        // subsets of them. Preserve the two predicates even when the maps coincide.
        let candidates = if let Some((a, b)) = left.zip(right) {
            let members = if Arc::ptr_eq(a, b) {
                Arc::clone(a)
            } else {
                Arc::new(a.iter().chain(b.iter()).map(|(&id, &f)| (id, f)).collect())
            };
            if matches!(*self.condition, Condition::Members(_))
                && matches!(*other.condition, Condition::Members(_))
            {
                return Ok(self.combine(Condition::Members(members), None));
            }
            Some(members)
        } else {
            None
        };
        let window = self.window.zip(other.window).map(|(a, b)| Bounds {
            min: crate::Point {
                x: a.min.x.min(b.min.x),
                y: a.min.y.min(b.min.y),
            },
            max: crate::Point {
                x: a.max.x.max(b.max.x),
                y: a.max.y.max(b.max.y),
            },
        });
        Ok(self.combine(
            Condition::Union {
                left: self.condition.clone(),
                right: other.condition.clone(),
                candidates,
            },
            window,
        ))
    }
    /// Features in this set that are absent from the other set.
    pub fn difference(&self, other: &Self) -> Result<Self> {
        self.check_library(other)?;
        if self.known_empty() || other.known_empty() {
            return Ok(self.clone());
        }
        Ok(self.combine(
            Condition::Difference(self.condition.clone(), other.condition.clone()),
            self.window,
        ))
    }
    /// Test membership of a borrowed feature. A handle from another library is an error.
    pub fn contains(&self, feature: Feature<'a, S>) -> Result<bool> {
        if !std::ptr::eq(self.library, feature.library) {
            return Err(Error::DifferentLibraries);
        }
        self.condition.matches(feature)
    }
    fn candidates(&self) -> Box<dyn Iterator<Item = Result<Feature<'a, S>>> + '_> {
        if let Some(members) = self.condition.candidates() {
            Box::new(members.values().copied().map(Ok))
        } else if let Some(bounds) = self.window {
            Box::new(self.library.in_bounds(bounds))
        } else {
            Box::new(self.library.features())
        }
    }
    /// Iterate matching features. An error ends this traversal.
    pub fn iter(&self) -> impl Iterator<Item = Result<Feature<'a, S>>> + '_ {
        let mut candidates = self.candidates();
        let mut done = false;
        std::iter::from_fn(move || {
            if done {
                return None;
            }
            for next in candidates.by_ref() {
                match next.and_then(|f| {
                    self.condition
                        .matches(f)
                        .map(|matched| matched.then_some(f))
                }) {
                    Ok(Some(f)) => return Some(Ok(f)),
                    Ok(None) => {}
                    Err(error) => {
                        done = true;
                        return Some(Err(error));
                    }
                }
            }
            None
        })
    }
    /// Find a typed OSM ID within this selection. Bounds or a member set narrow the scan.
    pub fn get(&self, id: FeatureId) -> Result<Option<Feature<'a, S>>> {
        if let Some(members) = self.condition.candidates() {
            return match members.get(&id) {
                Some(&feature) if self.condition.matches(feature)? => Ok(Some(feature)),
                _ => Ok(None),
            };
        }
        let mask = match id.kind {
            FeatureType::Node => crate::Types::NODE,
            FeatureType::Way => crate::Types::WAY.union(crate::Types::AREA),
            FeatureType::Relation => crate::Types::RELATION.union(crate::Types::AREA),
        };
        let candidates = crate::Features::new(self.library, self.window).restrict(mask.0, 0);
        for feature in candidates {
            let feature = feature?;
            if feature.id() == id && self.condition.matches(feature)? {
                return Ok(Some(feature));
            }
        }
        Ok(None)
    }
    /// Return the first matching feature, or `None` for an empty set.
    pub fn first(&self) -> Result<Option<Feature<'a, S>>> {
        self.iter().next().transpose()
    }
    /// Count matching features without retaining the results.
    pub fn count(&self) -> Result<u64> {
        self.iter().try_fold(0, |n, f| f.map(|_| n + 1))
    }

    /// Sum the area of matching features in square metres. Overlapping areas
    /// contribute independently. Empty sets return zero.
    pub fn area(&self) -> Result<Area> {
        self.iter()
            .try_fold(Area::new::<square_meter>(0.0), |total, feature| {
                Ok(total + feature?.area()?)
            })
    }

    /// Sum line lengths and polygon perimeters in metres over matching features.
    pub fn length(&self) -> Result<Length> {
        self.iter()
            .try_fold(Length::new::<meter>(0.0), |total, feature| {
                Ok(total + feature?.length()?)
            })
    }
}

// A long way can intersect two disjoint windows. Either window is a safe index
// hint for their conjunction, but the geometric intersection of the windows is not.
fn smaller(a: Option<Bounds>, b: Option<Bounds>) -> Option<Bounds> {
    let area =
        |b: Bounds| u64::from(b.max.x.abs_diff(b.min.x)) * u64::from(b.max.y.abs_diff(b.min.y));
    match (a, b) {
        (Some(a), Some(b)) => Some(if area(a) <= area(b) { a } else { b }),
        (a, b) => a.or(b),
    }
}

fn any_vertex<S: Source>(
    feature: Feature<'_, S>,
    test: &mut impl FnMut(Point) -> bool,
) -> Result<bool> {
    let mut steps = feature.library.limits().max_traversal_steps;
    let mut coordinates = feature.library.limits().max_coordinates;
    let mut visited = HashSet::new();
    fn visit<S: Source>(
        feature: Feature<'_, S>,
        test: &mut impl FnMut(Point) -> bool,
        depth: usize,
        visited: &mut HashSet<FeatureId>,
        steps: &mut u64,
        coordinates: &mut u64,
    ) -> Result<bool> {
        *steps = steps
            .checked_sub(1)
            .ok_or(Error::Limit("connected feature traversal"))?;
        if depth > feature.library.limits().max_tree_depth {
            return Err(Error::Limit("connected relation depth"));
        }
        let mut point_matches = |point| {
            *coordinates = coordinates
                .checked_sub(1)
                .ok_or(Error::Limit("connected feature coordinates"))?;
            Ok(test(point))
        };
        match feature.kind() {
            FeatureType::Node => point_matches(feature.point()?.ok_or(Error::WrongFeatureType)?),
            FeatureType::Way => {
                for point in feature.coordinates()? {
                    if point_matches(point?)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            FeatureType::Relation => {
                if visited.insert(feature.id()) {
                    for member in feature.members()? {
                        if visit(
                            member?.feature,
                            test,
                            depth + 1,
                            visited,
                            steps,
                            coordinates,
                        )? {
                            return Ok(true);
                        }
                    }
                }
                Ok(false)
            }
        }
    }
    visit(feature, test, 0, &mut visited, &mut steps, &mut coordinates)
}
