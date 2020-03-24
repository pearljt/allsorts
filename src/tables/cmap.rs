//! Parsing and writing of the `cmap` table.
//!
//! > This table defines the mapping of character codes to the glyph index values used in the font.
//! > It may contain more than one subtable, in order to support more than one character encoding
//! > scheme.
//!
//! — <https://docs.microsoft.com/en-us/typography/opentype/spec/cmap>

use std::collections::HashMap;
use std::convert::TryFrom;

use itertools::izip;

use crate::binary::read::{CheckIndex, ReadArray, ReadBinary, ReadCtxt, ReadFrom, ReadScope};
use crate::binary::write::{WriteBinary, WriteContext};
use crate::binary::{I16Be, U16Be, U32Be, U8};
use crate::error::{ParseError, WriteError};
use crate::size;

const SUB_HEADER_SIZE: usize = 4 * 2;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct PlatformId(pub u16);

impl PlatformId {
    pub const UNICODE: PlatformId = PlatformId(0);
    pub const MACINTOSH: PlatformId = PlatformId(1);
    pub const WINDOWS: PlatformId = PlatformId(3);
    pub const CUSTOM: PlatformId = PlatformId(4);
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct EncodingId(pub u16);

impl EncodingId {
    pub const WINDOWS_SYMBOL: EncodingId = EncodingId(0);
    pub const WINDOWS_UNICODE_BMP_UCS2: EncodingId = EncodingId(1);
    pub const WINDOWS_SHIFT_JIS: EncodingId = EncodingId(2);
    pub const WINDOWS_PRC: EncodingId = EncodingId(3);
    pub const WINDOWS_BIG5: EncodingId = EncodingId(4);
    pub const WINDOWS_WANSUNG: EncodingId = EncodingId(5);
    pub const WINDOWS_JOHAB: EncodingId = EncodingId(6);
    // pub const WINDOWS_RESERVED: EncodingId = EncodingId(7);
    // pub const WINDOWS_RESERVED: EncodingId = EncodingId(8);
    // pub const WINDOWS_RESERVED: EncodingId = EncodingId(9);
    pub const WINDOWS_UNICODE_UCS4: EncodingId = EncodingId(10);

    pub const MACINTOSH_APPLE_ROMAN: EncodingId = EncodingId(0);
    pub const MACINTOSH_UNICODE_UCS4: EncodingId = EncodingId(4);
}

pub struct Cmap<'a> {
    pub scope: ReadScope<'a>,
    encoding_records: ReadArray<'a, EncodingRecord>,
}

#[derive(Copy, Clone)]
pub struct EncodingRecord {
    pub platform_id: u16,
    pub encoding_id: u16,
    pub offset: u32,
}

pub enum CmapSubtable<'a> {
    Format0 {
        language: u16,
        glyph_id_array: ReadArray<'a, U8>,
    },
    Format2 {
        language: u16,
        sub_header_keys: ReadArray<'a, U16Be>,
        sub_headers: ReadArray<'a, SubHeader>,
        sub_headers_scope: ReadScope<'a>,
    },
    Format4 {
        language: u16,
        end_codes: ReadArray<'a, U16Be>,
        start_codes: ReadArray<'a, U16Be>,
        id_deltas: ReadArray<'a, I16Be>,
        id_range_offsets: ReadArray<'a, U16Be>,
        glyph_id_array: ReadArray<'a, U16Be>,
    },
    Format6 {
        language: u16,
        first_code: u16,
        glyph_id_array: ReadArray<'a, U16Be>,
    },
    Format10 {
        language: u32,
        start_char_code: u32,
        glyph_id_array: ReadArray<'a, U16Be>,
    },
    Format12 {
        language: u32,
        groups: ReadArray<'a, SequentialMapGroup>,
    },
}

// cmap subtable format 2 sub-header
pub struct SubHeader {
    first_code: u16,
    entry_count: u16,
    id_delta: i16,
    id_range_offset: u16,
}

#[derive(Copy, Clone)]
struct Format4Calculator {
    seg_count: u16,
}

pub struct SequentialMapGroup {
    start_char_code: u32,
    end_char_code: u32,
    start_glyph_id: u32,
}

impl<'a> ReadBinary<'a> for Cmap<'a> {
    type HostType = Self;

    fn read(ctxt: &mut ReadCtxt<'a>) -> Result<Self, ParseError> {
        let scope = ctxt.scope();
        let version = ctxt.read_u16be()?;
        ctxt.check(version == 0)?;
        let num_tables = usize::from(ctxt.read_u16be()?);
        let encoding_records = ctxt.read_array::<EncodingRecord>(num_tables)?;
        Ok(Cmap {
            scope,
            encoding_records,
        })
    }
}

