use crate::{
    Bounds, Error, Library, Point, Result,
    decode::{Decode, ReadWindow, add},
    source::Source,
};
use std::{borrow::Cow, fmt};

/// The underlying OSM object type. Area classification is available via [`Feature::is_area`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FeatureType {
    /// An OSM node.
    Node,
    /// An OSM way, including ways classified as areas.
    Way,
    /// An OSM relation, including relations classified as areas.
    Relation,
}
/// An OSM identifier qualified by object type. Display uses forms such as `N1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FeatureId {
    /// The OSM namespace in which this ID is unique.
    pub kind: FeatureType,
    /// Numeric OSM identifier within that namespace.
    pub id: u64,
}
impl fmt::Display for FeatureId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{id}",
            match self.kind {
                FeatureType::Node => 'N',
                FeatureType::Way => 'W',
                FeatureType::Relation => 'R',
            },
            id = self.id
        )
    }
}

/// Checked, compact tile bounds. Construction is done once per spatial tile.
#[derive(Clone, Copy, Default)]
pub(crate) struct TileExtent {
    page: u32,
    length: u32,
}
impl TileExtent {
    #[inline]
    pub(crate) fn new(start: u64, end: u64) -> Result<Self> {
        let page = u32::try_from(start >> 12)
            .ok()
            .filter(|_| start & 4095 == 0)
            .ok_or_else(|| Error::invalid(start, "invalid tile page"))?;
        let length = end
            .checked_sub(start)
            .and_then(|length| u32::try_from(length).ok())
            .ok_or_else(|| Error::invalid(start, "invalid tile extent"))?;
        Ok(Self { page, length })
    }
    #[inline]
    pub(crate) fn start(self) -> u64 {
        u64::from(self.page) << 12
    }
    #[inline]
    pub(crate) fn end(self) -> u64 {
        self.start() + u64::from(self.length)
    }
}

