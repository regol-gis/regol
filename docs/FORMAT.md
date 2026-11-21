# GOL 2.0 format notes

This is an evolving, independently written record of facts established from the
pinned Apache-2.0 Java reference and real GOL Tool 2.0.0 fixtures. [Implementation provenance](PROVENANCE.md)
It is not yet a complete format specification. All fixed-width fields observed
here are little-endian. Addresses below are byte offsets unless specified.

## Store and snapshot

- Header magic at 0 is `0x1ce50d6e`; format word at 4 is `2`.
- Active snapshot byte at 16 selects the snapshot at `128 + 64 * selector`.
- Snapshot offsets 0 and 4 hold separate unsigned 32-bit values for the
  revision and the modified-since revision. `regol info` reports the first value.
- Snapshot offset 24 contains the tile-index page; offset 28 contains tile count.
- Pages in this target are 4096 bytes. The final page need not be physically full.
- Header offset 20 gives the byte length of the metadata at page 1. Offset 24
  holds the total allocated page count. Header CRC32C at 504 covers bytes 0..504.
- Offset 108 counts tile-index payload words. CRC32C at 112 covers the metadata.
  CRC32C at 116 covers the complete tile index with every tile page entry zeroed.
- Each tile ends with a CRC32C over its length word and all preceding payload
  bytes. A blob must fit entirely within one 1 GiB mapping segment.
- Header offset 84 is an absolute global-string-table pointer; 88 is the index
  schema pointer; 92 the properties pointer; 96 the zoom-level bitset.
- Header word 80 bit 0 enables stored geometry-node IDs. Properties start with
  a u16 pair count and length-prefixed UTF-8 keys and values. The index schema
  starts with a u32 count followed by u32 entries: low u16 global key code,
  high u16 category (1..30). Category c corresponds to index bit c-1.
- The string table begins with a u16 count and then length-prefixed UTF-8 strings.
  Lengths are base-128 unsigned varints. Code 0 is the empty string.

## Tile index and spatial indexes

- Tile index begins with a payload-byte-length word. Root tile entry is at +4.
- An entry's low two bits `10` indicate a ready tile; remaining bits hold its
  page. `01` instead indicates a signed relative pointer to a child-level record.
- A child-level record contains the tile entry, an occupancy mask, then entries
  for set bits in row-major order. Each level advances 1..3 zooms. Eight-by-eight
  masks occupy 8 bytes; smaller masks occupy 4.
- A tile starts with a payload-length word; its own header has relative pointers
  at +8/+12/+16/+20 for nodes/nonarea ways/areas/nonarea relations, and +24 for
  exports. A zero pointer means an absent index.
- Each spatial index has 8-byte root entries (relative pointer with terminal bit,
  key-category bits). The tree has 20-byte branches: pointer and integer bbox.
  Pointer bit 0 ends a sibling list; bit 1 targets leaves. Mask low two bits before
  adding the signed displacement to the pointer field's address.
- Node leaves: x/y, feature header and tag pointer; 20 bytes, plus 4 for relation
  membership. Other leaves: bbox, feature header, tag pointer, body pointer;
  32 bytes. Feature-header bit 0 terminates a leaf list.
- North/west duplication flags identify copies in neighboring tiles. Suppression
  depends on query bounds and tile bounds; there is no result-sized dedup set.

## Features and tags

- A feature anchor points to a u64: 52-bit ID above 12 flag bits.
- Type is bits 3..4: node=0, way=1, relation=2. Area flag is bit 1.
- Coordinates or bbox precede the anchor. Tags are addressed by a signed relative
  pointer at anchor+8; bit 0 indicates local keys. Body pointer is at anchor+12.
- Common-key tags run forward from the table anchor. Key occupies bits 2..14 of
  the first u16; bit 15 terminates. Two type bits specify number/global string/
  wide number/local string. Entries are 4 or 6 bytes.
- Local-key tags run backward before the anchor. Their signed key displacement
  is scaled by 2, relative to the table anchor rounded down to 4-byte alignment.
  Three flag bits identify value representation and last entry. Values precede
  the key field. Entries are 6 or 8 bytes.
- `0x00008001` at the table anchor indicates no common-key tags; it is not a real
  empty-key tag. The pinned CLI exporter sometimes exposes it as `"":""`.
- Narrow numbers are u16 minus 256. Wide numbers have a 30-bit biased mantissa
  and a 2-bit decimal scale. Keep their decimal identity, avoiding float rounding.
- Local strings use signed relative pointers and length-prefixed UTF-8.
- A globally common value may also occur as a rare local key. Raw tag access
  exposes both encodings. GOQL selects the representation using the library
  dictionary, matching the native selector convention. [Query semantics](COMPATIBILITY.md#goql-matrix)

## Ways and relation members

- Way body starts with vertex count followed by zigzag-varint x/y deltas, starting
  from bbox minimum. Coordinates occupy a wrapping 32-bit domain, but a valid signed delta can
  require 33 bits (the difference between two extreme i32 coordinates).
- Area ways omit the repeated closing coordinate in storage; iteration restores it.
- When header flag 80/bit 0 is set, the coordinate stream is immediately followed
  by signed-varint deltas of original node IDs, starting at zero. There is one ID
  per stored coordinate; the closing area ID is likewise restored. Verified
  independently against original PBF references for a 64,412-feature extract.
- Relation bodies contain variable-width entries in member order. Flags identify
  final entry, foreign reference, changed role, changed tile, wide export delta.
- Local member pointers are relative to the entry rounded down to four bytes;
  signed field shifted right by 3 and scaled by 4 gives the displacement.
- Foreign references accumulate tile-index deltas (initial 0x4000) and export
  index deltas (initial 0x400). Exports are relative pointers to feature anchors.
- Roles persist until changed. Odd role words contain a global string code;
  even role words contain a signed relative pointer scaled by two.

## Encoding coverage and boundaries

The synthetic wide-export fixture exercises wide relation-member and backward
way-node export deltas. The optional wide-tile generator also exercises tile
deltas beyond the narrow encoding. Use `tools/inspect_wide_encodings.py` to
inspect a generated GOL.

A synthetic completed snapshot test retains distinct old/new tile generations
and directories in one file and verifies both active snapshot selections. The
pinned producer has no update command, so this is not evidence of an external
producer update workflow. Concurrent live updates are outside the storage contract.

These notes document the exercised GOL 2.0 encodings, not arbitrary extensions.
Unsupported versions and unavailable tiles fail explicitly.
