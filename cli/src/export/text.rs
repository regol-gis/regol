use super::{ExportResult, antimeridian::CutGeometry, kind_name};
use regol::{Bounds, Feature, FeatureType, Geometry, Point, Polygon, TagValue, source::Source};
use std::{
    cmp::Ordering,
    io::{self, Write},
    sync::Mutex,
};

mod geojson;
mod parallel;

const DEFAULT_PRECISION: usize = 7;
const MAX_PRECISION: usize = 15;
const FLUSH_THRESHOLD: usize = 64 * 1024;
const UNORDERED_PREFIX_STEPS: u64 = 512;
const UNORDERED_CHUNK_BYTES: usize = 128 * 1024;

static DIGITS2: &[u8; 200] = b"0001020304050607080910111213141516171819\
2021222324252627282930313233343536373839\
4041424344454647484950515253545556575859\
6061626364656667686970717273747576777879\
8081828384858687888990919293949596979899";

#[derive(Default)]
pub(super) struct Scratch {
    pub(super) points: Vec<Point>,
    pub(super) ring_coords: Vec<(i64, i64)>,
    pub(super) tag_buf: Vec<u8>,
    pub(super) integer: itoa::Buffer,
}

#[inline]
pub(super) fn flush_buf(output: &mut impl Write, buf: &mut Vec<u8>) -> io::Result<()> {
    if buf.len() >= FLUSH_THRESHOLD {
        output.write_all(buf)?;
        buf.clear();
    }
    Ok(())
}

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
            let mut buf = Vec::with_capacity(FLUSH_THRESHOLD);
            let mut scratch = Scratch::default();
            for feature in features {
                let feature = match feature {
                    Ok(feature) => feature,
                    Err(error) => {
                        if !buf.is_empty() {
                            output.write_all(&buf)?;
                        }
                        return Err(error.into());
                    }
                };
                write_wkt_feature_into(&mut buf, feature, &writer, &mut scratch)?;
                flush_buf(output, &mut buf)?;
            }
            if !buf.is_empty() {
                output.write_all(&buf)?;
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
                &|output, feature, _, buf, scratch| {
                    write_wkt_feature_into(buf, feature, &writer, scratch)?;
                    flush_buf(output, buf)?;
                    Ok(())
                },
                &|buf, feature, _, scratch| write_wkt_feature_into(buf, feature, &writer, scratch),
            )
        }
        Format::Csv => {
            let writer = CsvWriter::new(options, options.precision.unwrap_or(DEFAULT_PRECISION));
            writer.header(output)?;
            parallel::write(
                features,
                output,
                threads,
                &|output, feature, _, buf, scratch| {
                    writer.row_into(buf, feature, scratch)?;
                    flush_buf(output, buf)?;
                    Ok(())
                },
                &|buf, feature, _, scratch| writer.row_into(buf, feature, scratch),
            )
        }
    }
}

struct WorkerBuf {
    buf: Vec<u8>,
    scratch: Scratch,
}

impl Default for WorkerBuf {
    fn default() -> Self {
        Self {
            buf: Vec::with_capacity(UNORDERED_CHUNK_BYTES),
            scratch: Scratch::default(),
        }
    }
}

enum PartitionEncoder<'a> {
    Ids,
    GeoJson(geojson::Encoder<'a>),
    Wkt(GeometryWriter),
    Csv(CsvWriter<'a>),
}

impl PartitionEncoder<'_> {
    #[inline(never)]
    fn encode<S: Source>(
        &self,
        buf: &mut Vec<u8>,
        feature: Feature<'_, S>,
        first: bool,
        scratch: &mut Scratch,
    ) -> EncodeResult<()> {
        match self {
            Self::Ids => {
                push_feature_id(buf, &mut scratch.integer, feature.id());
                buf.push(b'\n');
                Ok(())
            }
            Self::GeoJson(encoder) => encoder.feature_into(buf, feature, first, scratch),
            Self::Wkt(writer) => write_wkt_feature_into(buf, feature, writer, scratch),
            Self::Csv(writer) => writer.row_into(buf, feature, scratch),
        }
    }
}