/// A small borrowed handle. Does not allocate or retain decoded geometry/tags.
pub struct Feature<'a, S> {
    pub(crate) library: &'a Library<S>,
    header: u64,
    pub(crate) tile: TileExtent,
    anchor_offset: u32,
    tag_pointer: i32,
}
impl<S> Copy for Feature<'_, S> {}
impl<S> Clone for Feature<'_, S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S> fmt::Debug for Feature<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Feature").field("id", &self.id()).finish()
    }
}
impl<S> Feature<'_, S> {
    #[inline]
    pub(crate) fn tile_start(self) -> u64 {
        self.tile.start()
    }
    #[inline]
    pub(crate) fn tile_end(self) -> u64 {
        self.tile.end()
    }
    #[inline]
    pub(crate) fn anchor(self) -> u64 {
        self.tile_start() + u64::from(self.anchor_offset)
    }
    /// Return the OSM type and identifier stored in the feature header.
    pub fn id(self) -> FeatureId {
        FeatureId {
            kind: self.kind(),
            id: self.header >> 12,
        }
    }
    /// Return the underlying OSM object type.
    pub fn kind(self) -> FeatureType {
        // Construction validates these two bits before a handle can escape.
        match (self.header >> 3) & 3 {
            0 => FeatureType::Node,
            1 => FeatureType::Way,
            2 => FeatureType::Relation,
            _ => unreachable!("validated feature header"),
        }
    }
    /// Whether the producer classified this way or relation as an area.
    pub fn is_area(self) -> bool {
        self.header & 2 != 0
    }
    pub(crate) fn flags(self) -> u16 {
        self.header as u16 & 0xfff
    }
}
impl<'a, S: Source> Feature<'a, S> {
    #[inline]
    pub(crate) fn at(
        library: &'a Library<S>,
        anchor: u64,
        tile_start: u64,
        tile_end: u64,
    ) -> Result<Self> {
        Self::at_source(
            library,
            &library.source,
            anchor,
            TileExtent::new(tile_start, tile_end)?,
        )
    }
    pub(crate) fn at_source(
        library: &'a Library<S>,
        source: &impl Source,
        anchor: u64,
        tile: TileExtent,
    ) -> Result<Self> {
        let record = source.array::<12>(anchor)?;
        Self::at_record(library, &record, anchor, tile)
    }
    #[inline]
    pub(crate) fn at_window<const EXACT: bool>(
        library: &'a Library<S>,
        window: &ReadWindow<'_, S>,
        anchor: u64,
        tile: TileExtent,
    ) -> Result<Self> {
        if let Some(record) = window.record::<12, EXACT>(anchor, tile.end()) {
            Self::at_record(library, record, anchor, tile)
        } else {
            Self::at_source(library, window, anchor, tile)
        }
    }
    #[inline]
    pub(crate) fn at_record(
        library: &'a Library<S>,
        record: &[u8; 12],
        anchor: u64,
        tile: TileExtent,
    ) -> Result<Self> {
        let kind = match record[0] & 24 {
            0 => FeatureType::Node,
            8 => FeatureType::Way,
            16 => FeatureType::Relation,
            _ => return Err(Error::invalid(anchor, "invalid feature type")),
        };
        let before = if kind == FeatureType::Node { 8 } else { 16 };
        let after = if kind == FeatureType::Node { 12 } else { 16 };
        // A tile starts below 2^44 and its length fits u32. These two
        // bounds also prove the relative anchor fits the compact handle.
        let start = tile.start();
        if anchor < start + before || anchor > tile.end().saturating_sub(after) {
            return Err(Error::invalid(anchor, "feature outside tile"));
        }
        Ok(Self::from_leaf_header(
            library,
            record,
            tile,
            (anchor - start) as u32,
        ))
    }
    /// Construct a handle after checking the full record against the tile
    /// and matching the header kind to its node or non-node record layout.
    #[inline]
    pub(crate) fn from_leaf_header(
        library: &'a Library<S>,
        record: &[u8; 12],
        tile: TileExtent,
        anchor_offset: u32,
    ) -> Self {
        Self {
            library,
            tile,
            anchor_offset,
            header: u64::from_le_bytes(record[..8].try_into().expect("eight header bytes")),
            tag_pointer: i32::from_le_bytes(
                record[8..].try_into().expect("four tag-pointer bytes"),
            ),
        }
    }
    /// Read the stored integer Mercator bounding box. A node has a single-point box.
    pub fn bounds(self) -> Result<Bounds> {
        self.bounds_source(&self.library.source)
    }
    pub(crate) fn bounds_source(self, source: &impl Source) -> Result<Bounds> {
        if self.kind() == FeatureType::Node {
            let bytes = source.array::<8>(self.anchor() - 8)?;
            let p = Point {
                x: i32::from_le_bytes(bytes[..4].try_into().expect("four x bytes")),
                y: i32::from_le_bytes(bytes[4..].try_into().expect("four y bytes")),
            };
            Ok(Bounds { min: p, max: p })
        } else {
            Bounds::read(source, self.anchor() - 16)
        }
    }
    // Keep checked bounds decoding in the candidate loop: returning a separate
    // Result<Bounds> here adds measurable per-candidate copying/call overhead.
    #[inline(always)]
    pub(crate) fn bounds_window<const EXACT: bool>(
        self,
        window: &ReadWindow<'_, S>,
    ) -> Result<Bounds> {
        if self.kind() == FeatureType::Node {
            if let Some(b) = window.record::<8, EXACT>(self.anchor() - 8, self.tile_end()) {
                let p = Point {
                    x: i32::from_le_bytes(b[..4].try_into().expect("four x bytes")),
                    y: i32::from_le_bytes(b[4..].try_into().expect("four y bytes")),
                };
                return Ok(Bounds { min: p, max: p });
            }
        } else if let Some(b) = window.record::<16, EXACT>(self.anchor() - 16, self.tile_end()) {
            return Bounds::from_record(b);
        }
        self.bounds_source(window)
    }
    /// Read a node's position. Ways and relations return `Ok(None)`.
    pub fn point(self) -> Result<Option<Point>> {
        if self.kind() != FeatureType::Node {
            return Ok(None);
        }
        let record = self.library.source.array::<8>(self.anchor() - 8)?;
        Ok(Some(Point {
            x: i32::from_le_bytes(record[..4].try_into().expect("four x bytes")),
            y: i32::from_le_bytes(record[4..].try_into().expect("four y bytes")),
        }))
    }
    pub(crate) fn body(self) -> Result<u64> {
        if self.kind() == FeatureType::Node {
            return Err(Error::WrongFeatureType);
        }
        let p = self.library.source.relative(self.anchor() + 12, !0)?;
        self.check(p, 1)?;
        Ok(p)
    }
    pub(crate) fn check(self, p: u64, n: u64) -> Result<()> {
        if p < self.tile_start() || p.checked_add(n).is_none_or(|e| e > self.tile_end()) {
            return Err(Error::invalid(p, "feature data outside tile"));
        }
        Ok(())
    }
    #[inline]
    pub(crate) fn checked_tag_table(self) -> Result<u64> {
        let p = add(self.anchor() + 8, i64::from(self.tag_pointer & !1))?;
        self.check(p, 4)?;
        Ok(p)
    }
    /// Iterate over the stored tags, including locally encoded keys and values.
    #[inline]
    pub fn tags(self) -> Result<Tags<'a, S>> {
        Tags::new(self)
    }
    /// Return the first stored tag with this key, in the order of [`Self::tags`].
    pub fn tag(self, key: &str) -> Result<Option<TagValue<'a>>> {
        if let Some(result) = self.tag_fast(key) {
            return Ok(result);
        }
        for tag in self.tags()? {
            let tag = tag?;
            if tag.key == key {
                return Ok(Some(tag.value));
            }
        }
        Ok(None)
    }

    #[inline]
    fn tile_bytes(self) -> Option<&'a [u8]> {
        let start = self.tile_start();
        let len = usize::try_from(self.tile_end().checked_sub(start)?).ok()?;
        let source = &self.library.source;
        if let Some(bytes) = source.as_bytes() {
            let off = usize::try_from(start).ok()?;
            bytes.get(off..off.checked_add(len)?)
        } else {
            source.retained_tail(start)?.get(..len)
        }
    }

    #[inline]
    fn tag_fast(self, key: &str) -> Option<Option<TagValue<'a>>> {
        let tile = self.tile_bytes()?;
        let dict = self.library.strings.get()?;
        let tile_start = self.tile_start();
        let table = self.checked_tag_table().ok()?;
        let table_off = usize::try_from(table.checked_sub(tile_start)?).ok()?;
        let first = u32::from_le_bytes(*tile.get(table_off..)?.first_chunk::<4>()?);

        let mut steps = 0u64;
        let max_tags = self.library.limits.max_tags;

        if first != 0x8001 {
            if let Some(target_code) = dict.key_code(key) {
                let mut off = table_off;
                loop {
                    steps += 1;
                    if steps > max_tags {
                        return None;
                    }
                    let tail = tile.get(off..)?;
                    let word = u32::from_le_bytes(*tail.first_chunk::<4>()?);
                    let found = ((word >> 2) & 8191) as u16;
                    if found == target_code {
                        let val = decode_inline_value(
                            tile,
                            dict,
                            off + 2,
                            word & 3,
                            (word >> 16) as u16,
                        )?;
                        return Some(Some(val));
                    }
                    if found > target_code || word & 0x8000 != 0 {
                        break;
                    }
                    off += 4 + usize::from((word as u16) & 2);
                }
            }
        }

        if self.tag_pointer & 1 == 0 {
            return Some(None);
        }

        let aligned_table = (table & !3).checked_sub(tile_start)? as i64;
        let mut off = table_off.checked_sub(6)?;
        let key_bytes = key.as_bytes();
        loop {
            steps += 1;
            if steps > max_tags {
                return None;
            }
            let entry = tile.get(off..off.checked_add(6)?)?;
            let word = i32::from_le_bytes(entry[2..6].try_into().ok()?);
            let flags = word & 7;
            let key_off =
                usize::try_from(aligned_table.checked_add(i64::from((word & !7) >> 1))?).ok()?;
            let (&klen, ktail) = tile.get(key_off..)?.split_first()?;
            if klen >= 128 {
                return None;
            }
            let kslice = ktail.get(..usize::from(klen))?;
            if kslice == key_bytes {
                std::str::from_utf8(kslice).ok()?;
                let val_off = if flags & 2 != 0 {
                    off.checked_sub(2)?
                } else {
                    off
                };
                let narrow = u16::from_le_bytes(tile.get(val_off..val_off + 2)?.try_into().ok()?);
                let val = decode_inline_value(tile, dict, val_off, (flags & 3) as u32, narrow)?;
                return Some(Some(val));
            }
            if flags & 4 != 0 {
                return Some(None);
            }
            let stride = 6 + (flags & 2) as usize;
            off = off.checked_sub(stride)?;
        }
    }
    /// Locate a prepared key without decoding values of unrelated tags.
    /// `code` comes from this library's dictionary; absent codes imply a local key.
    // Keep lookup together with the value test in compound traversal; otherwise
    // the extra helper calls outweigh the benefits of fusing the predicate loop.
    #[inline(always)]
    pub(crate) fn encoded_tag(
        self,
        key: &str,
        code: Option<u16>,
        window: &mut ReadWindow<'_, S>,
        strings: &mut ReadWindow<'_, S>,
    ) -> Result<Option<EncodedValue>> {
        let pointer = self.tag_pointer;
        let table = self.checked_tag_table()?;
        // Selected once for a dictionary-absent key. It can only occur in the
        // local table; keep the pointer check even when that table is absent.
        if code.is_none() && pointer & 1 == 0 {
            return Ok(None);
        }
        window.seek(table)?;
        let s = &*window;
        let mut steps = 0;
        if let Some(code) = code {
            let mut p = table;
            let mut cursor = window.tail(table, self.tile_end());
            loop {
                steps += 1;
                if steps > self.library.limits.max_tags {
                    return Err(Error::Limit("tag lookup"));
                }
                let entry = if let Some((entry, _)) =
                    cursor.and_then(|bytes| bytes.split_first_chunk::<4>())
                {
                    // The cursor is bounded by both the segment and the tile.
                    // split_first_chunk checks the entry's entire byte range.
                    u32::from_le_bytes(*entry)
                } else {
                    self.check(p, 4)?;
                    s.u32(p)?
                };
                if entry == 0x8001 {
                    return Ok(None);
                }
                let word = entry as u16;
                let found = (word >> 2) & 8191;
                if found == code {
                    // The narrow payload is already in this checked entry.
                    if word & 3 == 0 {
                        return Ok(Some(EncodedValue::Number {
                            mantissa: (entry >> 16) as i32 - 256,
                            scale: 0,
                        }));
                    }
                    if word & 3 == 1 {
                        let value = (entry >> 16) as u16;
                        if usize::from(value) >= self.library.string_count {
                            return Err(Error::invalid(p + 2, "undefined global string code"));
                        }
                        return Ok(Some(EncodedValue::Global(value)));
                    }
                    return self.encoded_value(s, i32::from(word & 3), p + 2).map(Some);
                }
                if found > code || word & 0x8000 != 0 {
                    return Ok(None);
                }
                let width = 4 + usize::from(word & 2);
                cursor = cursor.and_then(|bytes| bytes.get(width..));
                p += width as u64;
            }
        }
        // GOQL resolves a key to one storage representation for this library.
        let mut p = add(table, -6)?;
        loop {
            steps += 1;
            if steps > self.library.limits.max_tags {
                return Err(Error::Limit("tag lookup"));
            }
            self.check(p, 6)?;
            let word = s.i32(p + 2)?;
            let flags = word & 7;
            let value_ptr = if flags & 2 != 0 { add(p, -2)? } else { p };
            self.check(value_ptr, if flags & 2 != 0 { 8 } else { 6 })?;
            let key_ptr = add(table & !3, i64::from((word & !7) >> 1))?;
            self.check(key_ptr, 1)?;
            strings.seek(key_ptr)?;
            if self.local_string_equals(strings, key_ptr, key)? {
                return self.encoded_value(s, flags, value_ptr).map(Some);
            }
            if flags & 4 != 0 {
                return Ok(None);
            }
            p = add(p, -i64::from(6 + (flags & 2)))?;
        }
    }
    #[inline(always)]
    pub(crate) fn encoded_local_tag(
        self,
        key: &str,
        window: &mut ReadWindow<'_, S>,
        strings: &mut ReadWindow<'_, S>,
    ) -> Result<Option<EncodedValue>> {
        self.encoded_tag(key, None, window, strings)
    }
    #[inline]
    fn encoded_value(self, s: &impl Source, flags: i32, p: u64) -> Result<EncodedValue> {
        self.check(p, if flags & 2 != 0 { 4 } else { 2 })?;
        Ok(match flags & 3 {
            0 => EncodedValue::Number {
                mantissa: i32::from(s.u16(p)?) - 256,
                scale: 0,
            },
            1 => {
                let code = s.u16(p)?;
                if usize::from(code) >= self.library.string_count {
                    return Err(Error::invalid(p, "undefined global string code"));
                }
                EncodedValue::Global(code)
            }
            2 => {
                let n = s.u32(p)?;
                EncodedValue::Number {
                    mantissa: (n >> 2) as i32 - 256,
                    scale: (n & 3) as u8,
                }
            }
            _ => EncodedValue::Local {
                field: p,
                delta: s.i32(p)?,
            },
        })
    }
    #[inline]
    pub(crate) fn local_value_target(self, field: u64, delta: i32) -> Result<u64> {
        let target = add(field, i64::from(delta))?;
        self.check(target, 1)?;
        Ok(target)
    }
    fn tag_value(self, flags: i32, p: u64) -> Result<TagValue<'a>> {
        let s = &self.library.source;
        self.check(p, if flags & 2 != 0 { 4 } else { 2 })?;
        Ok(match flags & 3 {
            0 => TagValue::Number {
                mantissa: i32::from(s.u16(p)?) - 256,
                scale: 0,
            },
            1 => TagValue::Text(self.library.global_string(s.u16(p)?)?),
            2 => {
                let n = s.u32(p)?;
                TagValue::Number {
                    mantissa: (n >> 2) as i32 - 256,
                    scale: (n & 3) as u8,
                }
            }
            _ => {
                let target = s.relative(p, !0)?;
                self.check(target, 1)?;
                TagValue::Text(self.local_string(target)?)
            }
        })
    }
    fn local_string(self, start: u64) -> Result<Cow<'a, str>> {
        self.local_string_source(&self.library.source, start)
    }
    fn local_string_source<'s>(self, source: &'s impl Source, start: u64) -> Result<Cow<'s, str>> {
        self.check(start, 1)?;
        let mut cursor = start;
        let length = source.varint(&mut cursor)?;
        if length > 32767 {
            return Err(Error::invalid(start, "string exceeds format limit"));
        }
        self.check(start, cursor - start + length)?;
        source.text(cursor, length as usize)
    }
    fn local_bytes_source<'s>(self, source: &'s impl Source, start: u64) -> Result<Cow<'s, [u8]>> {
        self.check(start, 1)?;
        let mut cursor = start;
        let length = source.varint(&mut cursor)?;
        if length > 32767 {
            return Err(Error::invalid(start, "string exceeds format limit"));
        }
        self.check(start, cursor - start + length)?;
        if let Some(bytes) = source.view(cursor, length as usize)? {
            return Ok(Cow::Borrowed(bytes));
        }
        let mut out = vec![0; length as usize];
        source.read_exact_at(cursor, &mut out)?;
        Ok(Cow::Owned(out))
    }
    /// Return the payload of a one-byte-length string if it fits in both
    /// the current window and the tile. Longer encodings use the full decoder.
    #[inline]
    fn short_local_bytes<'s>(
        self,
        source: &'s ReadWindow<'_, S>,
        start: u64,
    ) -> Result<Option<&'s [u8]>> {
        self.check(start, 1)?;
        Ok(source.tail(start, self.tile_end()).and_then(|bytes| {
            let (&length, tail) = bytes.split_first()?;
            if length < 128 {
                tail.get(..usize::from(length))
            } else {
                None
            }
        }))
    }

    #[inline]
    pub(crate) fn local_bytes_window<'s>(
        self,
        source: &'s ReadWindow<'_, S>,
        start: u64,
    ) -> Result<Cow<'s, [u8]>> {
        if let Some(bytes) = self.short_local_bytes(source, start)? {
            return Ok(Cow::Borrowed(bytes));
        }
        self.local_bytes_source(source, start)
    }

    #[inline]
    pub(crate) fn local_string_window<'s>(
        self,
        source: &'s ReadWindow<'_, S>,
        start: u64,
    ) -> Result<Cow<'s, str>> {
        if let Some(bytes) = self.short_local_bytes(source, start)? {
            return std::str::from_utf8(bytes)
                .map(Cow::Borrowed)
                .map_err(|_| Error::invalid(start + 1, "invalid UTF-8"));
        }
        self.local_string_source(source, start)
    }

    #[inline]
    pub(crate) fn local_string_equals(
        self,
        source: &ReadWindow<'_, S>,
        start: u64,
        expected: &str,
    ) -> Result<bool> {
        if let Some(bytes) = self.short_local_bytes(source, start)? {
            return Ok(bytes == expected.as_bytes());
        }
        Ok(self.local_bytes_source(source, start)?.as_ref() == expected.as_bytes())
    }
    /// Stream the vertices of a way in order, including the closing point for an area way.
    pub fn coordinates(self) -> Result<Coordinates<'a, S>> {
        Coordinates::new(self)
    }
    /// Stream the members of a relation in stored order, with their roles.
    pub fn members(self) -> Result<Members<'a, S>> {
        Members::new(self)
    }
}

