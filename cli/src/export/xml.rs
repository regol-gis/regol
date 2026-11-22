use super::{Export, ExportResult, kind_name, osm_coordinates};
use regol::{Feature, Point, TagValue, source::Source};
use std::{
    io::Write,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};

static DIGITS2: &[u8; 200] = b"0001020304050607080910111213141516171819\
2021222324252627282930313233343536373839\
4041424344454647484950515253545556575859\
6061626364656667686970717273747576777879\
8081828384858687888990919293949596979899";

pub(super) fn write<S: Source + Sync>(
    export: &Export<'_, S>,
    writer: &mut impl Write,
    threads: Option<usize>,
) -> ExportResult<()> {
    writer.write_all(
        b"<?xml version='1.0' encoding='UTF-8'?>\n<osm version=\"0.6\" generator=\"regol\">\n",
    )?;
    let workers = threads.unwrap_or_else(|| {
        thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
    });
    let total = export.nodes.len()
        + export.vertices.len()
        + export.ways.len()
        + export.relations.len();

    if workers <= 1 || total < 1024 {
        write_serial(export, writer)?;
    } else {
        write_parallel(export, writer, workers)?;
    }

    writer.write_all(b"</osm>\n")?;
    Ok(())
}

fn write_serial<S: Source>(export: &Export<'_, S>, writer: &mut impl Write) -> ExportResult<()> {
    let mut elements = Vec::with_capacity(64 * 1024);
    let mut integer = itoa::Buffer::new();
    export.visit_nodes(|id, point, feature| {
        format_node(&mut elements, id, point, feature, &mut integer)?;
        flush_batch(writer, &mut elements)?;
        Ok(())
    })?;

    for (index, &way) in export.ways.iter().enumerate() {
        format_way(&mut elements, export, index, way, &mut integer)?;
        flush_batch(writer, &mut elements)?;
    }

    for &relation in &export.relations {
        format_relation(&mut elements, relation, &mut integer)?;
        flush_batch(writer, &mut elements)?;
    }

    if !elements.is_empty() {
        writer.write_all(&elements)?;
    }
    Ok(())
}

enum XmlChunk {
    Nodes {
        n_start: usize,
        n_end: usize,
        v_start: usize,
        v_end: usize,
    },
    Ways {
        start: usize,
        end: usize,
    },
    Relations {
        start: usize,
        end: usize,
    },
}

fn write_parallel<S: Source + Sync>(
    export: &Export<'_, S>,
    writer: &mut impl Write,
    workers: usize,
) -> ExportResult<()> {
    const CHUNK: usize = 2048;
    let mut tasks = Vec::new();

    // Partition nodes + vertices in exact sorted ID order.
    let mut n_pos = 0;
    let mut v_pos = 0;
    while n_pos < export.nodes.len() || v_pos < export.vertices.len() {
        let n_rem = export.nodes.len() - n_pos;
        let v_rem = export.vertices.len() - v_pos;
        if n_rem == 0 {
            let v_end = (v_pos + CHUNK).min(export.vertices.len());
            tasks.push(XmlChunk::Nodes {
                n_start: n_pos,
                n_end: n_pos,
                v_start: v_pos,
                v_end,
            });
            v_pos = v_end;
        } else if v_rem == 0 {
            let n_end = (n_pos + CHUNK).min(export.nodes.len());
            tasks.push(XmlChunk::Nodes {
                n_start: n_pos,
                n_end,
                v_start: v_pos,
                v_end: v_pos,
            });
            n_pos = n_end;
        } else if n_rem >= v_rem {
            let n_end = (n_pos + CHUNK).min(export.nodes.len());
            let v_end = if n_end == export.nodes.len() {
                export.vertices.len()
            } else {
                let bound_id = export.nodes[n_end - 1].id().id;
                v_pos + export.vertices[v_pos..].partition_point(|&(id, _)| id <= bound_id)
            };
            tasks.push(XmlChunk::Nodes {
                n_start: n_pos,
                n_end,
                v_start: v_pos,
                v_end,
            });
            n_pos = n_end;
            v_pos = v_end;
        } else {
            let v_end = (v_pos + CHUNK).min(export.vertices.len());
            let n_end = if v_end == export.vertices.len() {
                export.nodes.len()
            } else {
                let bound_id = export.vertices[v_end - 1].0;
                n_pos + export.nodes[n_pos..].partition_point(|f| f.id().id <= bound_id)
            };
            tasks.push(XmlChunk::Nodes {
                n_start: n_pos,
                n_end,
                v_start: v_pos,
                v_end,
            });
            n_pos = n_end;
            v_pos = v_end;
        }
    }

    let mut w_pos = 0;
    while w_pos < export.ways.len() {
        let end = (w_pos + CHUNK / 2).min(export.ways.len());
        tasks.push(XmlChunk::Ways { start: w_pos, end });
        w_pos = end;
    }

    let mut r_pos = 0;
    while r_pos < export.relations.len() {
        let end = (r_pos + CHUNK / 4).min(export.relations.len());
        tasks.push(XmlChunk::Relations { start: r_pos, end });
        r_pos = end;
    }

    if tasks.len() <= 1 {
        return write_serial(export, writer);
    }

    // Process in bounded waves to cap memory while scaling across cores.
    let wave_size = (workers * 2).clamp(4, 16);
    let mut wave_bufs: Vec<std::sync::Mutex<Result<Vec<u8>, regol::Error>>> = (0..wave_size)
        .map(|_| std::sync::Mutex::new(Ok(Vec::with_capacity(128 * 1024))))
        .collect();

    let num_threads = workers.min(wave_size);
    for wave in tasks.chunks(wave_size) {
        let next = AtomicUsize::new(0);
        let bufs_ref = &wave_bufs;
        thread::scope(|scope| {
            let run = || {
                let mut integer = itoa::Buffer::new();
                loop {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= wave.len() {
                        break;
                    }
                    let mut guard = bufs_ref[idx].lock().unwrap();
                    let mut buf = match std::mem::replace(&mut *guard, Ok(Vec::new())) {
                        Ok(mut b) => {
                            b.clear();
                            b
                        }
                        Err(_) => Vec::with_capacity(128 * 1024),
                    };
                    let res = format_chunk(export, &wave[idx], &mut buf, &mut integer);
                    *guard = res.map(|()| buf);
                }
            };
            for _ in 1..num_threads.min(wave.len()) {
                scope.spawn(run);
            }
            run();
        });
        for slot in &mut wave_bufs[..wave.len()] {
            let guard = slot.get_mut().unwrap();
            match guard {
                Ok(buf) => writer.write_all(buf)?,
                Err(_) => {
                    let err = std::mem::replace(guard, Ok(Vec::new())).unwrap_err();
                    return Err(err.into());
                }
            }
        }
    }
    Ok(())
}

