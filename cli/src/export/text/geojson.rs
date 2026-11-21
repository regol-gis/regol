use super::{
    CutGeometry, DEFAULT_PRECISION, EncodeResult, Encoding, Feature, Format, GeometryWriter,
    Options, Source, feature_geometry, tag_key, write_properties,
};
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
    for (index, feature) in features.into_iter().enumerate() {
        encoder.feature(output, feature?, index == 0)?;
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
        |output, feature, first| encoder.feature(output, feature, first),
        |output, feature, first| encoder.feature(output, feature, first),
    )?;
    encoder.end(output)
}

struct Encoder<'a> {
    collection: bool,
    include_bounds: bool,
    property_keys: Option<Vec<&'a str>>,
    writer: GeometryWriter,
}

impl<'a> Encoder<'a> {
    fn new(format: Format, options: &'a Options, precision: usize) -> Self {
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

    fn begin(&self, output: &mut impl Write) -> io::Result<()> {
        if self.collection {
            output.write_all(COLLECTION_START)?;
        }
        Ok(())
    }

    fn end(&self, output: &mut impl Write) -> EncodeResult<()> {
        if self.collection {
            output.write_all(COLLECTION_END)?;
        }
        Ok(())
    }

    fn feature(
        &self,
        output: &mut impl Write,
        feature: Feature<'_, impl Source>,
        first: bool,
    ) -> EncodeResult<()> {
        let geometry = feature_geometry(feature)?;
        let cut =
            CutGeometry::new(&geometry).map_err(|error| format!("{}: {error}", feature.id()))?;
        let bounds = if self.include_bounds {
            Some(match cut.bounds() {
                Some(bounds) => self.writer.cut_bounds(bounds),
                None => self.writer.bounds_coordinates(feature.bounds()?),
            })
        } else {
            None
        };
        if self.collection && !first {
            output.write_all(b",")?;
        }
        write!(
            output,
            "{{\"type\":\"Feature\",\"id\":\"{}\",\"geometry\":",
            feature.id()
        )?;
        self.writer.cut_geometry(output, &cut)?;
        if let Some(bounds) = bounds {
            output.write_all(b",\"bbox\":[")?;
            self.writer.write_bounds(output, bounds)?;
            output.write_all(b"]")?;
        }
        output.write_all(b",\"properties\":{")?;
        write_properties(feature, output, self.property_keys.as_deref())?;
        output.write_all(if self.collection { b"}}" } else { b"}}\n" })?;
        Ok(())
    }
}
