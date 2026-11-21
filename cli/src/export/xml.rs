use super::{Export, ExportResult, kind_name, osm_coordinates};
use regol::{Feature, Point, TagValue, source::Source};
use std::io::Write;

pub(super) fn write<S: Source>(
    export: &Export<'_, S>,
    writer: &mut impl Write,
) -> ExportResult<()> {
    writer.write_all(
        b"<?xml version='1.0' encoding='UTF-8'?>\n<osm version=\"0.6\" generator=\"regol\">\n",
    )?;
    let mut elements = Vec::with_capacity(16384);
    export.visit_nodes(|id, point, feature| {
        elements.extend_from_slice(b"  <node id=\"");
        number(&mut elements, id);
        elements.push(b'"');
        location(&mut elements, point);
        elements.extend_from_slice(b" version=\"1\"");
        if let Some(feature) = feature {
            let mut tags = feature.tags()?;
            if let Some(tag) = tags.next() {
                elements.extend_from_slice(b">\n");
                let tag = tag?;
                write_tag(&mut elements, &tag.key, &tag.value)?;
                for tag in tags {
                    let tag = tag?;
                    write_tag(&mut elements, &tag.key, &tag.value)?;
                }
                elements.extend_from_slice(b"  </node>\n");
            } else {
                elements.extend_from_slice(b"/>\n");
            }
        } else {
            elements.extend_from_slice(b"/>\n");
        }
        flush_batch(writer, &mut elements)?;
        Ok(())
    })?;

    for (index, &way) in export.ways.iter().enumerate() {
        elements.extend_from_slice(b"  <way id=\"");
        number(&mut elements, way.id().id);
        elements.extend_from_slice(b"\" version=\"1\">\n");
        if let Some(refs) = export.way_refs(index) {
            for &id in refs {
                write_ref(&mut elements, id);
            }
        } else {
            for node in way.way_nodes()? {
                let node = node?;
                if let Some(id) = node.id {
                    write_ref(&mut elements, id);
                } else {
                    elements.extend_from_slice(b"    <nd");
                    location(&mut elements, node.point);
                    elements.extend_from_slice(b"/>\n");
                }
            }
        }
        write_tags(&mut elements, way)?;
        elements.extend_from_slice(b"  </way>\n");
        flush_batch(writer, &mut elements)?;
    }

    for &relation in &export.relations {
        elements.extend_from_slice(b"  <relation id=\"");
        number(&mut elements, relation.id().id);
        elements.extend_from_slice(b"\" version=\"1\">\n");
        for member in relation.members()? {
            let member = member?;
            let id = member.feature.id();
            elements.extend_from_slice(b"    <member type=\"");
            elements.extend_from_slice(kind_name(id.kind).as_bytes());
            elements.extend_from_slice(b"\" ref=\"");
            number(&mut elements, id.id);
            elements.extend_from_slice(b"\" role=\"");
            escape(&mut elements, &member.role);
            elements.extend_from_slice(b"\"/>\n");
        }
        write_tags(&mut elements, relation)?;
        elements.extend_from_slice(b"  </relation>\n");
        flush_batch(writer, &mut elements)?;
    }

    elements.extend_from_slice(b"</osm>\n");
    writer.write_all(&elements)?;
    Ok(())
}

fn flush_batch(writer: &mut impl Write, elements: &mut Vec<u8>) -> std::io::Result<()> {
    if elements.len() >= 16384 {
        writer.write_all(elements)?;
        elements.clear();
    }
    Ok(())
}

fn number(buffer: &mut Vec<u8>, value: u64) {
    buffer.extend_from_slice(itoa::Buffer::new().format(value).as_bytes());
}