impl<'a> ReadFrom<'a> for EncodingRecord {
    type ReadType = (U16Be, U16Be, U32Be);
    fn from((platform_id, encoding_id, offset): (u16, u16, u32)) -> Self {
        EncodingRecord {
            platform_id,
            encoding_id,
            offset,
        }
    }
}

impl<'a> ReadBinary<'a> for CmapSubtable<'a> {
    type HostType = Self;

    fn read(ctxt: &mut ReadCtxt<'a>) -> Result<Self, ParseError> {
        let subtable_format = ctxt.read_u16be()?;
        match subtable_format {
            0 => {
                let length = usize::from(ctxt.read_u16be()?);
                ctxt.check(length >= 3 * size::U16 + 256)?;
                let language = ctxt.read_u16be()?;
                let glyph_id_array = ctxt.read_array::<U8>(256)?;
                Ok(CmapSubtable::Format0 {
                    language,
                    glyph_id_array,
                })
            }
            2 => {
                let _length = usize::from(ctxt.read_u16be()?);
                let language = ctxt.read_u16be()?;
                let sub_header_keys = ctxt.read_array::<U16Be>(256)?;

                // value is subHeader index × 8.
                // NOTE(unwrap): Safe because sub_header_keys has a non-zero length
                let max_sub_header_index =
                    sub_header_keys.iter().map(|value| value / 8).max().unwrap();
                let sub_headers_scope = ctxt.scope();
                let sub_headers =
                    ctxt.read_array::<SubHeader>(usize::from(max_sub_header_index) + 1)?;

                Ok(CmapSubtable::Format2 {
                    language,
                    sub_header_keys,
                    sub_headers,
                    sub_headers_scope,
                })
            }
            4 => {
                let length = usize::from(ctxt.read_u16be()?);
                let language = ctxt.read_u16be()?;
                let seg_count_x2 = usize::from(ctxt.read_u16be()?);
                ctxt.check((seg_count_x2 & 1) == 0)?;
                let seg_count = seg_count_x2 >> 1;
                let _search_range = ctxt.read_u16be()?;
                let _entry_selector = ctxt.read_u16be()?;
                let _range_shift = ctxt.read_u16be()?;
                let end_codes = ctxt.read_array::<U16Be>(seg_count)?;
                let _reserved_pad = ctxt.read_u16be()?;
                let start_codes = ctxt.read_array::<U16Be>(seg_count)?;
                let id_deltas = ctxt.read_array::<I16Be>(seg_count)?;
                let id_range_offsets = ctxt.read_array::<U16Be>(seg_count)?;
                ctxt.check(length >= (8 + (4 * seg_count)) * size::U16)?;
                let remaining = length - ((8 + (4 * seg_count)) * size::U16);
                ctxt.check((remaining & 1) == 0)?;
                let num_indices = remaining >> 1;
                let glyph_id_array = ctxt.read_array::<U16Be>(num_indices)?;
                Ok(CmapSubtable::Format4 {
                    language,
                    end_codes,
                    start_codes,
                    id_deltas,
                    id_range_offsets,
                    glyph_id_array,
                })
            }
            6 => {
                let _length = ctxt.read_u16be()?;
                let language = ctxt.read_u16be()?;
                let first_code = ctxt.read_u16be()?;
                let entry_count = usize::from(ctxt.read_u16be()?);
                let glyph_id_array = ctxt.read_array::<U16Be>(entry_count)?;
                Ok(CmapSubtable::Format6 {
                    language,
                    first_code,
                    glyph_id_array,
                })
            }
            10 => {
                let reserved = ctxt.read_u16be()?;
                ctxt.check(reserved == 0)?;
                let _length = ctxt.read_u32be()?;
                let language = ctxt.read_u32be()?;
                let start_char_code = ctxt.read_u32be()?;
                let num_chars = usize::try_from(ctxt.read_u32be()?)?;
                let glyph_id_array = ctxt.read_array::<U16Be>(num_chars)?;
                Ok(CmapSubtable::Format10 {
                    language,
                    start_char_code,
                    glyph_id_array,
                })
            }
            12 => {
                let reserved = ctxt.read_u16be()?;
                ctxt.check(reserved == 0)?;
                let _length = ctxt.read_u32be()?;
                let language = ctxt.read_u32be()?;
                let num_groups = usize::try_from(ctxt.read_u32be()?)?;
                let groups = ctxt.read_array::<SequentialMapGroup>(num_groups)?;
                Ok(CmapSubtable::Format12 { language, groups })
            }
            _ => Err(ParseError::BadVersion),
        }
    }
}

impl<'a> WriteBinary<&Self> for CmapSubtable<'a> {
    type Output = ();