fn format_chunk<S: Source>(
    export: &Export<'_, S>,
    chunk: &XmlChunk,
    elements: &mut Vec<u8>,
    integer: &mut itoa::Buffer,
) -> regol::Result<()> {
    match *chunk {
        XmlChunk::Nodes {
            n_start,
            n_end,
            v_start,
            v_end,
        } => {
            let mut vertices = export.vertices[v_start..v_end].iter().copied().peekable();
            for &feature in &export.nodes[n_start..n_end] {
                let id = feature.id().id;
                while let Some((vid, point)) = vertices.next_if(|v| v.0 < id) {
                    format_node::<S>(elements, vid, point, None, integer)?;
                }
                vertices.next_if(|v| v.0 == id);
                let point = feature.point()?.expect("node point");
                format_node(elements, id, point, Some(feature), integer)?;
            }
            for (vid, point) in vertices {
                format_node::<S>(elements, vid, point, None, integer)?;
            }
        }
        XmlChunk::Ways { start, end } => {
            for index in start..end {
                format_way(elements, export, index, export.ways[index], integer)?;
            }
        }
        XmlChunk::Relations { start, end } => {
            for &relation in &export.relations[start..end] {
                format_relation(elements, relation, integer)?;
            }
        }
    }
    Ok(())
}

#[inline]
fn format_node<S: Source>(
    elements: &mut Vec<u8>,
    id: u64,
    point: Point,
    feature: Option<Feature<'_, S>>,
    integer: &mut itoa::Buffer,
) -> regol::Result<()> {
    elements.extend_from_slice(b"  <node id=\"");
    elements.extend_from_slice(integer.format(id).as_bytes());
    elements.push(b'"');
    location(elements, point);
    elements.extend_from_slice(b" version=\"1\"");
    if let Some(feature) = feature {
        let mut tags = feature.tags()?;
        if let Some(tag) = tags.next() {
            elements.extend_from_slice(b">\n");
            let tag = tag?;
            write_tag(elements, &tag.key, &tag.value, integer);
            for tag in tags {
                let tag = tag?;
                write_tag(elements, &tag.key, &tag.value, integer);
            }
            elements.extend_from_slice(b"  </node>\n");
        } else {
            elements.extend_from_slice(b"/>\n");
        }
    } else {
        elements.extend_from_slice(b"/>\n");
    }
    Ok(())
}

#[inline]
fn format_way<S: Source>(
    elements: &mut Vec<u8>,
    export: &Export<'_, S>,
    index: usize,
    way: Feature<'_, S>,
    integer: &mut itoa::Buffer,
) -> regol::Result<()> {
    elements.extend_from_slice(b"  <way id=\"");
    elements.extend_from_slice(integer.format(way.id().id).as_bytes());
    elements.extend_from_slice(b"\" version=\"1\">\n");
    if let Some(refs) = export.way_refs(index) {
        elements.reserve(refs.len() * 28);
        for &id in refs {
            write_ref(elements, id, integer);
        }
    } else {
        for node in way.way_nodes()? {
            let node = node?;
            if let Some(id) = node.id {
                write_ref(elements, id, integer);
            } else {
                elements.extend_from_slice(b"    <nd");
                location(elements, node.point);
                elements.extend_from_slice(b"/>\n");
            }
        }
    }
    write_tags(elements, way, integer)?;
    elements.extend_from_slice(b"  </way>\n");
    Ok(())
}