fn coordinate(buffer: &mut Vec<u8>, value: i64) {
    if value < 0 {
        buffer.push(b'-');
    }
    let value = value.unsigned_abs();
    let mut digits = itoa::Buffer::new();
    let (whole, fraction) = if value < 10_000_000 {
        // The leading 1 keeps any zeroes before the first fractional digit.
        let padded = digits.format(value + 10_000_000);
        ("0", &padded[1..])
    } else {
        let digits = digits.format(value);
        digits.split_at(digits.len() - 7)
    };
    buffer.extend_from_slice(whole.as_bytes());
    let fraction = fraction.trim_end_matches('0');
    if !fraction.is_empty() {
        buffer.push(b'.');
        buffer.extend_from_slice(fraction.as_bytes());
    }
}

fn location(buffer: &mut Vec<u8>, point: Point) {
    let (lat, lon) = osm_coordinates(point);
    buffer.extend_from_slice(b" lat=\"");
    coordinate(buffer, lat);
    buffer.extend_from_slice(b"\" lon=\"");
    coordinate(buffer, lon);
    buffer.push(b'"');
}

fn write_ref(buffer: &mut Vec<u8>, id: u64) {
    buffer.extend_from_slice(b"    <nd ref=\"");
    number(buffer, id);
    buffer.extend_from_slice(b"\"/>\n");
}

fn write_tags<S: Source>(buffer: &mut Vec<u8>, feature: Feature<'_, S>) -> ExportResult<()> {
    for tag in feature.tags()? {
        let tag = tag?;
        write_tag(buffer, &tag.key, &tag.value)?;
    }
    Ok(())
}

fn write_tag(buffer: &mut Vec<u8>, key: &str, value: &TagValue<'_>) -> std::io::Result<()> {
    buffer.extend_from_slice(b"    <tag k=\"");
    escape(buffer, key);
    buffer.extend_from_slice(b"\" v=\"");
    match value {
        TagValue::Text(text) => escape(buffer, text),
        number => write!(buffer, "{number}")?,
    }
    buffer.extend_from_slice(b"\"/>\n");
    Ok(())
}

const ATTRIBUTE_ENTITIES: [&[u8]; 256] = {
    let mut entities: [&[u8]; 256] = [&[]; 256];
    let mut byte = 0;
    while byte < 32 {
        entities[byte] = b"\xEF\xBF\xBD";
        byte += 1;
    }
    entities[b'&' as usize] = b"&amp;";
    entities[b'<' as usize] = b"&lt;";
    entities[b'>' as usize] = b"&gt;";
    entities[b'"' as usize] = b"&quot;";
    entities[b'\t' as usize] = b"&#9;";
    entities[b'\n' as usize] = b"&#10;";
    entities[b'\r' as usize] = b"&#13;";
    entities
};

fn escape(buffer: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    let mut start = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        let entity = ATTRIBUTE_ENTITIES[usize::from(byte)];
        if !entity.is_empty() {
            buffer.extend_from_slice(&bytes[start..index]);
            buffer.extend_from_slice(entity);
            start = index + 1;
        }
    }
    buffer.extend_from_slice(&bytes[start..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinate_keeps_leading_fractional_zeroes_and_trims_trailing_zeroes() {
        for (value, expected) in [
            (0, "0"),
            (1, "0.0000001"),
            (-1, "-0.0000001"),
            (10, "0.000001"),
            (-1_230_000, "-0.123"),
            (9_999_999, "0.9999999"),
            (10_000_000, "1"),
            (10_000_001, "1.0000001"),
            (-10_030_000, "-1.003"),
            (1_799_999_999, "179.9999999"),
            (-1_800_000_000, "-180"),
        ] {
            let mut output = Vec::new();
            coordinate(&mut output, value);
            assert_eq!(output, expected.as_bytes(), "{value}");
        }
    }

    #[test]
    fn escape_preserves_non_ascii_and_xml_attribute_whitespace() {
        let mut output = Vec::new();
        escape(&mut output, "é&<>\"'\t\n\r\0\u{1f}中");
        assert_eq!(output, "é&amp;&lt;&gt;&quot;'&#9;&#10;&#13;��中".as_bytes());
    }
}