    fn write<C: WriteContext>(ctxt: &mut C, table: &CmapSubtable<'a>) -> Result<(), WriteError> {
        match table {
            CmapSubtable::Format0 {
                language,
                glyph_id_array,
            } => {
                U16Be::write(ctxt, 0u16)?; // format
                U16Be::write(ctxt, u16::try_from(3 * size::U16 + glyph_id_array.len())?)?; // length
                U16Be::write(ctxt, *language)?;
                <&ReadArray<'_, _>>::write(ctxt, glyph_id_array)?;
            }
            CmapSubtable::Format2 { .. } => {
                // Not implemented for now. Format 2 is rarely seen in the wild and would generally
                // not be generated for a subset font (the most common path for font writing)
                return Err(WriteError::NotImplemented);
            }
            CmapSubtable::Format4 {
                language,
                end_codes,
                start_codes,
                id_deltas,
                id_range_offsets,
                glyph_id_array,
            } => {
                let start = ctxt.bytes_written();
                let calc = Format4Calculator {
                    seg_count: u16::try_from(start_codes.len())?,
                };

                U16Be::write(ctxt, 4u16)?; // format
                let length = ctxt.placeholder::<U16Be, _>()?;
                U16Be::write(ctxt, *language)?;
                U16Be::write(ctxt, calc.seg_count_x2())?;
                U16Be::write(ctxt, calc.search_range())?;
                U16Be::write(ctxt, calc.entry_selector())?;
                U16Be::write(ctxt, calc.range_shift())?;
                <&ReadArray<'_, _>>::write(ctxt, end_codes)?;
                U16Be::write(ctxt, 0u16)?; // reserved_pad
                <&ReadArray<'_, _>>::write(ctxt, start_codes)?;
                <&ReadArray<'_, _>>::write(ctxt, id_deltas)?;
                <&ReadArray<'_, _>>::write(ctxt, id_range_offsets)?;
                <&ReadArray<'_, _>>::write(ctxt, glyph_id_array)?;
                ctxt.write_placeholder(length, u16::try_from(ctxt.bytes_written() - start)?)?;
            }
            CmapSubtable::Format6 {
                language,
                first_code,
                glyph_id_array,
            } => {
                let start = ctxt.bytes_written();

                U16Be::write(ctxt, 6u16)?; // format
                let length = ctxt.placeholder::<U16Be, _>()?;
                U16Be::write(ctxt, *language)?;
                U16Be::write(ctxt, *first_code)?;
                U16Be::write(ctxt, u16::try_from(glyph_id_array.len())?)?;
                <&ReadArray<'_, _>>::write(ctxt, glyph_id_array)?;
                ctxt.write_placeholder(length, u16::try_from(ctxt.bytes_written() - start)?)?;
            }
            CmapSubtable::Format10 {
                language,
                start_char_code,
                glyph_id_array,
            } => {
                let start = ctxt.bytes_written();

                U16Be::write(ctxt, 10u16)?; // format
                U16Be::write(ctxt, 0u16)?; // reserved
                let length = ctxt.placeholder::<U32Be, _>()?;
                U32Be::write(ctxt, *language)?;
                U32Be::write(ctxt, *start_char_code)?;
                U32Be::write(ctxt, u32::try_from(glyph_id_array.len())?)?;
                <&ReadArray<'_, _>>::write(ctxt, glyph_id_array)?;
                ctxt.write_placeholder(length, u32::try_from(ctxt.bytes_written() - start)?)?;
            }
            CmapSubtable::Format12 { language, groups } => {
                let start = ctxt.bytes_written();

                U16Be::write(ctxt, 12u16)?; // format
                U16Be::write(ctxt, 0u16)?; // reserved
                let length = ctxt.placeholder::<U32Be, _>()?;
                U32Be::write(ctxt, *language)?;
                U32Be::write(ctxt, u32::try_from(groups.len())?)?;
                <&ReadArray<'_, _>>::write(ctxt, groups)?;
                ctxt.write_placeholder(length, u32::try_from(ctxt.bytes_written() - start)?)?;
            }
        }

        Ok(())
    }
}

impl Format4Calculator {
    fn seg_count_x2(self) -> u16 {
        2 * self.seg_count
    }

    fn search_range(self) -> u16 {
        2 * (2u16.pow((self.seg_count as f64).log2().floor() as u32))
    }

    fn entry_selector(self) -> u16 {
        (self.search_range() as f64 / 2.).log2() as u16
    }

    fn range_shift(self) -> u16 {
        2 * self.seg_count - self.search_range()
    }
}

impl<'a> ReadFrom<'a> for SubHeader {
    type ReadType = ((U16Be, U16Be), (I16Be, U16Be));
    fn from(
        ((first_code, entry_count), (id_delta, id_range_offset)): ((u16, u16), (i16, u16)),
    ) -> Self {
        SubHeader {
            first_code,
            entry_count,
            id_delta,
            id_range_offset,
        }
    }
}

impl SubHeader {
    fn contains(&self, value: u16) -> bool {
        // FIXME replace with this when on Rust >= 1.35
        // (sub_header.first_code..sub_header.first_code + sub_header.entry_count).contains(value)
        value >= self.first_code && value < (self.first_code + self.entry_count)
    }