#[inline]
fn format_relation<S: Source>(
    elements: &mut Vec<u8>,
    relation: Feature<'_, S>,
    integer: &mut itoa::Buffer,
) -> regol::Result<()> {
    elements.extend_from_slice(b"  <relation id=\"");
    elements.extend_from_slice(integer.format(relation.id().id).as_bytes());
    elements.extend_from_slice(b"\" version=\"1\">\n");
    for member in relation.members()? {
        let member = member?;
        let id = member.feature.id();
        elements.extend_from_slice(b"    <member type=\"");
        elements.extend_from_slice(kind_name(id.kind).as_bytes());
        elements.extend_from_slice(b"\" ref=\"");
        elements.extend_from_slice(integer.format(id.id).as_bytes());
        elements.extend_from_slice(b"\" role=\"");
        escape(elements, &member.role);
        elements.extend_from_slice(b"\"/>\n");
    }
    write_tags(elements, relation, integer)?;
    elements.extend_from_slice(b"  </relation>\n");
    Ok(())
}

fn flush_batch(writer: &mut impl Write, elements: &mut Vec<u8>) -> std::io::Result<()> {
    if elements.len() >= 64 * 1024 {
        writer.write_all(elements)?;
        elements.clear();
    }
    Ok(())
}

#[inline]
fn coordinate(buffer: &mut Vec<u8>, value: i64) {
    let mut buf = [0u8; 16];
    let mut n = 0;
    if value < 0 {
        buf[0] = b'-';
        n = 1;
    }
    let abs = value.unsigned_abs();
    let whole = (abs / 10_000_000) as usize;
    let frac = (abs % 10_000_000) as usize;
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
    if frac != 0 {
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
        let mut end = n + 8;
        while buf[end - 1] == b'0' {
            end -= 1;
        }
        n = end;
    }
    buffer.extend_from_slice(&buf[..n]);
}

#[inline]
fn location(buffer: &mut Vec<u8>, point: Point) {
    let (lat, lon) = osm_coordinates(point);
    buffer.extend_from_slice(b" lat=\"");
    coordinate(buffer, lat);
    buffer.extend_from_slice(b"\" lon=\"");
    coordinate(buffer, lon);
    buffer.push(b'"');
}

#[inline]
fn write_ref(buffer: &mut Vec<u8>, id: u64, integer: &mut itoa::Buffer) {
    buffer.extend_from_slice(b"    <nd ref=\"");
    buffer.extend_from_slice(integer.format(id).as_bytes());
    buffer.extend_from_slice(b"\"/>\n");
}

#[inline]
fn write_tags<S: Source>(
    buffer: &mut Vec<u8>,
    feature: Feature<'_, S>,
    integer: &mut itoa::Buffer,
) -> regol::Result<()> {
    for tag in feature.tags()? {
        let tag = tag?;
        write_tag(buffer, &tag.key, &tag.value, integer);
    }
    Ok(())
}

#[inline]
fn write_tag(buffer: &mut Vec<u8>, key: &str, value: &TagValue<'_>, integer: &mut itoa::Buffer) {
    buffer.extend_from_slice(b"    <tag k=\"");
    escape(buffer, key);
    buffer.extend_from_slice(b"\" v=\"");
    match value {
        TagValue::Text(text) => escape(buffer, text),
        TagValue::Number { mantissa, scale } => {
            super::text::push_tag_number(buffer, *mantissa, *scale, integer);
        }
    }
    buffer.extend_from_slice(b"\"/>\n");
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

#[inline]
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
    use crate::export::Completion;

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
        assert_eq!(
            output,
            "é&amp;&lt;&gt;&quot;'&#9;&#10;&#13;\u{fffd}\u{fffd}中".as_bytes()
        );
    }

    #[test]
    fn parallel_xml_matches_serial_byte_for_byte() {
        let library =
            regol::Library::new(include_bytes!("../../tests/fixtures/ids.gol").as_slice()).unwrap();
        let mut export = Export::collect(
            library.features(),
            library.has_way_node_ids(),
            &Completion::All,
            None,
        )
        .unwrap();
        // Expand nodes & vertices beyond CHUNK so multiple parallel node waves run.
        let mut extra_vertices = Vec::new();
        for id in 100..2600u64 {
            extra_vertices.push((id, Point { x: (id as i32) * 1000, y: -(id as i32) * 1000 }));
        }
        export.vertices.extend(extra_vertices);
        export.vertices.sort_unstable_by_key(|v| v.0);
        export.vertices.dedup_by_key(|v| v.0);

        let mut serial = Vec::new();
        write_serial(&export, &mut serial).unwrap();
        let mut parallel = Vec::new();
        write_parallel(&export, &mut parallel, 4).unwrap();
        assert_eq!(serial, parallel);
    }
}