/// Validated inline tag payload. Referenced local strings stay lazy.
#[derive(Clone, Copy)]
pub(crate) enum EncodedValue {
    Number { mantissa: i32, scale: u8 },
    Global(u16),
    Local { field: u64, delta: i32 },
}

/// Numeric tags stay numeric without allocating a temporary decimal string.
/// Display preserves the encoded decimal scale.
#[derive(Clone, Debug, PartialEq)]
pub enum TagValue<'a> {
    /// A UTF-8 string borrowed from the source when available.
    Text(Cow<'a, str>),
    /// A fixed-point decimal whose value is `mantissa / 10^scale`.
    Number {
        /// Signed digits before applying the decimal scale.
        mantissa: i32,
        /// Number of decimal places.
        scale: u8,
    },
}
impl TagValue<'_> {
    /// Convert a number to floating point, or parse an entire text value as a number.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Self::Text(s) => s.parse().ok(),
            Self::Number { mantissa, scale } => {
                Some(f64::from(*mantissa) / 10f64.powi(i32::from(*scale)))
            }
        }
    }
}
impl fmt::Display for TagValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(s) => f.write_str(s),
            Self::Number { mantissa, scale: 0 } => write!(f, "{mantissa}"),
            Self::Number { mantissa, scale } => {
                let absolute = i64::from(*mantissa).abs();
                if *mantissa < 0 {
                    f.write_str("-")?;
                }
                if *scale > 9 {
                    return write!(f, "0.{:0width$}", absolute, width = usize::from(*scale));
                }
                let divisor = 10i64.pow(u32::from(*scale));
                write!(
                    f,
                    "{}.{:0width$}",
                    absolute / divisor,
                    absolute % divisor,
                    width = usize::from(*scale)
                )
            }
        }
    }
}
/// A decoded OSM tag.
#[derive(Clone, Debug, PartialEq)]
pub struct Tag<'a> {
    /// The tag's key.
    pub key: Cow<'a, str>,
    /// The text or numeric value stored in the GOL.
    pub value: TagValue<'a>,
}