    fn glyph_index_sub_array<'a>(
        &self,
        index: usize,
        sub_headers_scope: &ReadScope<'a>,
    ) -> Result<ReadArray<'a, U16Be>, ParseError> {
        let sub_header_offset = index * SUB_HEADER_SIZE;

        let glyph_index_sub_array = if self.entry_count > 0 {
            let entry_count = usize::from(self.entry_count);
            // The value of the idRangeOffset is the number of bytes past the actual
            // location of the idRangeOffset word where the glyphIndexArray element
            // corresponding to firstCode appears.
            // https://docs.microsoft.com/en-us/typography/opentype/spec/cmap#format-2-high-byte-mapping-through-table
            //
            // So we take the size of the SubHeader and subtract 2 for the size of the
            // id_range_offset field.
            let first_glpyh_index_offset =
                sub_header_offset + SUB_HEADER_SIZE - 2 + usize::from(self.id_range_offset);
            sub_headers_scope
                .offset(first_glpyh_index_offset)
                .ctxt()
                .read_array::<U16Be>(entry_count)?
        } else {
            ReadArray::empty()
        };

        Ok(glyph_index_sub_array)
    }
}

impl<'a> ReadFrom<'a> for SequentialMapGroup {
    type ReadType = (U32Be, U32Be, U32Be);
    fn from((start_char_code, end_char_code, start_glyph_id): (u32, u32, u32)) -> Self {
        SequentialMapGroup {
            start_char_code,
            end_char_code,
            start_glyph_id,
        }
    }
}

impl WriteBinary for SequentialMapGroup {
    type Output = ();

    fn write<C: WriteContext>(ctxt: &mut C, group: SequentialMapGroup) -> Result<(), WriteError> {
        U32Be::write(ctxt, group.start_char_code)?;
        U32Be::write(ctxt, group.end_char_code)?;
        U32Be::write(ctxt, group.start_glyph_id)?;

        Ok(())
    }
}

impl<'a> Cmap<'a> {
    /// Find the first encoding record for the given `platform_id`
    pub fn find_subtable_for_platform(&self, platform_id: PlatformId) -> Option<EncodingRecord> {
        self.encoding_records
            .iter()
            .find(|record| record.platform_id == platform_id.0)
    }

    /// Find the first encoding record for the given `platform_id` and `encoding_id`
    pub fn find_subtable(
        &self,
        platform_id: PlatformId,
        encoding_id: EncodingId,
    ) -> Option<EncodingRecord> {
        self.encoding_records.iter().find(|record| {
            record.platform_id == platform_id.0 && record.encoding_id == encoding_id.0
        })
    }

    pub fn encoding_records(&self) -> impl Iterator<Item = EncodingRecord> + 'a {
        self.encoding_records.iter()
    }
}

