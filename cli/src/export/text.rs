use super::{
    ExportResult,
    antimeridian::{CutGeometry, degrees},
    kind_name,
};
use regol::{Bounds, Feature, Geometry, Point, Polygon, TagValue, source::Source};
use std::{
    cmp::Ordering,
    io::{self, Write},
};

mod geojson;
mod parallel;

const DEFAULT_PRECISION: usize = 7;
const MAX_PRECISION: usize = 15;

#[derive(Clone, Copy)]
pub(crate) enum Format {
    GeoJson,
    GeoJsonLines,
    Wkt,
    Csv,
}

#[derive(Default)]
pub(crate) struct Options {
    keys: Option<Vec<String>>,
    columns: Option<Vec<String>>,
    precision: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field<'a> {
    Id,
    Type,
    OsmId,
    Latitude,
    Longitude,
    Bounds,
    Geometry,
    Tags,
    Tag(&'a str),
}

impl<'a> Field<'a> {
    fn parse(name: &'a str) -> Self {
        match name {
            "id" => Self::Id,
            "type" => Self::Type,
            "osm_id" => Self::OsmId,
            "lat" => Self::Latitude,
            "lon" => Self::Longitude,
            "bbox" => Self::Bounds,
            "geometry" | "geom" | "wkt" => Self::Geometry,
            "tags" => Self::Tags,
            name => Self::Tag(tag_key(name)),
        }
    }
}

fn tag_key(name: &str) -> &str {
    name.strip_prefix("tag:").unwrap_or(name)
}

fn names(value: &str) -> Result<Vec<String>, &'static str> {
    let names: Vec<_> = value.split(',').map(str::trim).collect();
    if names.contains(&"") {
        return Err("expected a comma-separated list without empty names");
    }
    if names.contains(&"tag:") {
        return Err("expected a tag key after tag:");
    }
    Ok(names.into_iter().map(str::to_owned).collect())
}

impl Options {
    pub(crate) fn set_keys(&mut self, value: &str) -> ExportResult<()> {
        self.keys = Some(if value.is_empty() {
            Vec::new()
        } else {
            names(value)?
        });
        Ok(())
    }

    pub(crate) fn set_columns(&mut self, value: &str) -> ExportResult<()> {
        self.columns = Some(names(value)?);
        Ok(())
    }

    pub(crate) fn set_precision(&mut self, value: &str) -> ExportResult<()> {
        self.precision = Some(
            value
                .parse::<usize>()
                .ok()
                .filter(|&precision| precision <= MAX_PRECISION)
                .ok_or("precision must be an integer from 0 to 15")?,
        );
        Ok(())
    }

    pub(crate) fn validate(&self, format: Option<Format>) -> ExportResult<()> {
        if self.keys.is_some()
            && !matches!(
                format,
                Some(Format::GeoJson | Format::GeoJsonLines | Format::Csv)
            )
        {
            return Err("--keys requires GeoJSON, GeoJSONL, or CSV output".into());
        }
        if self.columns.is_some() && !matches!(format, Some(Format::Csv)) {
            return Err("--columns requires CSV output".into());
        }
        if matches!(format, Some(Format::Csv)) {
            if self.keys.is_some() && self.columns.is_some() {
                return Err("use either --keys or --columns for CSV output".into());
            }
            if self.keys.as_ref().is_some_and(Vec::is_empty) {
                return Err("CSV output requires at least one column".into());
            }
        }
        if self.precision.is_some() && format.is_none() {
            return Err("--precision requires GeoJSON, GeoJSONL, WKT, or CSV output".into());
        }
        Ok(())
    }
}

pub(crate) fn write<'a, S: Source + 'a>(
    features: impl IntoIterator<Item = regol::Result<Feature<'a, S>>>,
    output: &mut impl Write,
    format: Format,
    options: &Options,
) -> EncodeResult<()> {
    let precision = options.precision.unwrap_or(DEFAULT_PRECISION);
    match format {
        Format::GeoJson | Format::GeoJsonLines => {
            geojson::write(features, output, format, options, precision)?;
        }
        Format::Wkt => {
            let writer = GeometryWriter::new(Encoding::Wkt, precision);
            for feature in features {
                write_wkt_feature(output, feature?, &writer)?;
            }
        }
        Format::Csv => write_csv(features, output, &CsvWriter::new(options, precision))?,
    }
    Ok(())
}