#[inline]
fn decode_inline_value<'a>(
    tile: &'a [u8],
    dict: &'a crate::library::Dictionary,
    val_off: usize,
    kind: u32,
    narrow: u16,
) -> Option<TagValue<'a>> {
    match kind {
        0 => Some(TagValue::Number {
            mantissa: i32::from(narrow) - 256,
            scale: 0,
        }),
        1 => Some(TagValue::Text(Cow::Borrowed(dict.get(narrow)?))),
        2 => {
            let wide = u32::from_le_bytes(*tile.get(val_off..)?.first_chunk::<4>()?);
            Some(TagValue::Number {
                mantissa: (wide >> 2) as i32 - 256,
                scale: (wide & 3) as u8,
            })
        }
        _ => {
            let delta = i32::from_le_bytes(*tile.get(val_off..)?.first_chunk::<4>()?);
            let target = val_off.checked_add_signed(isize::try_from(delta).ok()?)?;
            let (&len, tail) = tile.get(target..)?.split_first()?;
            if len >= 128 {
                return None;
            }
            let text = std::str::from_utf8(tail.get(..usize::from(len))?).ok()?;
            Some(TagValue::Text(Cow::Borrowed(text)))
        }
    }
}

/// Fallible iterator over a feature's tag table.
pub struct Tags<'a, S> {
    feature: Feature<'a, S>,
    tile_bytes: Option<&'a [u8]>,
    dictionary: Option<&'a crate::library::Dictionary>,
    table: u64,
    local: bool,
    next: Option<u64>,
    steps: u64,
}
impl<'a, S: Source> Tags<'a, S> {
    #[inline]
    fn new(feature: Feature<'a, S>) -> Result<Self> {
        let local = feature.tag_pointer & 1 != 0;
        let table = feature.checked_tag_table()?;
        let tile_bytes = feature.tile_bytes();
        let dictionary = feature.library.strings.get();
        let first = if let Some(bytes) = tile_bytes {
            let off = (table - feature.tile_start()) as usize;
            if let Some(chunk) = bytes.get(off..).and_then(|b| b.first_chunk::<4>()) {
                u32::from_le_bytes(*chunk)
            } else {
                feature.library.source.u32(table)?
            }
        } else {
            feature.library.source.u32(table)?
        };
        let next = if first == 0x8001 {
            if local { Some(add(table, -6)?) } else { None }
        } else {
            Some(table)
        };
        Ok(Self {
            feature,
            tile_bytes,
            dictionary,
            table,
            local,
            next,
            steps: 0,
        })
    }