impl<'a> CmapSubtable<'a> {
    pub fn map_glyph(&self, ch: u32) -> Result<Option<u16>, ParseError> {
        match *self {
            CmapSubtable::Format0 {
                ref glyph_id_array, ..
            } => {
                let index = usize::try_from(ch)?;
                if index < glyph_id_array.len() {
                    let glyph_id = glyph_id_array.get_item(index);
                    Ok(Some(u16::from(glyph_id)))
                } else {
                    Ok(None)
                }
            }
            CmapSubtable::Format2 {
                ref sub_header_keys,
                ref sub_headers,
                ref sub_headers_scope,
                ..
            } => {
                let high_byte = ((ch >> 8) & 0xff) as u8;
                let low_byte = ((ch) & 0xff) as u8;

                let header_index_byte = if high_byte == 0
                    && sub_header_keys
                        .check_index(usize::from(low_byte))
                        .map(|_| sub_header_keys.get_item(usize::from(low_byte)))?
                        == 0
                {
                    usize::from(low_byte)
                } else {
                    usize::from(high_byte)
                };

                let sub_header_key = usize::from(
                    // value is subHeader index × 8.
                    sub_header_keys
                        .check_index(header_index_byte)
                        .map(|_| sub_header_keys.get_item(header_index_byte))?
                        / 8,
                );
                let sub_header = sub_headers
                    .check_index(sub_header_key)
                    .map(|_| sub_headers.get_item(sub_header_key))?;

                if !sub_header.contains(u16::from(low_byte)) {
                    return Ok(Some(0));
                }
                let glyph_id_index = u16::from(low_byte) - sub_header.first_code;

                let glyph_index_sub_array =
                    sub_header.glyph_index_sub_array(sub_header_key, sub_headers_scope)?;
                let mut glyph_id = glyph_index_sub_array
                    .check_index(usize::from(glyph_id_index))
                    .map(|_| glyph_index_sub_array.get_item(usize::from(glyph_id_index)))?;

                if glyph_id != 0 {
                    // The idDelta arithmetic is modulo 65536.
                    glyph_id = ((glyph_id as isize + sub_header.id_delta as isize) & 0xffff) as u16
                }

                Ok(Some(glyph_id))
            }
            CmapSubtable::Format4 {
                ref end_codes,
                ref start_codes,
                ref id_deltas,
                ref id_range_offsets,
                ref glyph_id_array,
                ..
            } => {
                for i in 0..end_codes.len() {
                    // Find segment that contains `ch`
                    let end_code = u32::from(end_codes.get_item(i));
                    let start_code = u32::from(start_codes.get_item(i));
                    if start_code <= ch && ch <= end_code {
                        // This segment contains ch
                        let id_delta = i32::from(id_deltas.get_item(i));
                        let id_range_offset = id_range_offsets.get_item(i);
                        let glyph_id = if id_range_offset == 0 {
                            // If the idRangeOffset is 0, the idDelta value is added directly to
                            // the character code offset (i.e. idDelta[i] + c) to get the
                            // corresponding glyph index. The idDelta arithmetic is modulo 65536.
                            (((ch as i32) + id_delta) as u32) & 0xFFFF
                        } else {
                            let index = offset_to_index(
                                i,
                                id_range_offset,
                                ch - start_code,
                                id_range_offsets.len(),
                            )?;
                            ((i32::from(glyph_id_array.get_item(index)) + id_delta) as u32) & 0xFFFF
                        };
                        return Ok(Some(glyph_id as u16));
                    }
                }
                Ok(None)
            }
            CmapSubtable::Format6 {
                first_code,
                ref glyph_id_array,
                ..
            } => {
                let first_code = u32::from(first_code);
                if first_code <= ch {
                    let index = usize::try_from(ch - first_code)?;
                    if index < glyph_id_array.len() {
                        let glyph_id = glyph_id_array.get_item(index);
                        Ok(Some(glyph_id))
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }
            CmapSubtable::Format10 {
                start_char_code,
                ref glyph_id_array,
                ..
            } => {
                if ch >= start_char_code {
                    let index = usize::try_from(ch - start_char_code)?;
                    if index < glyph_id_array.len() {
                        let glyph_id = glyph_id_array.get_item(index);
                        Ok(Some(glyph_id))
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }
            CmapSubtable::Format12 { ref groups, .. } => {
                for group in groups {
                    if group.start_char_code <= ch && ch <= group.end_char_code {
                        let glyph_id = group.start_glyph_id + (ch - group.start_char_code);
                        return Ok(Some(u16::try_from(glyph_id)?));
                    }
                }
                Ok(None)
            }
        }
    }

    /// Extract all the mappings from the sub-table.
    ///
    /// The returned `HashMap` maps glyph indexes to char codes. If more than one char code maps to
    /// the same glyph, the `HashMap` will contain the **first** mapping encountered. Also note that
    /// the char code is not necessarily Unicode. It depends on on the encoding of the cmap
    /// sub-table.
    ///
    /// This method primarily exists to support [GlyphNames](crate::glyph_info::GlyphNames).
    pub(crate) fn mappings(&self) -> Result<HashMap<u16, u32>, ParseError> {
        match self {
            CmapSubtable::Format0 {
                language: _,
                glyph_id_array,
            } => {
                let mut mappings = HashMap::with_capacity(glyph_id_array.len());
                for (ch, gid) in glyph_id_array.iter().enumerate() {
                    // cast is safe as format 0 can only contain 256 glyphs
                    mappings.entry(u16::from(gid)).or_insert(ch as u32);
                }
                Ok(mappings)
            }
            // It's unlikely that a sub-table using format 2 would be selected for mappings as most
            // fonts that contain format 2 would probably contain a platform/encoding combination
            // that uses a different format, which would be selected first. As a result support
            // for it is not yet implemented.
            CmapSubtable::Format2 { .. } => return Err(ParseError::NotImplemented),
            CmapSubtable::Format4 {
                language: _,
                end_codes,
                start_codes,
                id_deltas,
                id_range_offsets,
                glyph_id_array,
            } => {
                let mut mappings = HashMap::with_capacity(glyph_id_array.len());
                let zipped = izip!(
                    start_codes.iter(),
                    end_codes.iter(),
                    id_deltas.iter(),
                    id_range_offsets.iter()
                );
                for (i, (start_code, end_code, id_delta, id_range_offset)) in zipped.enumerate() {
                    for (offset_from_start, ch) in (start_code..=end_code).enumerate() {
                        let glyph_id = if id_range_offset == 0 {
                            ((i32::from(ch) + i32::from(id_delta)) & 0xFFFF) as u16
                        } else {
                            let index = offset_to_index(
                                i,
                                id_range_offset,
                                offset_from_start as u32,
                                id_range_offsets.len(),
                            )?;
                            glyph_id_array.check_index(index)?;
                            ((i32::from(glyph_id_array.get_item(index)) + i32::from(id_delta))
                                & 0xFFFF) as u16
                        };
                        mappings.entry(glyph_id).or_insert(u32::from(ch));
                    }
                }
                Ok(mappings)
            }
            CmapSubtable::Format6 {
                language: _,
                first_code,
                glyph_id_array,
            } => {
                let mut mappings = HashMap::with_capacity(glyph_id_array.len());
                for (index, gid) in glyph_id_array.iter().enumerate() {
                    // cast is safe as the entryCount of the glyphIdArray is a 16-bit value
                    mappings
                        .entry(gid)
                        .or_insert(u32::from(*first_code) + index as u32);
                }
                Ok(mappings)
            }
            CmapSubtable::Format10 {
                language: _,
                start_char_code,
                glyph_id_array,
            } => {
                let mut mappings = HashMap::with_capacity(glyph_id_array.len());
                for (index, gid) in glyph_id_array.iter().enumerate() {
                    let index = u32::try_from(index)?;
                    mappings.entry(gid).or_insert(*start_char_code + index);
                }
                Ok(mappings)
            }
            CmapSubtable::Format12 { groups, .. } => {
                let mut mappings = HashMap::new();
                for record in groups.iter() {
                    for (i, ch) in (record.start_char_code..=record.end_char_code).enumerate() {
                        mappings
                            .entry(u16::try_from(record.start_glyph_id)? + u16::try_from(i)?)
                            .or_insert(ch);
                    }
                }
                Ok(mappings)
            }
        }
    }
}

// For converting cmap format 4 offsets to indexes into the glyph id array.
fn offset_to_index(
    i: usize,
    id_range_offset: u16,
    start_code_offset: u32,
    id_range_offsets_len: usize,
) -> Result<usize, ParseError> {
    // Offset into `id_range_offsets` that `i` represents, * 2 for 16-bit values in the array.
    // cast is safe as i is segment index, which is a u16
    let offset_in_id_range_offsets = u32::from(id_range_offset) + i as u32 * 2;
    // Offset into `glyph_id_array` is offset from `start_code` of `ch` * 2 for 16-bit glyph ids.
    let glyph_id_offset = offset_in_id_range_offsets + start_code_offset * 2;
    // Bounds check, cast is safe as id_range_offsets has max segCount items, a 16-bit value.
    if glyph_id_offset >= id_range_offsets_len as u32 * 2 && (glyph_id_offset & 1) == 0 {
        // Turn the offsets into an index
        Ok(((glyph_id_offset >> 1) as usize) - id_range_offsets_len)
    } else {
        return Err(ParseError::BadIndex);
    }
}

pub mod owned {
    use super::{
        size, Format4Calculator, I16Be, SequentialMapGroup, TryFrom, U16Be, U32Be, WriteBinary,
        WriteContext, WriteError,
    };

    pub struct Cmap {
        pub encoding_records: Vec<EncodingRecord>,
    }

    pub struct EncodingRecord {
        pub platform_id: u16,
        pub encoding_id: u16,
        pub sub_table: CmapSubtable,
    }

    pub enum CmapSubtable {
        Format0 {
            language: u16,
            glyph_id_array: Box<[u8; 256]>,
        },
        Format4 {
            language: u16,
            end_codes: Vec<u16>,
            start_codes: Vec<u16>,
            id_deltas: Vec<i16>,
            id_range_offsets: Vec<u16>,
            glyph_id_array: Vec<u16>,
        },
        Format6 {
            language: u16,
            first_code: u16,
            glyph_id_array: Vec<u16>,
        },
        Format10 {
            language: u32,
            start_char_code: u32,
            glyph_id_array: Vec<u16>,
        },
        Format12 {
            language: u32,
            groups: Vec<SequentialMapGroup>,
        },
    }

    impl<'a> WriteBinary<Self> for Cmap {
        type Output = ();

        fn write<C: WriteContext>(ctxt: &mut C, table: Cmap) -> Result<(), WriteError> {
            let start = ctxt.bytes_written();
            U16Be::write(ctxt, 0u16)?; // version
            U16Be::write(ctxt, u16::try_from(table.encoding_records.len())?)?;

            // encoding records
            let mut offsets = Vec::with_capacity(table.encoding_records.len());
            for record in &table.encoding_records {
                U16Be::write(ctxt, record.platform_id)?;
                U16Be::write(ctxt, record.encoding_id)?;
                let offset = ctxt.placeholder::<U32Be, _>()?;
                offsets.push(offset);
            }

            // sub-tables
            for (record, placeholder) in table.encoding_records.into_iter().zip(offsets.into_iter())
            {
                let offset = u32::try_from(ctxt.bytes_written() - start)?;
                CmapSubtable::write(ctxt, record.sub_table)?;
                ctxt.write_placeholder(placeholder, offset)?;
            }

            Ok(())
        }
    }

    impl<'a> WriteBinary<Self> for CmapSubtable {
        type Output = ();

        fn write<C: WriteContext>(ctxt: &mut C, table: CmapSubtable) -> Result<(), WriteError> {
            match table {
                CmapSubtable::Format0 {
                    language,
                    glyph_id_array,
                } => {
                    U16Be::write(ctxt, 0u16)?; // format
                    U16Be::write(ctxt, u16::try_from(3 * size::U16 + glyph_id_array.len())?)?; // length
                    U16Be::write(ctxt, language)?;
                    ctxt.write_bytes(glyph_id_array.as_ref())?;
                }
                CmapSubtable::Format4 {
                    language,
                    end_codes,
                    start_codes,
                    id_deltas,
                    id_range_offsets,
                    glyph_id_array,
                } => {
                    let start = ctxt.bytes_written();
                    let calc = Format4Calculator {
                        seg_count: u16::try_from(start_codes.len())?,
                    };

                    U16Be::write(ctxt, 4u16)?; // format
                    let length = ctxt.placeholder::<U16Be, _>()?;
                    U16Be::write(ctxt, language)?;
                    U16Be::write(ctxt, calc.seg_count_x2())?;
                    U16Be::write(ctxt, calc.search_range())?;
                    U16Be::write(ctxt, calc.entry_selector())?;
                    U16Be::write(ctxt, calc.range_shift())?;
                    ctxt.write_vec::<U16Be>(end_codes)?;
                    U16Be::write(ctxt, 0u16)?; // reserved_pad
                    ctxt.write_vec::<U16Be>(start_codes)?;
                    ctxt.write_vec::<I16Be>(id_deltas)?;
                    ctxt.write_vec::<U16Be>(id_range_offsets)?;
                    ctxt.write_vec::<U16Be>(glyph_id_array)?;
                    ctxt.write_placeholder(length, u16::try_from(ctxt.bytes_written() - start)?)?;
                }
                CmapSubtable::Format6 {
                    language,
                    first_code,
                    glyph_id_array,
                } => {
                    let start = ctxt.bytes_written();

                    U16Be::write(ctxt, 6u16)?; // format
                    let length = ctxt.placeholder::<U16Be, _>()?;
                    U16Be::write(ctxt, language)?;
                    U16Be::write(ctxt, first_code)?;
                    U16Be::write(ctxt, u16::try_from(glyph_id_array.len())?)?;
                    ctxt.write_vec::<U16Be>(glyph_id_array)?;
                    ctxt.write_placeholder(length, u16::try_from(ctxt.bytes_written() - start)?)?;
                }
                CmapSubtable::Format10 {
                    language,
                    start_char_code,
                    glyph_id_array,
                } => {
                    let start = ctxt.bytes_written();

                    U16Be::write(ctxt, 10u16)?; // format
                    U16Be::write(ctxt, 0u16)?; // reserved
                    let length = ctxt.placeholder::<U32Be, _>()?;
                    U32Be::write(ctxt, language)?;
                    U32Be::write(ctxt, start_char_code)?;
                    U32Be::write(ctxt, u32::try_from(glyph_id_array.len())?)?;
                    ctxt.write_vec::<U16Be>(glyph_id_array)?;
                    ctxt.write_placeholder(length, u32::try_from(ctxt.bytes_written() - start)?)?;
                }
                CmapSubtable::Format12 { language, groups } => {
                    let start = ctxt.bytes_written();

                    U16Be::write(ctxt, 12u16)?; // format
                    U16Be::write(ctxt, 0u16)?; // reserved
                    let length = ctxt.placeholder::<U32Be, _>()?;
                    U32Be::write(ctxt, language)?;
                    U32Be::write(ctxt, u32::try_from(groups.len())?)?;
                    ctxt.write_vec::<SequentialMapGroup>(groups)?;
                    ctxt.write_placeholder(length, u32::try_from(ctxt.bytes_written() - start)?)?;
                }
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::{OpenTypeFile, OpenTypeFont};
    use crate::tag;
    use crate::tests::read_fixture;
    use std::path::Path;

    #[test]
    fn test_calculator() {
        let calc = Format4Calculator { seg_count: 39 };
        assert_eq!(calc.seg_count_x2(), 78);
        assert_eq!(calc.search_range(), 64);
        assert_eq!(calc.entry_selector(), 5);
        assert_eq!(calc.range_shift(), 14);
    }

    #[test]
    fn offset_to_index_start() {
        let i = 0;
        let id_range_offset = 4;
        let start_code_offset = 0;
        let id_range_offsets_len = 2;

        let index = offset_to_index(i, id_range_offset, start_code_offset, id_range_offsets_len);
        assert_eq!(index, Ok(0));
    }

    #[test]
    fn offset_to_index_near_end() {
        let i = 0;
        let id_range_offset = 4;
        let start_code_offset = 222;
        let id_range_offsets_len = 2;

        let index = offset_to_index(i, id_range_offset, start_code_offset, id_range_offsets_len);
        assert_eq!(index, Ok(222));
    }

    fn with_cmap_subtable<P: AsRef<Path>>(
        path: P,
        platform: PlatformId,
        encoding: EncodingId,
        f: impl Fn(CmapSubtable<'_>),
    ) {
        let font_buffer = read_fixture(path);
        let opentype_file = ReadScope::new(&font_buffer)
            .read::<OpenTypeFile<'_>>()
            .unwrap();
        let ttf = match opentype_file.font {
            OpenTypeFont::Single(offset_table) => offset_table,
            OpenTypeFont::Collection(_) => panic!("expected a TTF font"),
        };
        let cmap = ttf
            .read_table(&opentype_file.scope, tag::CMAP)
            .unwrap()
            .unwrap()
            .read::<Cmap<'_>>()
            .unwrap();
        let encoding_record = cmap.find_subtable(platform, encoding).unwrap();
        let cmap_subtable = cmap
            .scope
            .offset(usize::try_from(encoding_record.offset).unwrap())
            .read::<CmapSubtable<'_>>()
            .unwrap();
        f(cmap_subtable);
    }

    #[test]
    fn test_mappings_format0() {
        with_cmap_subtable(
            "tests/fonts/opentype/TwitterColorEmoji-SVGinOT.ttf",
            PlatformId::MACINTOSH,
            EncodingId::MACINTOSH_APPLE_ROMAN,
            |cmap_subtable| {
                match cmap_subtable {
                    CmapSubtable::Format0 { .. } => {}
                    _ => {
                        panic!("expected CmapSubtable::Format0");
                    }
                };

                let mappings = cmap_subtable.mappings().unwrap();
                let copyright = cmap_subtable.map_glyph('©' as u32).unwrap().unwrap();
                assert_eq!(mappings[&copyright], '©' as u32);
            },
        );
    }

    #[test]
    fn test_mappings_format4() {
        with_cmap_subtable(
            "tests/fonts/opentype/TwitterColorEmoji-SVGinOT.ttf",
            PlatformId::UNICODE,
            EncodingId(3),
            |cmap_subtable| {
                match cmap_subtable {
                    CmapSubtable::Format4 { .. } => {}
                    _ => {
                        panic!("expected CmapSubtable::Format4");
                    }
                };

                let mappings = cmap_subtable.mappings().unwrap();
                // Format 4 can only represent 16-bit chars (Basic Multilingual Plane)
                let soccer_ball = cmap_subtable.map_glyph('⚽' as u32).unwrap().unwrap();
                let double_exclamation = cmap_subtable.map_glyph('‼' as u32).unwrap().unwrap();
                assert_eq!(mappings[&soccer_ball], '⚽' as u32);
                assert_eq!(mappings[&double_exclamation], '‼' as u32);
            },
        );
    }

    #[test]
    fn test_mappings_format6() {
        with_cmap_subtable(
            "tests/fonts/opentype/Klei.otf",
            PlatformId::MACINTOSH,
            EncodingId::MACINTOSH_APPLE_ROMAN,
            |cmap_subtable| {
                match cmap_subtable {
                    CmapSubtable::Format6 { .. } => {}
                    _ => {
                        panic!("expected CmapSubtable::Format6");
                    }
                };

                let mappings = cmap_subtable.mappings().unwrap();
                let a = cmap_subtable.map_glyph('a' as u32).unwrap().unwrap();
                let caron = cmap_subtable.map_glyph(255).unwrap().unwrap();
                assert_eq!(mappings[&a], 'a' as u32);
                assert_eq!(mappings[&caron], 255);
            },
        );
    }

    #[test]
    fn test_mappings_format12() {
        with_cmap_subtable(
            "tests/fonts/opentype/TwitterColorEmoji-SVGinOT.ttf",
            PlatformId::WINDOWS,
            EncodingId::WINDOWS_UNICODE_UCS4,
            |cmap_subtable| {
                match cmap_subtable {
                    CmapSubtable::Format12 { .. } => {}
                    _ => {
                        panic!("expected CmapSubtable::Format12");
                    }
                };

                let mappings = cmap_subtable.mappings().unwrap();
                // Format 12 uses 32-bit chars so can map all of Unicode
                let dove = cmap_subtable.map_glyph('🕊' as u32).unwrap().unwrap();
                let nerd_face = cmap_subtable.map_glyph('🤓' as u32).unwrap().unwrap();
                assert_eq!(mappings[&dove], '🕊' as u32);
                assert_eq!(mappings[&nerd_face], '🤓' as u32);
            },
        );
    }
}
