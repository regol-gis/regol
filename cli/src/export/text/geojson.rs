use super::{
    CutGeometry, DEFAULT_PRECISION, EncodeResult, Encoding, Feature, Format, GeometryWriter,
    Options, Scratch, Source, feature_geometry, flush_buf, push_feature_id, push_properties,
    tag_key,
};
use regol::FeatureType;
use std::io::{self, Write};

const COLLECTION_START: &[u8] = b"{\"type\":\"FeatureCollection\",\"features\":[";
const COLLECTION_END: &[u8] = b"]}\n";

pub(super) fn write<'a, S: Source + 'a>(
    features: impl IntoIterator<Item = regol::Result<Feature<'a, S>>>,
    output: &mut impl Write,
    format: Format,
    options: &Options,
    precision: usize,
) -> EncodeResult<()> {
    let encoder = Encoder::new(format, options, precision);
    encoder.begin(output)?;
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut scratch = Scratch::default();
    for (index, feature) in features.into_iter().enumerate() {
        let feature = match feature {
            Ok(feature) => feature,
            Err(error) => {
                if !buf.is_empty() {
                    output.write_all(&buf)?;
                }
                return Err(error.into());
            }
        };
        encoder.feature_into(&mut buf, feature, index == 0, &mut scratch)?;
        flush_buf(output, &mut buf)?;
    }
    if !buf.is_empty() {
        output.write_all(&buf)?;
    }
    encoder.end(output)
}

pub(crate) fn write_parallel<'a, S: Source + Sync + 'a>(
    features: impl IntoIterator<Item = regol::Result<Feature<'a, S>>>,
    output: &mut impl Write,
    format: Format,
    options: &Options,
    threads: usize,
) -> EncodeResult<()> {
    let encoder = Encoder::new(
        format,
        options,
        options.precision.unwrap_or(DEFAULT_PRECISION),
    );
    encoder.begin(output)?;

    super::parallel::write(
        features,
        output,
        threads,
        |output, feature, first, buf, scratch| {
            encoder.feature_into(buf, feature, first, scratch)?;
            flush_buf(output, buf)?;
            Ok(())
        },
        |buf, feature, first, scratch| encoder.feature_into(buf, feature, first, scratch),
    )?;
    encoder.end(output)
}

pub(super) struct Encoder<'a> {
    pub(super) collection: bool,
    include_bounds: bool,
    property_keys: Option<Vec<&'a str>>,
    writer: GeometryWriter,
}

impl<'a> Encoder<'a> {
    pub(super) fn new(format: Format, options: &'a Options, precision: usize) -> Self {
        let keys = options.keys.as_ref();
        let include_bounds = keys.is_some_and(|keys| keys.iter().any(|key| key == "bbox"));
        let property_keys = keys
            .filter(|keys| !keys.iter().any(|key| key == "*"))
            .map(|keys| {
                let mut selected = Vec::new();
                for key in keys
                    .iter()
                    .filter(|key| *key != "bbox")
                    .map(|key| tag_key(key))
                {
                    if !selected.contains(&key) {
                        selected.push(key);
                    }
                }
                selected
            });
        Self {
            collection: matches!(format, Format::GeoJson),
            include_bounds,
            property_keys,
            writer: GeometryWriter::new(Encoding::Json, precision),
        }
    }

    pub(super) fn begin(&self, output: &mut impl Write) -> io::Result<()> {
        if self.collection {
            output.write_all(COLLECTION_START)?;
        }
        Ok(())
    }

    pub(super) fn end(&self, output: &mut impl Write) -> EncodeResult<()> {
        if self.collection {
            output.write_all(COLLECTION_END)?;
        }
        Ok(())
    }