    #[inline]
    fn read_global(&mut self, p: u64) -> Option<Tag<'a>> {
        let tile = self.tile_bytes?;
        let dictionary = self.dictionary?;
        let off = (p - self.feature.tile_start()) as usize;
        let bytes = tile.get(off..)?;
        let word = u32::from_le_bytes(*bytes.first_chunk()?);
        let key = Cow::Borrowed(dictionary.get(((word >> 2) & 8191) as u16)?);
        let narrow = (word >> 16) as u16;
        let value = decode_inline_value(tile, dictionary, off + 2, word & 3, narrow)?;
        self.next = if word & 0x8000 != 0 {
            if self.local {
                Some(self.table.checked_sub(6)?)
            } else {
                None
            }
        } else {
            Some(p + 4 + u64::from(word & 2))
        };
        Some(Tag { key, value })
    }

    #[inline]
    fn read_local(&mut self, p: u64) -> Option<Tag<'a>> {
        let tile = self.tile_bytes?;
        let dictionary = self.dictionary?;
        let tile_start = self.feature.tile_start();
        let off = usize::try_from(p.checked_sub(tile_start)?).ok()?;
        let entry = tile.get(off..off.checked_add(6)?)?;
        let word = i32::from_le_bytes(entry[2..6].try_into().ok()?);
        let flags = word & 7;
        let aligned_table = (self.table & !3).checked_sub(tile_start)? as i64;
        let key_off =
            usize::try_from(aligned_table.checked_add(i64::from((word & !7) >> 1))?).ok()?;
        let (&klen, ktail) = tile.get(key_off..)?.split_first()?;
        if klen >= 128 {
            return None;
        }
        let key_str = std::str::from_utf8(ktail.get(..usize::from(klen))?).ok()?;
        let val_off = if flags & 2 != 0 {
            off.checked_sub(2)?
        } else {
            off
        };
        let narrow = u16::from_le_bytes(tile.get(val_off..val_off + 2)?.try_into().ok()?);
        let value = decode_inline_value(tile, dictionary, val_off, (flags & 3) as u32, narrow)?;
        self.next = if flags & 4 != 0 {
            None
        } else {
            Some(p.checked_sub(u64::from(6 + (flags & 2) as u32))?)
        };
        Some(Tag {
            key: Cow::Borrowed(key_str),
            value,
        })
    }

    #[inline(never)]
    fn read_fallback(&mut self, p: u64) -> Result<Tag<'a>> {
        let f = self.feature;
        let lib = f.library;
        let s = &lib.source;
        let (key, flags, value_ptr) = if p < self.table {
            f.check(p, 6)?;
            let word = s.i32(p + 2)?;
            let flags = word & 7;
            let key_ptr = add(self.table & !3, ((word & !7) >> 1) as i64)?;
            f.check(key_ptr, 1)?;
            let key = f.local_string(key_ptr)?;
            self.next = if flags & 4 != 0 {
                None
            } else {
                Some(add(p, -(6 + (flags & 2)) as i64)?)
            };
            (key, flags, if flags & 2 != 0 { add(p, -2)? } else { p })
        } else {
            f.check(p, 4)?;
            let word = s.u16(p)?;
            let key = lib.global_string((word >> 2) & 8191)?;
            self.next = if word & 0x8000 != 0 {
                if self.local {
                    Some(add(self.table, -6)?)
                } else {
                    None
                }
            } else {
                Some(p + 4 + u64::from(word & 2))
            };
            (key, i32::from(word & 3), p + 2)
        };
        let value = f.tag_value(flags, value_ptr)?;
        if self.dictionary.is_none() {
            self.dictionary = lib.strings.get();
        }
        Ok(Tag { key, value })
    }
}
impl<'a, S: Source> Iterator for Tags<'a, S> {
    type Item = Result<Tag<'a>>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let p = self.next?;
        self.steps += 1;
        if self.steps > self.feature.library.limits.max_tags {
            self.next = None;
            return Some(Err(Error::Limit("tags")));
        }
        if p >= self.table {
            if let Some(tag) = self.read_global(p) {
                return Some(Ok(tag));
            }
        } else if let Some(tag) = self.read_local(p) {
            return Some(Ok(tag));
        }
        let result = self.read_fallback(p);
        if result.is_err() {
            self.next = None;
        }
        Some(result)
    }
}
impl<S: Source> std::iter::FusedIterator for Tags<'_, S> {}