pub(crate) fn write_parallel<'a, S: Source + Sync + 'a>(
    features: impl IntoIterator<Item = regol::Result<Feature<'a, S>>>,
    output: &mut impl Write,
    format: Format,
    options: &Options,
    threads: usize,
) -> EncodeResult<()> {
    if threads <= 1 {
        return write(features, output, format, options);
    }
    match format {
        Format::GeoJson | Format::GeoJsonLines => {
            geojson::write_parallel(features, output, format, options, threads)
        }
        Format::Wkt => {
            let precision = options.precision.unwrap_or(DEFAULT_PRECISION);
            let writer = GeometryWriter::new(Encoding::Wkt, precision);
            parallel::write(
                features,
                output,
                threads,
                |output, feature, _| write_wkt_feature(output, feature, &writer),
                |output, feature, _| write_wkt_feature(output, feature, &writer),
            )
        }
        Format::Csv => {
            let writer = CsvWriter::new(options, options.precision.unwrap_or(DEFAULT_PRECISION));
            if writer.has_geometry || writer.has_centroid {
                writer.header(output)?;
                parallel::write(
                    features,
                    output,
                    threads,
                    |output, feature, _| writer.row(output, feature),
                    |output, feature, _| writer.row(output, feature),
                )
            } else {
                write_csv(features, output, &writer)
            }
        }
    }
}

fn write_wkt_feature(
    output: &mut impl Write,
    feature: Feature<'_, impl Source>,
    writer: &GeometryWriter,
) -> EncodeResult<()> {
    writer.geometry(output, &feature_geometry(feature)?)?;
    output.write_all(b"\n")?;
    Ok(())
}

type EncodeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn feature_geometry(feature: Feature<'_, impl Source>) -> EncodeResult<Geometry> {
    feature
        .geometry()
        .map_err(|error| format!("{}: {error}", feature.id()).into())
}

fn write_properties(
    feature: Feature<'_, impl Source>,
    output: &mut impl Write,
    keys: Option<&[&str]>,
) -> EncodeResult<()> {
    let mut first = true;
    let mut property = |key: &str, value: TagValue<'_>| -> EncodeResult<()> {
        if !first {
            output.write_all(b",")?;
        }
        first = false;
        serde_json::to_writer(&mut *output, key)?;
        output.write_all(b":")?;
        match value {
            TagValue::Text(value) => serde_json::to_writer(&mut *output, value.as_ref())?,
            number => write!(output, "{number}")?,
        }
        Ok(())
    };
    if let Some(keys) = keys {
        for key in keys {
            if let Some(value) = feature.tag(key)? {
                property(key, value)?;
            }
        }
    } else {
        for tag in feature.tags()? {
            let tag = tag?;
            property(&tag.key, tag.value)?;
        }
    }
    Ok(())
}

fn write_csv<'a, S: Source + 'a>(
    features: impl IntoIterator<Item = regol::Result<Feature<'a, S>>>,
    output: &mut impl Write,
    writer: &CsvWriter<'_>,
) -> EncodeResult<()> {
    writer.header(output)?;
    for feature in features {
        writer.row(output, feature?)?;
    }
    Ok(())
}

struct CsvWriter<'a> {
    columns: Vec<(&'a str, Field<'a>)>,
    has_geometry: bool,
    has_centroid: bool,
    has_bounds: bool,
    writer: GeometryWriter,
}