    pub(super) fn feature_into(
        &self,
        out: &mut Vec<u8>,
        feature: Feature<'_, impl Source>,
        first: bool,
        scratch: &mut Scratch,
    ) -> EncodeResult<()> {
        let id = feature.id();
        match id.kind {
            FeatureType::Node => {
                let point = feature
                    .point()
                    .map_err(|error| format!("{id}: {error}"))?
                    .ok_or_else(|| format!("{id}: wrong feature type"))?;
                let bounds = if self.include_bounds {
                    let (lon, lat) = self.writer.coordinates(point);
                    Some([lon, lat, lon, lat])
                } else {
                    None
                };
                self.write_prefix(out, id, first, &mut scratch.integer);
                out.extend_from_slice(b"{\"type\":\"Point\",\"coordinates\":");
                self.writer.push_point(out, point, &mut scratch.integer);
                out.push(b'}');
                self.write_suffix(out, feature, bounds, &mut scratch.integer)
            }
            FeatureType::Way => {
                scratch.points.clear();
                let coords = feature
                    .coordinates()
                    .map_err(|error| format!("{id}: {error}"))?;
                if let Some(upper) = coords.size_hint().1 {
                    scratch.points.reserve(upper.min(1024));
                }
                for point in coords {
                    scratch
                        .points
                        .push(point.map_err(|error| format!("{id}: {error}"))?);
                }
                let is_area = feature.is_area();
                if is_area
                    && (scratch.points.len() < 4
                        || scratch.points.first() != scratch.points.last())
                {
                    return Err(
                        format!("{id}: ring is unclosed or has fewer than four coordinates").into(),
                    );
                }
                if !super::super::antimeridian::crosses(&scratch.points) {
                    let bounds = if self.include_bounds {
                        Some(self.writer.bounds_coordinates(feature.bounds()?))
                    } else {
                        None
                    };
                    self.write_prefix(out, id, first, &mut scratch.integer);
                    if is_area {
                        out.extend_from_slice(b"{\"type\":\"Polygon\",\"coordinates\":[");
                        scratch.ring_coords.clear();
                        scratch.ring_coords.reserve(scratch.points.len());
                        for &p in &scratch.points {
                            scratch.ring_coords.push(self.writer.coordinates(p));
                        }
                        self.writer.push_ring_slice(
                            out,
                            &scratch.ring_coords,
                            true,
                            &mut scratch.integer,
                        );
                        out.extend_from_slice(b"]}");
                    } else {
                        out.extend_from_slice(b"{\"type\":\"LineString\",\"coordinates\":");
                        self.writer
                            .push_line_points(out, &scratch.points, &mut scratch.integer);
                        out.push(b'}');
                    }
                    return self.write_suffix(out, feature, bounds, &mut scratch.integer);
                }
                // Antimeridian-crossing way falls back to CutGeometry.
                let geometry = if is_area {
                    regol::Geometry::Polygon(regol::Polygon {
                        exterior: std::mem::take(&mut scratch.points),
                        interiors: Vec::new(),
                    })
                } else {
                    regol::Geometry::LineString(std::mem::take(&mut scratch.points))
                };
                let cut = CutGeometry::new(&geometry)
                    .map_err(|error| format!("{id}: {error}"))?;
                let bounds = if self.include_bounds {
                    Some(match cut.bounds() {
                        Some(bounds) => self.writer.cut_bounds(bounds),
                        None => self.writer.bounds_coordinates(feature.bounds()?),
                    })
                } else {
                    None
                };
                self.write_prefix(out, id, first, &mut scratch.integer);
                self.writer.push_cut_geometry(out, &cut, scratch);
                self.write_suffix(out, feature, bounds, &mut scratch.integer)
            }
            FeatureType::Relation => {
                let geometry = feature_geometry(feature)?;
                let cut = CutGeometry::new(&geometry)
                    .map_err(|error| format!("{id}: {error}"))?;
                let bounds = if self.include_bounds {
                    Some(match cut.bounds() {
                        Some(bounds) => self.writer.cut_bounds(bounds),
                        None => self.writer.bounds_coordinates(feature.bounds()?),
                    })
                } else {
                    None
                };
                self.write_prefix(out, id, first, &mut scratch.integer);
                self.writer.push_cut_geometry(out, &cut, scratch);
                self.write_suffix(out, feature, bounds, &mut scratch.integer)
            }
        }
    }

    #[inline]
    fn write_prefix(
        &self,
        out: &mut Vec<u8>,
        id: regol::FeatureId,
        first: bool,
        integer: &mut itoa::Buffer,
    ) {
        if self.collection && !first {
            out.push(b',');
        }
        out.extend_from_slice(b"{\"type\":\"Feature\",\"id\":\"");
        push_feature_id(out, integer, id);
        out.extend_from_slice(b"\",\"geometry\":");
    }

    #[inline]
    fn write_suffix(
        &self,
        out: &mut Vec<u8>,
        feature: Feature<'_, impl Source>,
        bounds: Option<[i64; 4]>,
        integer: &mut itoa::Buffer,
    ) -> EncodeResult<()> {
        if let Some(bounds) = bounds {
            out.extend_from_slice(b",\"bbox\":[");
            self.writer.push_bounds(out, bounds, integer);
            out.push(b']');
        }
        out.extend_from_slice(b",\"properties\":{");
        push_properties(feature, out, self.property_keys.as_deref(), integer)?;
        out.extend_from_slice(if self.collection { b"}}" } else { b"}}\n" });
        Ok(())
    }
}