/// Fallible iterator over the integer Mercator vertices of a way.
pub struct Coordinates<'a, S> {
    feature: Feature<'a, S>,
    window: ReadWindow<'a, S>,
    p: u64,
    remaining: u64,
    current: Point,
    first: Option<Point>,
    close: bool,
    done: bool,
}
impl<'a, S: Source> Coordinates<'a, S> {
    #[cfg(feature = "way-checkpoints")]
    pub(crate) fn stored_count(&self) -> u64 {
        self.remaining
    }

    #[cfg(feature = "way-checkpoints")]
    pub(crate) fn position(&self) -> u64 {
        self.p
    }

    #[cfg(feature = "way-checkpoints")]
    pub(crate) fn restarted(
        feature: Feature<'a, S>,
        p: u64,
        current: Point,
        remaining: u64,
    ) -> Self {
        Self {
            feature,
            window: ReadWindow::new(&feature.library.source),
            p,
            remaining,
            current,
            first: None,
            close: false,
            done: false,
        }
    }

    fn new(feature: Feature<'a, S>) -> Result<Self> {
        if feature.kind() != FeatureType::Way {
            return Err(Error::WrongFeatureType);
        }
        let mut p = feature.body()?;
        let remaining = feature.library.source.varint(&mut p)?;
        if remaining > feature.library.limits.max_coordinates {
            return Err(Error::Limit("way coordinates"));
        }
        Ok(Self {
            feature,
            window: ReadWindow::new(&feature.library.source),
            p,
            remaining,
            current: feature.bounds()?.min,
            first: None,
            close: feature.is_area(),
            done: false,
        })
    }
    fn read(&mut self) -> Result<Point> {
        // Decode both short deltas with one checked window lookup. Use the
        // existing path for long varints, window misses and tile boundaries.
        if let Some((x, y, used)) = self
            .window
            .record::<6, false>(self.p, self.feature.tile_end())
            .and_then(short_coordinate_pair)
        {
            self.p += used;
            return Ok(self.advance(x, y));
        }
        self.window.seek(self.p)?;
        self.read_fallback()
    }