impl<'a> CsvWriter<'a> {
    fn new(options: &'a Options, precision: usize) -> Self {
        let mut columns = Vec::new();
        let mut add = |name| {
            let field = Field::parse(name);
            if !columns.iter().any(|&(_, existing)| existing == field) {
                columns.push((name, field));
            }
        };
        if let Some(names) = options.columns.as_ref().or(options.keys.as_ref()) {
            for name in names {
                add(name.as_str());
            }
        } else {
            for name in ["id", "type", "geometry"] {
                add(name);
            }
        }
        let has_centroid = columns
            .iter()
            .any(|&(_, field)| matches!(field, Field::Latitude | Field::Longitude));
        let has_geometry = columns.iter().any(|&(_, field)| field == Field::Geometry);
        let has_bounds = columns.iter().any(|&(_, field)| field == Field::Bounds);
        let writer = GeometryWriter::new(Encoding::Wkt, precision);
        Self {
            columns,
            has_geometry,
            has_centroid,
            has_bounds,
            writer,
        }
    }

    fn header(&self, output: &mut impl Write) -> io::Result<()> {
        for (index, &(header, _)) in self.columns.iter().enumerate() {
            comma(output, index)?;
            write_csv_field(output, header)?;
        }
        output.write_all(b"\r\n")
    }

    fn row(&self, output: &mut impl Write, feature: Feature<'_, impl Source>) -> EncodeResult<()> {
        let mut integer = itoa::Buffer::new();
        let geometry = (self.has_geometry || self.has_centroid)
            .then(|| feature_geometry(feature))
            .transpose()?;
        let centroid = if self.has_centroid {
            geometry
                .as_ref()
                .and_then(Geometry::centroid)
                .map(|point| self.writer.coordinates(point))
        } else {
            None
        };
        let bounds = self.has_bounds.then(|| feature.bounds()).transpose()?;
        for (index, &(_, field)) in self.columns.iter().enumerate() {
            comma(output, index)?;
            match field {
                Field::Id => write_csv_id(output, &mut integer, feature.id())?,
                Field::Type => output.write_all(kind_name(feature.kind()).as_bytes())?,
                Field::OsmId => output.write_all(integer.format(feature.id().id).as_bytes())?,
                Field::Latitude | Field::Longitude => {
                    if let Some((lon, lat)) = centroid {
                        let value = if field == Field::Latitude { lat } else { lon };
                        self.writer.decimal(output, value)?;
                    }
                }
                Field::Bounds => {
                    output.write_all(b"\"")?;
                    self.writer
                        .bounds(output, bounds.expect("CSV bbox column"))?;
                    output.write_all(b"\"")?;
                }
                Field::Geometry => {
                    output.write_all(b"\"")?;
                    self.writer
                        .geometry(output, geometry.as_ref().expect("CSV geometry column"))?;
                    output.write_all(b"\"")?;
                }
                Field::Tags => {
                    output.write_all(b"\"{")?;
                    write_properties(feature, &mut CsvQuoted(output), None)?;
                    output.write_all(b"}\"")?;
                }
                Field::Tag(key) => match feature.tag(key)? {
                    Some(TagValue::Text(value)) => write_csv_field(output, &value)?,
                    Some(number) => write!(output, "{number}")?,
                    None => {}
                },
            }
        }
        output.write_all(b"\r\n")?;
        Ok(())
    }
}

fn write_csv_id(
    output: &mut impl Write,
    integer: &mut itoa::Buffer,
    id: regol::FeatureId,
) -> io::Result<()> {
    let digits = integer.format(id.id).as_bytes();
    let mut qualified = [0_u8; 21];
    qualified[0] = super::id_prefix(id.kind);
    qualified[1..=digits.len()].copy_from_slice(digits);
    output.write_all(&qualified[..=digits.len()])
}

fn comma(output: &mut impl Write, index: usize) -> io::Result<()> {
    if index > 0 {
        output.write_all(b",")?;
    }
    Ok(())
}

fn write_csv_field(output: &mut impl Write, field: &str) -> io::Result<()> {
    if field
        .bytes()
        .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'))
    {
        output.write_all(b"\"")?;
        csv_quoted_contents(output, field.as_bytes())?;
        output.write_all(b"\"")
    } else {
        output.write_all(field.as_bytes())
    }
}