fn run_partitioned<S: Source + Sync, W: Write + Send>(
    prepared: &regol::PreparedQuery<'_, '_, S>,
    bounds: Option<Bounds>,
    area: Option<&crate::area::Area>,
    sink: &Mutex<(&mut W, bool)>,
    threads: usize,
    collection: bool,
    encoder: &PartitionEncoder<'_>,
) -> EncodeResult<()> {
    let first_err: Mutex<Option<Box<dyn std::error::Error + Send + Sync>>> = Mutex::new(None);
    let flush_chunk = |buf: &mut Vec<u8>| -> regol::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let mut guard = sink.lock().unwrap();
        let (writer, first_written) = &mut *guard;
        let res = (|| -> io::Result<()> {
            if collection && !*first_written {
                writer.write_all(b",")?;
            }
            writer.write_all(buf)?;
            *first_written = false;
            Ok(())
        })();
        buf.clear();
        if let Err(err) = res {
            let mut e = first_err.lock().unwrap();
            if e.is_none() {
                *e = Some(Box::new(err));
            }
            return Err(regol::Error::Io(io::Error::other("output write error")));
        }
        Ok(())
    };

    let res = regol::QueryExecutor::try_fold_reduce_once(
        threads,
        prepared,
        bounds,
        UNORDERED_PREFIX_STEPS,
        WorkerBuf::default,
        |mut acc, feature| {
            if let Some(area) = area {
                if !area.intersects_feature(feature)? {
                    return Ok(acc);
                }
            }
            let first_in_buf = acc.buf.is_empty();
            if let Err(err) = encoder.encode(&mut acc.buf, feature, first_in_buf, &mut acc.scratch)
            {
                let mut e = first_err.lock().unwrap();
                if e.is_none() {
                    *e = Some(err);
                }
                return Err(regol::Error::Io(io::Error::other("encode error")));
            }
            if acc.buf.len() >= UNORDERED_CHUNK_BYTES {
                flush_chunk(&mut acc.buf)?;
            }
            Ok(acc)
        },
        |mut left, mut right| {
            flush_chunk(&mut left.buf)?;
            flush_chunk(&mut right.buf)?;
            Ok(left)
        },
    );
    match res {
        Ok(mut last) => {
            if let Err(err) = flush_chunk(&mut last.buf) {
                if let Some(e) = first_err.lock().unwrap().take() {
                    return Err(e);
                }
                return Err(err.into());
            }
            Ok(())
        }
        Err(err) => {
            if let Some(e) = first_err.lock().unwrap().take() {
                return Err(e);
            }
            Err(err.into())
        }
    }
}

pub(crate) fn write_ids_partitioned<S: Source + Sync, W: Write + Send>(
    prepared: &regol::PreparedQuery<'_, '_, S>,
    bounds: Option<Bounds>,
    area: Option<&crate::area::Area>,
    output: &mut W,
    threads: usize,
) -> EncodeResult<()> {
    let sink = Mutex::new((output, true));
    run_partitioned(
        prepared,
        bounds,
        area,
        &sink,
        threads,
        false,
        &PartitionEncoder::Ids,
    )
}

pub(crate) fn write_partitioned<S: Source + Sync, W: Write + Send>(
    prepared: &regol::PreparedQuery<'_, '_, S>,
    bounds: Option<Bounds>,
    area: Option<&crate::area::Area>,
    output: &mut W,
    format: Format,
    options: &Options,
    threads: usize,
) -> EncodeResult<()> {
    let precision = options.precision.unwrap_or(DEFAULT_PRECISION);
    let sink = Mutex::new((output, true));
    match format {
        Format::GeoJson | Format::GeoJsonLines => {
            let encoder = geojson::Encoder::new(format, options, precision);
            let collection = encoder.collection;
            encoder.begin(sink.lock().unwrap().0)?;
            let wrapped = PartitionEncoder::GeoJson(encoder);
            run_partitioned(prepared, bounds, area, &sink, threads, collection, &wrapped)?;
            if let PartitionEncoder::GeoJson(encoder) = wrapped {
                encoder.end(sink.lock().unwrap().0)?;
            }
        }
        Format::Wkt => {
            let wrapped = PartitionEncoder::Wkt(GeometryWriter::new(Encoding::Wkt, precision));
            run_partitioned(prepared, bounds, area, &sink, threads, false, &wrapped)?;
        }
        Format::Csv => {
            let writer = CsvWriter::new(options, precision);
            writer.header(sink.lock().unwrap().0)?;
            let wrapped = PartitionEncoder::Csv(writer);
            run_partitioned(prepared, bounds, area, &sink, threads, false, &wrapped)?;
        }
    }
    Ok(())
}