    fn read_fallback(&mut self) -> Result<Point> {
        let s = &self.window;
        let x = s.signed_varint(&mut self.p)?;
        let y = s.signed_varint(&mut self.p)?;
        self.feature.check(self.p - 1, 1)?;
        // The difference between two i32 coordinates may require 33 signed
        // bits. Some producers encode that full difference, others wrap it to
        // i32; both recover the same coordinate modulo 2^32.
        let limit = i64::from(u32::MAX);
        if !(-limit..=limit).contains(&x) || !(-limit..=limit).contains(&y) {
            return Err(Error::invalid(
                self.p,
                "coordinate delta exceeds coordinate domain",
            ));
        }
        Ok(self.advance(x as i32, y as i32))
    }

    #[inline]
    fn advance(&mut self, x: i32, y: i32) -> Point {
        self.current.x = self.current.x.wrapping_add(x);
        self.current.y = self.current.y.wrapping_add(y);
        self.first.get_or_insert(self.current);
        self.remaining -= 1;
        self.current
    }

    /// Collect a fresh way iterator whose full coordinate count fits the
    /// geometry budget. A checked window supplies runs of short deltas.
    pub(crate) fn collect_owned(mut self, capacity: usize) -> Result<Vec<Point>> {
        let mut out = Vec::with_capacity(capacity);
        while self.remaining != 0 {
            self.window.seek(self.p)?;
            let mut tail = self
                .window
                .tail(self.p, self.feature.tile_end())
                .unwrap_or_default();
            let original_len = tail.len();
            while self.remaining != 0 {
                let Some((x, y, used)) = tail.first_chunk::<6>().and_then(short_coordinate_pair)
                else {
                    break;
                };
                self.current.x = self.current.x.wrapping_add(x);
                self.current.y = self.current.y.wrapping_add(y);
                self.remaining -= 1;
                out.push(self.current);
                tail = &tail[used as usize..];
            }
            let used = original_len - tail.len();
            self.p += used as u64;
            if self.remaining != 0 {
                if used != 0 {
                    self.window.seek(self.p)?;
                }
                out.push(self.read_fallback()?);
            }
        }
        if self.close {
            if let Some(&first) = out.first() {
                out.push(first);
            }
        }
        Ok(out)
    }
}
#[inline]
fn short_coordinate_pair(bytes: &[u8; 6]) -> Option<(i32, i32, u64)> {
    fn read(bytes: &[u8]) -> Option<(u32, usize)> {
        let &[a, b, c] = bytes.first_chunk::<3>()?;
        if a < 128 {
            return Some((u32::from(a), 1));
        }
        let low = u32::from(a & 127);
        if b < 128 {
            return Some((low | u32::from(b) << 7, 2));
        }
        if c < 128 {
            return Some((low | u32::from(b & 127) << 7 | u32::from(c) << 14, 3));
        }
        None
    }
    let (x, x_len) = read(bytes)?;
    let (y, y_len) = read(&bytes[x_len..])?;
    Some((
        zigzag_coordinate(x),
        zigzag_coordinate(y),
        (x_len + y_len) as u64,
    ))
}