fn csv_quoted_contents(output: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    for (index, part) in bytes.split(|&byte| byte == b'"').enumerate() {
        if index > 0 {
            output.write_all(b"\"\"")?;
        }
        output.write_all(part)?;
    }
    Ok(())
}

struct CsvQuoted<'a, W>(&'a mut W);

impl<W: Write> Write for CsvQuoted<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        csv_quoted_contents(self.0, bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[derive(Clone, Copy)]
enum Encoding {
    Json,
    Wkt,
}

struct GeometryWriter {
    encoding: Encoding,
    precision: usize,
    scale: i64,
}

impl GeometryWriter {
    fn new(encoding: Encoding, precision: usize) -> Self {
        Self {
            encoding,
            precision,
            scale: 10i64.pow(precision as u32),
        }
    }

    fn geometry(&self, output: &mut impl Write, geometry: &Geometry) -> io::Result<()> {
        let (json_name, wkt_name, empty) = match geometry {
            Geometry::Point(_) => ("Point", "POINT", false),
            Geometry::LineString(line) => ("LineString", "LINESTRING", line.is_empty()),
            Geometry::Polygon(_) => ("Polygon", "POLYGON", false),
            Geometry::MultiPolygon(polygons) => {
                ("MultiPolygon", "MULTIPOLYGON", polygons.is_empty())
            }
            Geometry::Collection(children) => (
                "GeometryCollection",
                "GEOMETRYCOLLECTION",
                children.is_empty(),
            ),
        };
        match self.encoding {
            Encoding::Json => {
                let field = if matches!(geometry, Geometry::Collection(_)) {
                    "geometries"
                } else {
                    "coordinates"
                };
                write!(output, "{{\"type\":\"{json_name}\",\"{field}\":")?;
            }
            Encoding::Wkt => {
                write!(output, "{wkt_name} ")?;
                if empty {
                    return output.write_all(b"EMPTY");
                }
            }
        }
        match geometry {
            Geometry::Point(point) if matches!(self.encoding, Encoding::Wkt) => {
                self.sequence(output, [point], |output, point| {
                    self.position(output, *point)
                })?
            }
            Geometry::Point(point) => self.position(output, *point)?,
            Geometry::LineString(line) => self.line(output, line.iter().copied())?,
            Geometry::Polygon(polygon) => self.polygon(output, polygon)?,
            Geometry::MultiPolygon(polygons) => {
                self.sequence(output, polygons, |output, polygon| {
                    self.polygon(output, polygon)
                })?
            }
            Geometry::Collection(children) => {
                self.sequence(output, children, |output, child| {
                    self.geometry(output, child)
                })?
            }
        }
        if matches!(self.encoding, Encoding::Json) {
            output.write_all(b"}")?;
        }
        Ok(())
    }

    fn cut_geometry(&self, output: &mut impl Write, geometry: &CutGeometry<'_>) -> io::Result<()> {
        let (kind, field) = match geometry {
            CutGeometry::Unchanged(geometry) => return self.geometry(output, geometry),
            CutGeometry::Lines(lines) if lines.len() == 1 => ("LineString", "coordinates"),
            CutGeometry::Lines(_) => ("MultiLineString", "coordinates"),
            CutGeometry::Polygons { multi: false, .. } => ("Polygon", "coordinates"),
            CutGeometry::Polygons { .. } => ("MultiPolygon", "coordinates"),
            CutGeometry::Collection(_) => ("GeometryCollection", "geometries"),
        };
        write!(output, "{{\"type\":\"{kind}\",\"{field}\":")?;
        let line = |output: &mut _, line: &geo::LineString| {
            self.sequence(output, &line.0, |output, &coord| {
                self.write_position(output, self.rounded(coord))
            })
        };
        match geometry {
            CutGeometry::Unchanged(_) => unreachable!(),
            CutGeometry::Lines(lines) if lines.len() == 1 => line(output, &lines[0])?,
            CutGeometry::Lines(lines) => self.sequence(output, lines, line)?,
            CutGeometry::Polygons {
                pieces,
                multi: false,
            } => self.cut_polygon(output, &pieces[0])?,
            CutGeometry::Polygons { pieces, .. } => {
                self.sequence(output, pieces, |output, piece| {
                    self.cut_polygon(output, piece)
                })?
            }
            CutGeometry::Collection(children) => {
                self.sequence(output, children, |output, child| {
                    self.cut_geometry(output, child)
                })?
            }
        }
        output.write_all(b"}")
    }

    fn polygon(&self, output: &mut impl Write, polygon: &Polygon) -> io::Result<()> {
        let rings = std::iter::once(&polygon.exterior).chain(&polygon.interiors);
        self.sequence(output, rings.enumerate(), |output, (index, ring)| {
            self.ring(
                output,
                ring.iter().map(|&point| self.coordinates(point)),
                index == 0,
            )
        })
    }

    fn cut_polygon(&self, output: &mut impl Write, polygon: &geo::Polygon) -> io::Result<()> {
        let rings = std::iter::once(polygon.exterior()).chain(polygon.interiors());
        self.sequence(output, rings.enumerate(), |output, (index, ring)| {
            self.ring(
                output,
                ring.0.iter().map(|&coord| self.rounded(coord)),
                index == 0,
            )
        })
    }

    fn ring(
        &self,
        output: &mut impl Write,
        coords: impl Clone + DoubleEndedIterator<Item = (i64, i64)>,
        exterior: bool,
    ) -> io::Result<()> {
        let opposite = if exterior {
            Ordering::Less
        } else {
            Ordering::Greater
        };
        if matches!(self.encoding, Encoding::Json) && Self::ring_winding(coords.clone()) == opposite
        {
            self.sequence(output, coords.rev(), |output, coord| {
                self.write_position(output, coord)
            })
        } else {
            self.sequence(output, coords, |output, coord| {
                self.write_position(output, coord)
            })
        }
    }

    fn line(
        &self,
        output: &mut impl Write,
        points: impl IntoIterator<Item = Point>,
    ) -> io::Result<()> {
        self.sequence(output, points, |output, point| self.position(output, point))
    }

    fn position(&self, output: &mut impl Write, point: Point) -> io::Result<()> {
        self.write_position(output, self.coordinates(point))
    }

    fn write_position(&self, output: &mut impl Write, (lon, lat): (i64, i64)) -> io::Result<()> {
        match self.encoding {
            Encoding::Json => self.sequence(output, [lon, lat], |output, value| {
                self.decimal(output, value)
            }),
            Encoding::Wkt => {
                self.decimal(output, lon)?;
                output.write_all(b" ")?;
                self.decimal(output, lat)
            }
        }
    }

    fn bounds(&self, output: &mut impl Write, bounds: Bounds) -> io::Result<()> {
        self.write_bounds(output, self.bounds_coordinates(bounds))
    }

    fn bounds_coordinates(&self, bounds: Bounds) -> [i64; 4] {
        let (west, south) = self.coordinates(bounds.min);
        let (east, north) = self.coordinates(bounds.max);
        [west, south, east, north]
    }

    fn cut_bounds(&self, [west, south, east, north]: [f64; 4]) -> [i64; 4] {
        let mut bounds = [west, south, east, north].map(|value| self.round(value));
        if west > east && bounds[0] <= bounds[2] {
            // The gap in a wrapping bbox can vanish at low precision.
            bounds[0] = -180 * self.scale;
            bounds[2] = 180 * self.scale;
        }
        bounds
    }

    fn write_bounds(&self, output: &mut impl Write, bounds: [i64; 4]) -> io::Result<()> {
        for (index, value) in bounds.into_iter().enumerate() {
            comma(output, index)?;
            self.decimal(output, value)?;
        }
        Ok(())
    }

    fn coordinates(&self, point: Point) -> (i64, i64) {
        self.rounded(degrees(point))
    }

    fn rounded(&self, coord: geo::Coord) -> (i64, i64) {
        (self.round(coord.x), self.round(coord.y))
    }

    fn round(&self, value: f64) -> i64 {
        (value * self.scale as f64).round_ties_even() as i64
    }

    fn decimal(&self, output: &mut impl Write, value: i64) -> io::Result<()> {
        let mut number = itoa::Buffer::new();
        if self.precision == 0 {
            return output.write_all(number.format(value).as_bytes());
        }

        let digits = number.format(value.unsigned_abs()).as_bytes();
        let mut bytes = [b'0'; 21];
        let sign = usize::from(value < 0);
        if sign != 0 {
            bytes[0] = b'-';
        }
        let integer_len = digits.len().saturating_sub(self.precision);
        let dot = sign + integer_len.max(1);
        bytes[dot] = b'.';
        let end = dot + 1 + self.precision;
        if integer_len != 0 {
            bytes[sign..dot].copy_from_slice(&digits[..integer_len]);
        }
        let fraction = &digits[integer_len..];
        bytes[end - fraction.len()..end].copy_from_slice(fraction);
        output.write_all(&bytes[..end])
    }

    fn ring_winding(mut ring: impl Iterator<Item = (i64, i64)>) -> Ordering {
        let Some(mut previous) = ring.next() else {
            return Ordering::Equal;
        };
        let mut high = 0i128;
        let mut low = 0i128;
        // Projection and rounding can each flip a ring. Use the exact
        // published decimal positions. Each cross product fits in i128, and
        // summing its two halves separately avoids overflow for large rings.
        for next in ring {
            let cross = i128::from(previous.0) * i128::from(next.1)
                - i128::from(next.0) * i128::from(previous.1);
            high += cross >> 64;
            low += i128::from(cross as u64);
            previous = next;
        }
        (high + (low >> 64), low as u64).cmp(&(0, 0))
    }

    fn sequence<W: Write, T>(
        &self,
        output: &mut W,
        items: impl IntoIterator<Item = T>,
        mut item: impl FnMut(&mut W, T) -> io::Result<()>,
    ) -> io::Result<()> {
        let (open, close) = match self.encoding {
            Encoding::Json => (b"[", b"]"),
            Encoding::Wkt => (b"(", b")"),
        };
        output.write_all(open)?;
        for (index, value) in items.into_iter().enumerate() {
            comma(output, index)?;
            item(output, value)?;
        }
        output.write_all(close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallel::{BATCH_SIZE, CHUNK_SIZE, MIN_PARALLEL_SIZE};
    use serde_json::{Value, json};

    #[test]
    fn parallel_output_matches_serial_across_batches_and_query_errors() {
        let library =
            regol::Library::new(include_bytes!("../../tests/fixtures/antimeridian.gol").as_slice())
                .unwrap();
        let all = library
            .features()
            .collect::<regol::Result<Vec<_>>>()
            .unwrap();
        let wkt_options = Options {
            precision: Some(MAX_PRECISION),
            ..Options::default()
        };
        let mut geojson_options = Options {
            precision: Some(MAX_PRECISION),
            ..Options::default()
        };
        geojson_options.set_keys("bbox,name,tag:bbox,name").unwrap();
        let mut csv_options = Options {
            precision: Some(MAX_PRECISION),
            ..Options::default()
        };
        csv_options
            .set_columns("id,geom,geometry,lat,lon,bbox,name,tags")
            .unwrap();
        for (format, options) in [
            (Format::GeoJson, &geojson_options),
            (Format::GeoJsonLines, &geojson_options),
            (Format::Wkt, &wkt_options),
            (Format::Csv, &csv_options),
        ] {
            for (count, fail) in [
                (0, false),
                (MIN_PARALLEL_SIZE - 1, false),
                (MIN_PARALLEL_SIZE, false),
                (BATCH_SIZE, false),
                (BATCH_SIZE + CHUNK_SIZE + 1, false),
                (MIN_PARALLEL_SIZE + 1, true),
            ] {
                let features = || {
                    all.iter()
                        .copied()
                        .cycle()
                        .take(count)
                        .map(Ok)
                        .chain(fail.then_some(Err(regol::Error::Limit("test query"))))
                };
                let mut serial = Vec::new();
                let mut parallel = Vec::new();
                let serial_result = write(features(), &mut serial, format, options);
                let parallel_result = write_parallel(features(), &mut parallel, format, options, 4);
                assert_eq!(
                    serial_result.map_err(|error| error.to_string()),
                    parallel_result.map_err(|error| error.to_string())
                );
                assert!(serial == parallel, "output differs for {count} records");
            }
        }
    }

    #[test]
    fn multipolygon_keeps_components_and_holes() {
        let point = |x, y| Point {
            x: x * 11_930_465,
            y: y * 11_930_465,
        };
        let ring = |x, y, size| {
            vec![
                point(x, y),
                point(x, y + size),
                point(x + size, y + size),
                point(x + size, y),
                point(x, y),
            ]
        };
        let geometry = Geometry::MultiPolygon(vec![
            Polygon {
                exterior: ring(0, 0, 3),
                interiors: vec![ring(1, 1, 1)],
            },
            Polygon {
                exterior: ring(4, 0, 1),
                interiors: Vec::new(),
            },
        ]);
        let mut output = Vec::new();
        GeometryWriter::new(Encoding::Json, 0)
            .geometry(&mut output, &geometry)
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&output).unwrap(),
            json!({
                "type": "MultiPolygon",
                "coordinates": [
                    [
                        [[0,0],[3,0],[3,3],[0,3],[0,0]],
                        [[1,1],[1,2],[2,2],[2,1],[1,1]]
                    ],
                    [[[4,0],[5,0],[5,1],[4,1],[4,0]]]
                ]
            })
        );
        output.clear();
        GeometryWriter::new(Encoding::Wkt, 0)
            .geometry(&mut output, &geometry)
            .unwrap();
        assert_eq!(
            output,
            b"MULTIPOLYGON (((0 0,0 3,3 3,3 0,0 0),(1 1,1 2,2 2,2 1,1 1)),((4 0,4 1,5 1,5 0,4 0)))"
        );
    }

    #[test]
    fn geojson_winds_a_ring_after_geographic_projection() {
        let point = |x, y| Point { x, y };
        // The middle vertex is below the chord in Mercator and above it
        // after converting the three vertices to longitude and latitude.
        let first = point(-600_000_000, 0);
        let geometry = Geometry::Polygon(Polygon {
            exterior: vec![
                first,
                point(0, 600_000_000),
                point(600_000_000, 1_400_000_000),
                first,
            ],
            interiors: Vec::new(),
        });
        let mut output = Vec::new();
        GeometryWriter::new(Encoding::Json, 7)
            .geometry(&mut output, &geometry)
            .unwrap();
        let value: Value = serde_json::from_slice(&output).unwrap();
        let ring = value["coordinates"][0].as_array().unwrap();
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.first(), ring.last());
        assert!(ring[1][0].as_f64().unwrap() > 0.0);
        assert_eq!(ring[2][0], json!(0.0));
    }

    #[test]
    fn geojson_winds_using_rounded_coordinates_and_keeps_ring_closure() {
        let point = |lon: f64, lat: f64| Point {
            x: (lon * 11_930_465.0).round() as i32,
            y: (lat * 11_930_465.0).round() as i32,
        };
        // The triangle winds clockwise at full precision and counterclockwise
        // when each coordinate is rounded to the nearest degree.
        let first = point(0.01, 0.01);
        let exterior = vec![first, point(0.51, 0.49), point(1.49, 0.51), first];
        let writer = GeometryWriter::new(Encoding::Json, 0);
        assert_eq!(
            GeometryWriter::ring_winding(exterior.iter().map(|&point| writer.coordinates(point))),
            Ordering::Greater
        );
        let precise = GeometryWriter::new(Encoding::Json, 15);
        assert_eq!(
            GeometryWriter::ring_winding(exterior.iter().map(|&point| precise.coordinates(point))),
            Ordering::Less
        );

        let geometry = Geometry::Polygon(Polygon {
            exterior,
            interiors: Vec::new(),
        });
        let mut output = Vec::new();
        writer.geometry(&mut output, &geometry).unwrap();
        let value: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            value["coordinates"],
            json!([[[0, 0], [1, 0], [1, 1], [0, 0]]])
        );

        // A ring that collapses at this precision keeps all positions.
        let geometry = Geometry::Polygon(Polygon {
            exterior: vec![first, point(0.2, 0.1), point(0.1, 0.2), first],
            interiors: Vec::new(),
        });
        output.clear();
        writer.geometry(&mut output, &geometry).unwrap();
        let value: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            value["coordinates"],
            json!([[[0, 0], [0, 0], [0, 0], [0, 0]]])
        );
    }

    #[test]
    fn wkt_handles_empty_geometries() {
        for (geometry, expected) in [
            (Geometry::LineString(Vec::new()), "LINESTRING EMPTY"),
            (Geometry::MultiPolygon(Vec::new()), "MULTIPOLYGON EMPTY"),
            (Geometry::Collection(Vec::new()), "GEOMETRYCOLLECTION EMPTY"),
        ] {
            let mut output = Vec::new();
            GeometryWriter::new(Encoding::Wkt, 7)
                .geometry(&mut output, &geometry)
                .unwrap();
            assert_eq!(output, expected.as_bytes());
        }
    }

    #[test]
    fn csv_field_preserves_quotes_unicode_and_line_breaks() {
        let mut output = Vec::new();
        write_csv_field(&mut output, "Café,\"London\"\r\nNew line").unwrap();
        assert_eq!(output, "\"Café,\"\"London\"\"\r\nNew line\"".as_bytes());
    }

    #[test]
    fn csv_id_matches_feature_display() {
        let mut integer = itoa::Buffer::new();
        for kind in [
            regol::FeatureType::Node,
            regol::FeatureType::Way,
            regol::FeatureType::Relation,
        ] {
            for id in [0, u64::MAX] {
                let id = regol::FeatureId { kind, id };
                let mut output = Vec::new();
                write_csv_id(&mut output, &mut integer, id).unwrap();
                assert_eq!(output, id.to_string().as_bytes());
            }
        }
    }

    #[test]
    fn coordinate_precision_normalizes_zero_and_keeps_endpoints_in_range() {
        let geometry = Geometry::Point(Point { x: i32::MIN, y: -1 });
        let mut output = Vec::new();
        GeometryWriter::new(Encoding::Wkt, 6)
            .geometry(&mut output, &geometry)
            .unwrap();
        assert_eq!(output, b"POINT (-180.000000 0.000000)");

        for precision in [0, DEFAULT_PRECISION, MAX_PRECISION] {
            let writer = GeometryWriter::new(Encoding::Json, precision);
            for (endpoint, longitude) in [(i32::MIN, -180.0), (i32::MAX, 180.0)] {
                let geometry = Geometry::Point(Point {
                    x: endpoint,
                    y: endpoint,
                });
                output.clear();
                writer.geometry(&mut output, &geometry).unwrap();
                let value: Value = serde_json::from_slice(&output).unwrap();
                let lon = value["coordinates"][0].as_f64().unwrap();
                let lat = value["coordinates"][1].as_f64().unwrap();
                assert!((-180.0..=180.0).contains(&lon));
                assert!((lon - longitude).abs() < 1.0e-7);
                assert!((-90.0..=90.0).contains(&lat));
            }
        }
    }

    #[test]
    fn text_exports_return_output_io_errors() {
        struct ClosedOutput;

        impl Write for ClosedOutput {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let library =
            regol::Library::new(include_bytes!("../../tests/fixtures/basic.gol").as_slice())
                .unwrap();
        let query = regol::Query::parse("n[amenity=cafe]").unwrap();
        let prepared = query.prepare(&library).unwrap();
        for format in [
            Format::GeoJson,
            Format::GeoJsonLines,
            Format::Wkt,
            Format::Csv,
        ] {
            let error = write(
                prepared.iter(),
                &mut ClosedOutput,
                format,
                &Options::default(),
            )
            .unwrap_err();
            assert_eq!(
                error.downcast_ref::<io::Error>().unwrap().kind(),
                io::ErrorKind::BrokenPipe
            );
        }
    }
}