fn write_wkt_feature_into(
    out: &mut Vec<u8>,
    feature: Feature<'_, impl Source>,
    writer: &GeometryWriter,
    scratch: &mut Scratch,
) -> EncodeResult<()> {
    let id = feature.id();
    match id.kind {
        FeatureType::Node => {
            let point = feature
                .point()
                .map_err(|error| format!("{id}: {error}"))?
                .ok_or_else(|| format!("{id}: wrong feature type"))?;
            out.extend_from_slice(b"POINT (");
            writer.push_point(out, point, &mut scratch.integer);
            out.extend_from_slice(b")\n");
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
            if feature.is_area() {
                if scratch.points.len() < 4 || scratch.points.first() != scratch.points.last() {
                    return Err(format!(
                        "{id}: ring is unclosed or has fewer than four coordinates"
                    )
                    .into());
                }
                out.extend_from_slice(b"POLYGON (");
                writer.push_line_points(out, &scratch.points, &mut scratch.integer);
                out.extend_from_slice(b")\n");
            } else if scratch.points.is_empty() {
                out.extend_from_slice(b"LINESTRING EMPTY\n");
            } else {
                out.extend_from_slice(b"LINESTRING ");
                writer.push_line_points(out, &scratch.points, &mut scratch.integer);
                out.push(b'\n');
            }
        }
        FeatureType::Relation => {
            let geometry = feature_geometry(feature)?;
            writer.push_geometry(out, &geometry, scratch);
            out.push(b'\n');
        }
    }
    Ok(())
}

type EncodeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn feature_geometry(feature: Feature<'_, impl Source>) -> EncodeResult<Geometry> {
    feature
        .geometry()
        .map_err(|error| format!("{}: {error}", feature.id()).into())
}

#[inline]
pub(super) fn push_json_str(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b < 0x20 || b == b'"' || b == b'\\' {
            out.extend_from_slice(&bytes[start..i]);
            match b {
                b'"' => out.extend_from_slice(b"\\\""),
                b'\\' => out.extend_from_slice(b"\\\\"),
                0x08 => out.extend_from_slice(b"\\b"),
                0x0c => out.extend_from_slice(b"\\f"),
                b'\n' => out.extend_from_slice(b"\\n"),
                b'\r' => out.extend_from_slice(b"\\r"),
                b'\t' => out.extend_from_slice(b"\\t"),
                c => {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    out.extend_from_slice(&[
                        b'\\',
                        b'u',
                        b'0',
                        b'0',
                        HEX[(c >> 4) as usize],
                        HEX[(c & 0xf) as usize],
                    ]);
                }
            }
            start = i + 1;
        }
    }
    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}

#[inline]
pub(crate) fn push_tag_number(
    out: &mut Vec<u8>,
    mantissa: i32,
    scale: u8,
    integer: &mut itoa::Buffer,
) {
    if scale == 0 {
        out.extend_from_slice(integer.format(mantissa).as_bytes());
        return;
    }
    let scale = usize::from(scale);
    if mantissa < 0 {
        out.push(b'-');
    }
    let abs = i64::from(mantissa).unsigned_abs();
    let digits = integer.format(abs).as_bytes();
    if digits.len() <= scale {
        out.extend_from_slice(b"0.");
        let zeroes = scale - digits.len();
        for _ in 0..zeroes {
            out.push(b'0');
        }
        out.extend_from_slice(digits);
    } else {
        let split = digits.len() - scale;
        out.extend_from_slice(&digits[..split]);
        out.push(b'.');
        out.extend_from_slice(&digits[split..]);
    }
}