fn zigzag_coordinate(n: u32) -> i32 {
    ((n >> 1) as i32) ^ -((n & 1) as i32)
}

impl<S: Source> Iterator for Coordinates<'_, S> {
    type Item = Result<Point>;
    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.done {
            return (0, Some(0));
        }
        if self.remaining == 0 {
            let closing = usize::from(self.close && self.first.is_some());
            return (closing, Some(closing));
        }
        // Corrupt input may terminate with one error before the declared count.
        // This is an upper bound, not an ExactSizeIterator promise.
        let upper = usize::try_from(self.remaining)
            .ok()
            .and_then(|n| n.checked_add(usize::from(self.close)));
        (1, upper)
    }
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.remaining == 0 {
            self.done = true;
            return if self.close { self.first.map(Ok) } else { None };
        }
        let result = self.read();
        if result.is_err() {
            self.done = true;
        }
        Some(result)
    }
}
impl<S: Source> std::iter::FusedIterator for Coordinates<'_, S> {}

/// A feature referenced by a relation, together with its role.
#[derive(Debug)]
pub struct Member<'a, S> {
    /// The referenced node, way or relation.
    pub feature: Feature<'a, S>,
    /// The role string, which can be empty.
    pub role: Cow<'a, str>,
}
/// Fallible iterator over relation members in stored order.
pub struct Members<'a, S> {
    owner: Feature<'a, S>,
    next: Option<u64>,
    role: Cow<'a, str>,
    tip: i64,
    tex: i64,
    cached_foreign: Option<crate::links::ExportTable>,
    steps: u64,
}
impl<'a, S: Source> Members<'a, S> {
    fn new(owner: Feature<'a, S>) -> Result<Self> {
        if owner.kind() != FeatureType::Relation {
            return Err(Error::WrongFeatureType);
        }
        let p = owner.body()?;
        owner.check(p, 4)?;
        let next = if owner.library.source.u32(p)? == 0 {
            None
        } else {
            Some(p)
        };
        Ok(Self {
            owner,
            next,
            role: Cow::Borrowed(""),
            tip: 0x4000,
            tex: 0x400,
            cached_foreign: None,
            steps: 0,
        })
    }
    fn read(&mut self, start: u64) -> Result<Member<'a, S>> {
        let owner = self.owner;
        let lib = owner.library;
        let s = &lib.source;
        self.steps += 1;
        if self.steps > lib.limits.max_members {
            return Err(Error::Limit("relation members"));
        }
        owner.check(start, 2)?;
        let narrow = s.u16(start)?;
        let foreign = narrow & 2 != 0;
        let width = if foreign && narrow & 16 == 0 { 2 } else { 4 };
        owner.check(start, width)?;
        let word = if width == 2 {
            i32::from(narrow as i16)
        } else {
            s.i32(start)?
        };
        let mut p = start + width;
        if foreign {
            if word & 8 != 0 {
                owner.check(p, 2)?;
                let delta = s.u16(p)? as i16;
                let delta = if delta & 1 != 0 {
                    owner.check(p, 4)?;
                    let d = s.i32(p)?;
                    p += 4;
                    d >> 1
                } else {
                    p += 2;
                    i32::from(delta) >> 1
                };
                self.tip += i64::from(delta);
            }
            self.tex += i64::from(word >> 5);
        }
        if word & 4 != 0 {
            owner.check(p, 2)?;
            let role = s.u16(p)?;
            if role & 1 != 0 {
                self.role = lib.global_string(role >> 1)?;
                p += 2;
            } else {
                owner.check(p, 4)?;
                let target = add(p, (s.i32(p)? >> 1) as i64)?;
                owner.check(target, 1)?;
                self.role = owner.local_string(target)?;
                p += 4;
            }
        }
        let feature = if foreign {
            let tip = u32::try_from(self.tip)
                .map_err(|_| Error::invalid(start, "invalid foreign tile"))?;
            let tex = u32::try_from(self.tex)
                .map_err(|_| Error::invalid(start, "invalid export index"))?;
            crate::links::foreign_feature(lib, &mut self.cached_foreign, tip, tex)?
        } else {
            Feature::at_source(
                lib,
                &lib.source,
                add(start & !3, i64::from(word >> 3) * 4)?,
                owner.tile,
            )?
        };
        self.next = if word & 1 != 0 { None } else { Some(p) };
        Ok(Member {
            feature,
            role: self.role.clone(),
        })
    }
}
impl<'a, S: Source> Iterator for Members<'a, S> {
    type Item = Result<Member<'a, S>>;
    fn next(&mut self) -> Option<Self::Item> {
        let p = self.next?;
        let result = self.read(p);
        if result.is_err() {
            self.next = None;
        }
        Some(result)
    }
}
impl<S: Source> std::iter::FusedIterator for Members<'_, S> {}

#[cfg(test)]
mod compact_handle_tests;
#[cfg(test)]
mod coordinate_hints;
#[cfg(test)]
mod optimized_query_errors;

#[cfg(test)]
mod tag_iteration_errors;