pub(super) fn push_properties(
    feature: Feature<'_, impl Source>,
    out: &mut Vec<u8>,
    keys: Option<&[&str]>,
    integer: &mut itoa::Buffer,
) -> EncodeResult<()> {
    let mut first = true;
    let mut property = |key: &str, value: TagValue<'_>| {
        if !first {
            out.push(b',');
        }
        first = false;
        push_json_str(out, key);
        out.push(b':');
        match value {
            TagValue::Text(value) => push_json_str(out, value.as_ref()),
            TagValue::Number { mantissa, scale } => {
                push_tag_number(out, mantissa, scale, integer);
            }
        }
    };
    if let Some(keys) = keys {
        for key in keys {
            if let Some(value) = feature.tag(key)? {
                property(key, value);
            }
        }
    } else {
        for tag in feature.tags()? {
            let tag = tag?;
            property(&tag.key, tag.value);
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
    let mut buf = Vec::with_capacity(FLUSH_THRESHOLD);
    let mut scratch = Scratch::default();
    for feature in features {
        let feature = match feature {
            Ok(feature) => feature,
            Err(error) => {
                if !buf.is_empty() {
                    output.write_all(&buf)?;
                }
                return Err(error.into());
            }
        };
        writer.row_into(&mut buf, feature, &mut scratch)?;
        flush_buf(output, &mut buf)?;
    }
    if !buf.is_empty() {
        output.write_all(&buf)?;
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

    fn row_into(
        &self,
        out: &mut Vec<u8>,
        feature: Feature<'_, impl Source>,
        scratch: &mut Scratch,
    ) -> EncodeResult<()> {
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
            if index > 0 {
                out.push(b',');
            }
            match field {
                Field::Id => push_feature_id(out, &mut scratch.integer, feature.id()),
                Field::Type => out.extend_from_slice(kind_name(feature.kind()).as_bytes()),
                Field::OsmId => {
                    out.extend_from_slice(scratch.integer.format(feature.id().id).as_bytes())
                }
                Field::Latitude | Field::Longitude => {
                    if let Some((lon, lat)) = centroid {
                        let value = if field == Field::Latitude { lat } else { lon };
                        self.writer.push_decimal(out, value, &mut scratch.integer);
                    }
                }
                Field::Bounds => {
                    out.push(b'"');
                    let coords = self
                        .writer
                        .bounds_coordinates(bounds.expect("CSV bbox column"));
                    self.writer.push_bounds(out, coords, &mut scratch.integer);
                    out.push(b'"');
                }
                Field::Geometry => {
                    out.push(b'"');
                    self.writer.push_geometry(
                        out,
                        geometry.as_ref().expect("CSV geometry column"),
                        scratch,
                    );
                    out.push(b'"');
                }
                Field::Tags => {
                    scratch.tag_buf.clear();
                    scratch.tag_buf.push(b'{');
                    push_properties(feature, &mut scratch.tag_buf, None, &mut scratch.integer)?;
                    scratch.tag_buf.push(b'}');
                    out.push(b'"');
                    push_csv_quoted_contents(out, &scratch.tag_buf);
                    out.push(b'"');
                }
                Field::Tag(key) => match feature.tag(key)? {
                    Some(TagValue::Text(value)) => push_csv_field(out, &value),
                    Some(TagValue::Number { mantissa, scale }) => {
                        push_tag_number(out, mantissa, scale, &mut scratch.integer);
                    }
                    None => {}
                },
            }
        }
        out.extend_from_slice(b"\r\n");
        Ok(())
    }
}

#[inline]
pub(super) fn push_feature_id(out: &mut Vec<u8>, integer: &mut itoa::Buffer, id: regol::FeatureId) {
    out.push(super::id_prefix(id.kind));
    out.extend_from_slice(integer.format(id.id).as_bytes());
}

fn comma(output: &mut impl Write, index: usize) -> io::Result<()> {
    if index > 0 {
        output.write_all(b",")?;
    }
    Ok(())
}

fn write_csv_field(output: &mut impl Write, field: &str) -> io::Result<()> {
    let mut buf = Vec::with_capacity(field.len() + 2);
    push_csv_field(&mut buf, field);
    output.write_all(&buf)
}

#[inline]
fn push_csv_field(out: &mut Vec<u8>, field: &str) {
    let bytes = field.as_bytes();
    if bytes
        .iter()
        .any(|&byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'))
    {
        out.push(b'"');
        push_csv_quoted_contents(out, bytes);
        out.push(b'"');
    } else {
        out.extend_from_slice(bytes);
    }
}

#[inline]
fn push_csv_quoted_contents(out: &mut Vec<u8>, bytes: &[u8]) {
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'"' {
            out.extend_from_slice(&bytes[start..i]);
            out.extend_from_slice(b"\"\"");
            start = i + 1;
        }
    }
    out.extend_from_slice(&bytes[start..]);
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

    fn push_geometry(&self, out: &mut Vec<u8>, geometry: &Geometry, scratch: &mut Scratch) {
        let (json_header, wkt_name, empty) = match geometry {
            Geometry::Point(_) => (
                &b"{\"type\":\"Point\",\"coordinates\":"[..],
                &b"POINT "[..],
                false,
            ),
            Geometry::LineString(line) => (
                &b"{\"type\":\"LineString\",\"coordinates\":"[..],
                &b"LINESTRING "[..],
                line.is_empty(),
            ),
            Geometry::Polygon(_) => (
                &b"{\"type\":\"Polygon\",\"coordinates\":"[..],
                &b"POLYGON "[..],
                false,
            ),
            Geometry::MultiPolygon(polygons) => (
                &b"{\"type\":\"MultiPolygon\",\"coordinates\":"[..],
                &b"MULTIPOLYGON "[..],
                polygons.is_empty(),
            ),
            Geometry::Collection(children) => (
                &b"{\"type\":\"GeometryCollection\",\"geometries\":"[..],
                &b"GEOMETRYCOLLECTION "[..],
                children.is_empty(),
            ),
        };
        match self.encoding {
            Encoding::Json => out.extend_from_slice(json_header),
            Encoding::Wkt => {
                out.extend_from_slice(wkt_name);
                if empty {
                    out.extend_from_slice(b"EMPTY");
                    return;
                }
            }
        }
        match geometry {
            Geometry::Point(point) if matches!(self.encoding, Encoding::Wkt) => {
                out.push(b'(');
                self.push_point(out, *point, &mut scratch.integer);
                out.push(b')');
            }
            Geometry::Point(point) => self.push_point(out, *point, &mut scratch.integer),
            Geometry::LineString(line) => {
                self.push_line_points(out, line, &mut scratch.integer);
            }
            Geometry::Polygon(polygon) => self.push_polygon(out, polygon, scratch),
            Geometry::MultiPolygon(polygons) => {
                let (open, close) = self.delims();
                out.push(open);
                for (index, polygon) in polygons.iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    self.push_polygon(out, polygon, scratch);
                }
                out.push(close);
            }
            Geometry::Collection(children) => {
                let (open, close) = self.delims();
                out.push(open);
                for (index, child) in children.iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    self.push_geometry(out, child, scratch);
                }
                out.push(close);
            }
        }
        if matches!(self.encoding, Encoding::Json) {
            out.push(b'}');
        }
    }

    #[inline(never)]
    pub(super) fn push_cut_geometry(
        &self,
        out: &mut Vec<u8>,
        geometry: &CutGeometry<'_>,
        scratch: &mut Scratch,
    ) {
        let header: &[u8] = match geometry {
            CutGeometry::Unchanged(geometry) => {
                self.push_geometry(out, geometry, scratch);
                return;
            }
            CutGeometry::Lines(lines) if lines.len() == 1 => {
                b"{\"type\":\"LineString\",\"coordinates\":"
            }
            CutGeometry::Lines(_) => b"{\"type\":\"MultiLineString\",\"coordinates\":",
            CutGeometry::Polygons { multi: false, .. } => b"{\"type\":\"Polygon\",\"coordinates\":",
            CutGeometry::Polygons { .. } => b"{\"type\":\"MultiPolygon\",\"coordinates\":",
            CutGeometry::Collection(_) => b"{\"type\":\"GeometryCollection\",\"geometries\":",
        };
        out.extend_from_slice(header);
        let push_line = |out: &mut Vec<u8>, line: &geo::LineString, integer: &mut itoa::Buffer| {
            out.push(b'[');
            for (i, &coord) in line.0.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                self.push_position(out, self.rounded(coord), integer);
            }
            out.push(b']');
        };
        match geometry {
            CutGeometry::Unchanged(_) => unreachable!(),
            CutGeometry::Lines(lines) if lines.len() == 1 => {
                push_line(out, &lines[0], &mut scratch.integer);
            }
            CutGeometry::Lines(lines) => {
                out.push(b'[');
                for (i, line) in lines.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    push_line(out, line, &mut scratch.integer);
                }
                out.push(b']');
            }
            CutGeometry::Polygons {
                pieces,
                multi: false,
            } => self.push_cut_polygon(out, &pieces[0], scratch),
            CutGeometry::Polygons { pieces, .. } => {
                out.push(b'[');
                for (i, piece) in pieces.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    self.push_cut_polygon(out, piece, scratch);
                }
                out.push(b']');
            }
            CutGeometry::Collection(children) => {
                out.push(b'[');
                for (i, child) in children.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    self.push_cut_geometry(out, child, scratch);
                }
                out.push(b']');
            }
        }
        out.push(b'}');
    }

    #[inline]
    fn delims(&self) -> (u8, u8) {
        match self.encoding {
            Encoding::Json => (b'[', b']'),
            Encoding::Wkt => (b'(', b')'),
        }
    }

    fn push_polygon(&self, out: &mut Vec<u8>, polygon: &Polygon, scratch: &mut Scratch) {
        let (open, close) = self.delims();
        out.push(open);
        let rings = std::iter::once(&polygon.exterior).chain(&polygon.interiors);
        for (index, ring) in rings.enumerate() {
            if index > 0 {
                out.push(b',');
            }
            if matches!(self.encoding, Encoding::Json) {
                scratch.ring_coords.clear();
                scratch.ring_coords.reserve(ring.len());
                for &p in ring {
                    scratch.ring_coords.push(self.coordinates(p));
                }
                self.push_ring_slice(out, &scratch.ring_coords, index == 0, &mut scratch.integer);
            } else {
                self.push_line_points(out, ring, &mut scratch.integer);
            }
        }
        out.push(close);
    }

    fn push_cut_polygon(&self, out: &mut Vec<u8>, polygon: &geo::Polygon, scratch: &mut Scratch) {
        out.push(b'[');
        let rings = std::iter::once(polygon.exterior()).chain(polygon.interiors());
        for (index, ring) in rings.enumerate() {
            if index > 0 {
                out.push(b',');
            }
            scratch.ring_coords.clear();
            scratch.ring_coords.reserve(ring.0.len());
            for &coord in &ring.0 {
                scratch.ring_coords.push(self.rounded(coord));
            }
            self.push_ring_slice(out, &scratch.ring_coords, index == 0, &mut scratch.integer);
        }
        out.push(b']');
    }

    pub(super) fn push_ring_slice(
        &self,
        out: &mut Vec<u8>,
        coords: &[(i64, i64)],
        exterior: bool,
        integer: &mut itoa::Buffer,
    ) {
        let opposite = if exterior {
            Ordering::Less
        } else {
            Ordering::Greater
        };
        out.reserve(coords.len() * (2 * self.precision + 14) + 2);
        out.push(b'[');
        if Self::ring_winding(coords.iter().copied()) == opposite {
            for (i, &coord) in coords.iter().rev().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                self.push_position(out, coord, integer);
            }
        } else {
            for (i, &coord) in coords.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                self.push_position(out, coord, integer);
            }
        }
        out.push(b']');
    }

    pub(super) fn push_line_points(
        &self,
        out: &mut Vec<u8>,
        points: &[Point],
        integer: &mut itoa::Buffer,
    ) {
        let (open, close) = self.delims();
        out.reserve(points.len() * (2 * self.precision + 14) + 2);
        out.push(open);
        for (i, &point) in points.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            self.push_point(out, point, integer);
        }
        out.push(close);
    }

    #[inline]
    pub(super) fn push_point(&self, out: &mut Vec<u8>, point: Point, integer: &mut itoa::Buffer) {
        self.push_position(out, self.coordinates(point), integer);
    }

    #[inline]
    fn push_position(&self, out: &mut Vec<u8>, (lon, lat): (i64, i64), integer: &mut itoa::Buffer) {
        if self.precision == 7 {
            let mut buf = [0u8; 32];
            let mut pos = 0;
            match self.encoding {
                Encoding::Json => {
                    buf[0] = b'[';
                    pos = 1;
                    pos += fmt_decimal_7(&mut buf[pos..], lon);
                    buf[pos] = b',';
                    pos += 1;
                    pos += fmt_decimal_7(&mut buf[pos..], lat);
                    buf[pos] = b']';
                    pos += 1;
                }
                Encoding::Wkt => {
                    pos += fmt_decimal_7(&mut buf[pos..], lon);
                    buf[pos] = b' ';
                    pos += 1;
                    pos += fmt_decimal_7(&mut buf[pos..], lat);
                }
            }
            out.extend_from_slice(&buf[..pos]);
            return;
        }
        match self.encoding {
            Encoding::Json => {
                out.push(b'[');
                self.push_decimal(out, lon, integer);
                out.push(b',');
                self.push_decimal(out, lat, integer);
                out.push(b']');
            }
            Encoding::Wkt => {
                self.push_decimal(out, lon, integer);
                out.push(b' ');
                self.push_decimal(out, lat, integer);
            }
        }
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

    pub(super) fn push_bounds(
        &self,
        out: &mut Vec<u8>,
        bounds: [i64; 4],
        integer: &mut itoa::Buffer,
    ) {
        for (index, value) in bounds.into_iter().enumerate() {
            if index > 0 {
                out.push(b',');
            }
            self.push_decimal(out, value, integer);
        }
    }

    #[inline]
    fn coordinates(&self, point: Point) -> (i64, i64) {
        super::projection::coordinates_ties_even(point, self.scale as f64)
    }

    fn rounded(&self, coord: geo::Coord) -> (i64, i64) {
        (self.round(coord.x), self.round(coord.y))
    }

    fn round(&self, value: f64) -> i64 {
        (value * self.scale as f64).round_ties_even() as i64
    }

    #[inline]
    fn push_decimal(&self, out: &mut Vec<u8>, value: i64, integer: &mut itoa::Buffer) {
        if self.precision == 7 {
            let mut buf = [0u8; 16];
            let n = fmt_decimal_7(&mut buf, value);
            out.extend_from_slice(&buf[..n]);
            return;
        }
        self.push_decimal_general(out, value, integer);
    }

    fn push_decimal_general(&self, out: &mut Vec<u8>, value: i64, integer: &mut itoa::Buffer) {
        if self.precision == 0 {
            out.extend_from_slice(integer.format(value).as_bytes());
            return;
        }

        let digits = integer.format(value.unsigned_abs()).as_bytes();
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
        out.extend_from_slice(&bytes[..end]);
    }

    #[cfg(test)]
    fn geometry(&self, output: &mut Vec<u8>, geometry: &Geometry) -> io::Result<()> {
        let mut scratch = Scratch::default();
        self.push_geometry(output, geometry, &mut scratch);
        Ok(())
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
}

#[inline(always)]
fn fmt_decimal_7(buf: &mut [u8], value: i64) -> usize {
    let neg = value < 0;
    let abs = value.unsigned_abs();
    let whole = (abs / 10_000_000) as usize;
    let frac = (abs % 10_000_000) as usize;
    let mut n = 0;
    if neg {
        buf[0] = b'-';
        n = 1;
    }
    if whole >= 100 {
        buf[n] = b'0' + (whole / 100) as u8;
        let rem = whole % 100;
        buf[n + 1] = DIGITS2[rem * 2];
        buf[n + 2] = DIGITS2[rem * 2 + 1];
        n += 3;
    } else if whole >= 10 {
        buf[n] = DIGITS2[whole * 2];
        buf[n + 1] = DIGITS2[whole * 2 + 1];
        n += 2;
    } else {
        buf[n] = b'0' + whole as u8;
        n += 1;
    }
    buf[n] = b'.';
    let d0 = frac / 1_000_000;
    let r1 = frac % 1_000_000;
    let d12 = r1 / 10_000;
    let r2 = r1 % 10_000;
    let d34 = r2 / 100;
    let d56 = r2 % 100;
    buf[n + 1] = b'0' + d0 as u8;
    buf[n + 2] = DIGITS2[d12 * 2];
    buf[n + 3] = DIGITS2[d12 * 2 + 1];
    buf[n + 4] = DIGITS2[d34 * 2];
    buf[n + 5] = DIGITS2[d34 * 2 + 1];
    buf[n + 6] = DIGITS2[d56 * 2];
    buf[n + 7] = DIGITS2[d56 * 2 + 1];
    n + 8
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallel::{BATCH_SIZE, CHUNK_SIZE, MIN_PARALLEL_SIZE};
    use serde_json::{Value, json};

    #[test]
    fn fast_decimal_7_matches_general_formatter() {
        let writer = GeometryWriter::new(Encoding::Json, 7);
        let mut integer = itoa::Buffer::new();
        for value in [
            -1_800_000_000,
            -1_799_999_999,
            -1_000_000_001,
            -1_000_000_000,
            -999_999_999,
            -100_000_000,
            -99_999_999,
            -10_000_001,
            -10_000_000,
            -9_999_999,
            -100,
            -1,
            0,
            1,
            100,
            9_999_999,
            10_000_000,
            10_000_001,
            99_999_999,
            100_000_000,
            999_999_999,
            1_000_000_000,
            1_799_999_999,
            1_800_000_000,
        ] {
            let mut fast = [0u8; 16];
            let n = fmt_decimal_7(&mut fast, value);
            let mut general = Vec::new();
            writer.push_decimal_general(&mut general, value, &mut integer);
            assert_eq!(&fast[..n], general.as_slice(), "mismatch for {value}");
        }
    }

    #[test]
    fn push_json_str_matches_serde_json_across_ascii_and_unicode() {
        let mut all_ascii = String::new();
        for byte in 0u8..=127 {
            all_ascii.push(byte as char);
        }
        for sample in [
            "",
            "residential",
            "Headingley Lane",
            "quote\"slash\\ctrl\x00\x08\x0c\n\r\t\x1f",
            "Café 東京 😀",
            &all_ascii,
        ] {
            let mut fast = Vec::new();
            push_json_str(&mut fast, sample);
            let expected = serde_json::to_vec(sample).unwrap();
            assert_eq!(fast, expected, "mismatch for {sample:?}");
        }
    }

    #[test]
    fn push_tag_number_matches_tag_value_display() {
        let mut integer = itoa::Buffer::new();
        for scale in 0..=12u8 {
            for mantissa in [
                i32::MIN,
                -100_000,
                -100,
                -5,
                -1,
                0,
                1,
                5,
                10,
                99,
                100,
                123456,
                i32::MAX,
            ] {
                let mut fast = Vec::new();
                push_tag_number(&mut fast, mantissa, scale, &mut integer);
                let expected = TagValue::Number { mantissa, scale }.to_string();
                assert_eq!(
                    fast,
                    expected.as_bytes(),
                    "mismatch for mantissa={mantissa}, scale={scale}"
                );
            }
        }
    }

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
                push_feature_id(&mut output, &mut integer, id);
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
